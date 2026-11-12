//! Canonical typed intents, portable snapshots, and ordered durable execution.
//!
//! The wire formats are shared with the standalone generated Rust engines. The
//! WAL implementation lives in `vera-rt`, so append recovery and native replay
//! use exactly the same versioned frames and corruption checks.

mod fingerprint;

use std::{collections::BTreeMap, error::Error, fmt, io};

use crate::{
    ast,
    ir::{CheckedProgram, EffectId, TransitionId},
    runtime::{validate_state, EffectRecord, Machine, RuntimeError, State},
    types::{Domain, ScalarType, ValueType},
    value::{self, Value},
};
use num_bigint::{BigInt, Sign};

pub use vera_rt::{checksum, IntentRecord, IoUringWAL as Wal};

#[derive(Debug)]
pub enum PersistenceError {
    InvalidEncoding(String),
    InvalidState(RuntimeError),
    Io(io::Error),
}

impl PersistenceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidEncoding(_) => "ERR_INVALID_ENCODING",
            Self::InvalidState(_) => "ERR_INVALID_STATE",
            Self::Io(_) => "ERR_IO",
        }
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding(message) => write!(f, "invalid VERA encoding: {message}"),
            Self::InvalidState(error) => write!(f, "invalid VERA state: {error}"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidEncoding(_) => None,
            Self::InvalidState(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for PersistenceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<RuntimeError> for PersistenceError {
    fn from(error: RuntimeError) -> Self {
        Self::InvalidState(error)
    }
}

type Result<T> = std::result::Result<T, PersistenceError>;

fn invalid(message: impl Into<String>) -> PersistenceError {
    PersistenceError::InvalidEncoding(message.into())
}

/// A stable identity of the source AST, independent of compiler-resolved IDs.
pub fn program_fingerprint(program: &CheckedProgram) -> [u8; 32] {
    program_fingerprint_from_ast(program.syntax())
}

pub fn program_fingerprint_from_ast(module: &ast::Module) -> [u8; 32] {
    fingerprint::program(module)
}

fn schema_type(program: &CheckedProgram, ty: &ValueType) -> String {
    match ty {
        ValueType::Scalar(ScalarType::Id64(table)) => {
            format!("id64<{}>", program.table(*table).name)
        }
        ValueType::Scalar(ty) => ty.to_string(),
        ValueType::Named(id) => format!("named<{}>", program.type_definition(*id).name),
    }
}

/// The original portable snapshot schema identity, including declaration order.
pub fn schema_fingerprint(program: &CheckedProgram) -> [u8; 32] {
    use fingerprint::Canonical as C;
    let tables = program
        .tables()
        .iter()
        .map(|table| {
            C::Array(vec![
                C::String(table.name.clone()),
                C::Number(table.capacity.to_string()),
                C::Array(
                    table
                        .columns
                        .iter()
                        .map(|column| {
                            C::Array(vec![
                                C::String(column.name.clone()),
                                C::String(schema_type(program, &column.ty)),
                                C::Bool(column.tracked_sum),
                                C::Bool(column.unique),
                            ])
                        })
                        .collect(),
                ),
            ])
        })
        .collect();
    let effects = program
        .effects()
        .iter()
        .map(|effect| {
            C::Array(vec![
                C::String(effect.name.clone()),
                C::Array(
                    effect
                        .params
                        .iter()
                        .map(|param| {
                            C::Array(vec![
                                C::String(param.name.clone()),
                                C::String(schema_type(program, &param.ty)),
                            ])
                        })
                        .collect(),
                ),
            ])
        })
        .collect();
    // Name references plus one reachable definition each avoid expanding a
    // shared nested type DAG once per field occurrence. Sort by name, not ID.
    let mut pending = program
        .tables()
        .iter()
        .flat_map(|t| t.columns.iter().map(|c| c.ty.clone()))
        .chain(
            program
                .effects()
                .iter()
                .flat_map(|e| e.params.iter().map(|p| p.ty.clone())),
        )
        .collect::<Vec<_>>();
    let mut definitions = BTreeMap::new();
    while let Some(ty) = pending.pop() {
        let ValueType::Named(id) = ty else {
            continue;
        };
        let definition = program.type_definition(id);
        if definitions.contains_key(&definition.name) {
            continue;
        }
        let (kind, fields) = match &definition.kind {
            crate::ir::TypeDefinitionKind::Record(fields) => (
                "record",
                fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect::<Vec<_>>(),
            ),
            crate::ir::TypeDefinitionKind::Union(variants) => (
                "union",
                variants
                    .iter()
                    .map(|v| (v.name.clone(), v.payload.clone()))
                    .collect::<Vec<_>>(),
            ),
        };
        let descriptor = C::Array(vec![
            C::String(definition.name.clone()),
            C::String(kind.into()),
            C::Array(
                fields
                    .iter()
                    .map(|(name, ty)| {
                        C::Array(vec![
                            C::String(name.clone()),
                            C::String(schema_type(program, ty)),
                        ])
                    })
                    .collect(),
            ),
        ]);
        pending.extend(fields.into_iter().map(|(_, ty)| ty));
        definitions.insert(definition.name.clone(), descriptor);
    }
    if definitions.is_empty() {
        // Preserve the historical scalar-only snapshot identity byte-for-byte.
        C::object([("effects", C::Array(effects)), ("tables", C::Array(tables))]).digest()
    } else {
        C::object([
            ("effects", C::Array(effects)),
            ("tables", C::Array(tables)),
            ("types", C::Array(definitions.into_values().collect())),
        ])
        .digest()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Intent {
    pub transition: TransitionId,
    pub args: Vec<Value>,
}

impl Intent {
    pub fn from_parameters(
        program: &CheckedProgram,
        name: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Self> {
        let transition = program
            .transition_id(name)
            .ok_or_else(|| invalid(format!("unknown transition {name}")))?;
        let params = &program.transition(transition).params;
        if parameters.len() != params.len() {
            return Err(invalid(
                "transition argument names do not match its parameters",
            ));
        }
        let args = params
            .iter()
            .map(|param| {
                let value = parameters
                    .get(&param.name)
                    .ok_or_else(|| invalid(format!("missing argument {}", param.name)))?;
                value::coerce_value(program, value, &param.ty).map_err(invalid)
            })
            .collect::<Result<_>>()?;
        Ok(Self { transition, args })
    }

    pub fn parameters(&self, program: &CheckedProgram) -> Result<BTreeMap<String, Value>> {
        self.validate(program)?;
        Ok(program
            .transition(self.transition)
            .params
            .iter()
            .zip(&self.args)
            .map(|(param, value)| (param.name.clone(), value.clone()))
            .collect())
    }

    fn validate(&self, program: &CheckedProgram) -> Result<()> {
        let transition = program
            .transitions()
            .get(self.transition.0)
            .ok_or_else(|| invalid("unknown transition tag"))?;
        u32::try_from(self.transition.0).map_err(|_| invalid("transition tag exceeds u32"))?;
        if self.args.len() != transition.params.len() {
            return Err(invalid(
                "transition argument count does not match its parameters",
            ));
        }
        for (arg, param) in self.args.iter().zip(&transition.params) {
            value::coerce_value(program, arg, &param.ty).map_err(invalid)?;
        }
        Ok(())
    }

    /// The transition ordinal is in the WAL header; the payload begins with the
    /// program fingerprint and then contains every typed argument in source order.
    pub fn encode(&self, program: &CheckedProgram) -> Result<(u32, Vec<u8>)> {
        self.validate(program)?;
        let transition = program.transition(self.transition);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&program_fingerprint(program));
        for (arg, param) in self.args.iter().zip(&transition.params) {
            encode_typed(program, &mut bytes, arg, &param.ty)?;
        }
        Ok((self.transition.0 as u32, bytes))
    }

    pub fn decode(program: &CheckedProgram, transition: u32, bytes: &[u8]) -> Result<Self> {
        let transition = usize::try_from(transition)
            .ok()
            .and_then(|index| program.transitions().get(index))
            .ok_or_else(|| invalid("unknown transition tag"))?;
        let mut reader = Reader::new(bytes);
        if reader.take(32)? != program_fingerprint(program) {
            return Err(invalid("intent program fingerprint does not match"));
        }
        let args = transition
            .params
            .iter()
            .map(|param| reader.typed(program, &param.ty))
            .collect::<Result<_>>()?;
        reader.done()?;
        Ok(Self {
            transition: transition.id,
            args,
        })
    }
}

fn encode_typed(
    program: &CheckedProgram,
    bytes: &mut Vec<u8>,
    value: &Value,
    ty: &ValueType,
) -> Result<()> {
    bytes.extend(value::encode_value(program, value, ty).map_err(invalid)?);
    Ok(())
}

fn encode_bitmap(bytes: &mut Vec<u8>, bits: &[bool]) {
    for chunk in bits.chunks(64) {
        let mut word = 0u64;
        for (index, bit) in chunk.iter().enumerate() {
            word |= u64::from(*bit) << index;
        }
        bytes.extend_from_slice(&word.to_le_bytes());
    }
}

fn encode_aggregate(bytes: &mut Vec<u8>, aggregate: &BigInt, ty: &ScalarType) -> Result<()> {
    let domain = Domain::aggregate(ty.clone());
    if !value::in_domain(aggregate, &domain) {
        return Err(invalid("aggregate exceeds its canonical wire domain"));
    }
    bytes.extend(
        value::to_le_bytes(
            &Value::Int(aggregate.clone()),
            usize::from(domain.width / 8),
            domain.signed(),
        )
        .map_err(invalid)?,
    );
    Ok(())
}

/// Validate and encode the complete `VERASNP2` state, including its CRC32.
pub fn encode_state(program: &CheckedProgram, state: &State) -> Result<Vec<u8>> {
    validate_state(program, state)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"VERASNP2");
    bytes.extend_from_slice(&schema_fingerprint(program));
    for (table, data) in program.tables().iter().zip(&state.tables) {
        encode_bitmap(&mut bytes, &data.occupied);
        for generation in &data.generations {
            bytes.extend_from_slice(&generation.to_le_bytes());
        }
        for (index, column) in table.columns.iter().enumerate() {
            for value in &data.columns[index] {
                encode_typed(program, &mut bytes, value, &column.ty)?;
            }
            if column.tracked_sum {
                let aggregate = data.aggregates[index]
                    .as_ref()
                    .ok_or_else(|| invalid("missing tracked aggregate"))?;
                encode_aggregate(
                    &mut bytes,
                    aggregate,
                    column
                        .ty
                        .as_scalar()
                        .ok_or_else(|| invalid("non-scalar aggregate"))?,
                )?;
            }
            if column.unique {
                let index = data.indexes[index]
                    .as_ref()
                    .ok_or_else(|| invalid("missing unique index"))?;
                for key in &index.keys {
                    let key = u64::try_from(*key).map_err(|_| invalid("index key exceeds u64"))?;
                    bytes.extend_from_slice(&key.to_le_bytes());
                }
                encode_bitmap(&mut bytes, &index.used);
            }
        }
    }
    let count = u64::try_from(state.outbox.len()).map_err(|_| invalid("outbox exceeds u64"))?;
    bytes.extend_from_slice(&count.to_le_bytes());
    for record in &state.outbox {
        let effect = program
            .effects()
            .get(record.effect.0)
            .ok_or_else(|| invalid("unknown effect tag"))?;
        let tag = u32::try_from(record.effect.0).map_err(|_| invalid("effect tag exceeds u32"))?;
        bytes.extend_from_slice(&tag.to_le_bytes());
        for (value, param) in record.args.iter().zip(&effect.params) {
            encode_typed(program, &mut bytes, value, &param.ty)?;
        }
    }
    bytes.extend_from_slice(&checksum(&bytes).to_le_bytes());
    Ok(bytes)
}

/// Compute the byte lower bound before allocating table storage from a snapshot.
fn minimum_snapshot_size(program: &CheckedProgram) -> Result<usize> {
    let mut size = 8u64 + 32 + 8 + 4;
    let add = |size: &mut u64, count: u64, width: u64| -> Result<()> {
        *size = count
            .checked_mul(width)
            .and_then(|extra| size.checked_add(extra))
            .ok_or_else(|| invalid("snapshot size exceeds its length domain"))?;
        Ok(())
    };
    for table in program.tables() {
        add(&mut size, table.capacity.div_ceil(64), 8)?;
        add(&mut size, table.capacity, 4)?;
        for (index, column) in table.columns.iter().enumerate() {
            add(
                &mut size,
                table.capacity,
                u64::try_from(program.value_wire_width(&column.ty).map_err(invalid)?)
                    .map_err(|_| invalid("type width exceeds u64"))?,
            )?;
            if column.tracked_sum {
                add(
                    &mut size,
                    1,
                    u64::from(
                        column
                            .ty
                            .as_scalar()
                            .ok_or_else(|| invalid("non-scalar aggregate"))?
                            .aggregate_width()
                            / 8,
                    ),
                )?;
            }
            if column.unique {
                let buckets = table.layout.unique_buckets[index]
                    .ok_or_else(|| invalid("missing unique index layout"))?;
                add(&mut size, buckets, 8)?;
                add(&mut size, buckets.div_ceil(64), 8)?;
            }
        }
    }
    usize::try_from(size).map_err(|_| invalid("snapshot does not fit this platform"))
}

/// Check envelope, scalar domains and all runtime state invariants before admission.
pub fn decode_state(program: &CheckedProgram, bytes: &[u8]) -> Result<State> {
    if bytes.len() < minimum_snapshot_size(program)? {
        return Err(invalid("truncated snapshot"));
    }
    let end = bytes.len() - 4;
    let expected = u32::from_le_bytes(bytes[end..].try_into().expect("four checksum bytes"));
    if checksum(&bytes[..end]) != expected {
        return Err(invalid("snapshot checksum mismatch"));
    }
    let mut reader = Reader::new(&bytes[..end]);
    if reader.take(8)? != b"VERASNP2" || reader.take(32)? != schema_fingerprint(program) {
        return Err(invalid("snapshot header or schema does not match"));
    }
    let mut state = State::empty(program)?;
    for (table, data) in program.tables().iter().zip(&mut state.tables) {
        reader.bitmap(&mut data.occupied)?;
        for generation in &mut data.generations {
            *generation = reader.u32()?;
        }
        for (index, column) in table.columns.iter().enumerate() {
            for value in &mut data.columns[index] {
                *value = reader.typed(program, &column.ty)?;
            }
            if column.tracked_sum {
                data.aggregates[index] = Some(
                    reader.aggregate(
                        column
                            .ty
                            .as_scalar()
                            .ok_or_else(|| invalid("non-scalar aggregate"))?,
                    )?,
                );
            }
            if column.unique {
                let index = data.indexes[index]
                    .as_mut()
                    .ok_or_else(|| invalid("missing unique index storage"))?;
                for key in &mut index.keys {
                    *key = usize::try_from(reader.u64()?)
                        .map_err(|_| invalid("index key exceeds platform domain"))?;
                }
                reader.bitmap(&mut index.used)?;
            }
        }
    }
    let count = usize::try_from(reader.u64()?)
        .map_err(|_| invalid("outbox count exceeds platform domain"))?;
    // Even an effect with no arguments needs a four-byte tag.
    if count > reader.remaining() / 4 {
        return Err(invalid("outbox count exceeds remaining snapshot bytes"));
    }
    state
        .outbox
        .try_reserve(count)
        .map_err(|_| invalid("outbox cannot fit available memory"))?;
    for _ in 0..count {
        let tag = usize::try_from(reader.u32()?).map_err(|_| invalid("invalid effect tag"))?;
        let effect = program
            .effects()
            .get(tag)
            .ok_or_else(|| invalid("unknown effect tag"))?;
        let args = effect
            .params
            .iter()
            .map(|param| reader.typed(program, &param.ty))
            .collect::<Result<_>>()?;
        state.outbox.push(EffectRecord {
            effect: EffectId(tag),
            args,
        });
    }
    reader.done()?;
    validate_state(program, &state)?;
    Ok(state)
}

pub fn snapshot(program: &CheckedProgram, state: &State, path: &str) -> Result<()> {
    let bytes = encode_state(program, state)?;
    vera_rt::atomic_write(path, &bytes)?;
    Ok(())
}

pub fn restore(program: &CheckedProgram, path: &str) -> Result<State> {
    decode_state(program, &std::fs::read(path)?)
}

pub fn append_intent(wal: &mut Wal, program: &CheckedProgram, intent: &Intent) -> Result<()> {
    let (transition, bytes) = intent.encode(program)?;
    wal.append(transition, bytes)?;
    Ok(())
}

/// Verify all frame and typed payload identities before replay mutates any state.
pub fn decode_wal(program: &CheckedProgram, path: &str) -> Result<Vec<Intent>> {
    vera_rt::replay_file(path)?
        .into_iter()
        .map(|record| Intent::decode(program, record.transition, &record.args))
        .collect()
}

/// Replay an entire valid log transactionally. Failed transitions retain their
/// ordered outcomes and leave the staged state unchanged, just as live calls do.
pub fn replay(
    machine: &mut Machine<'_>,
    path: &str,
) -> Result<Vec<std::result::Result<(), RuntimeError>>> {
    let intents = decode_wal(machine.program, path)?;
    let mut staged = Machine::with_state(machine.program, machine.state.clone())?;
    let mut outcomes = Vec::with_capacity(intents.len());
    for intent in intents {
        let parameters = intent.parameters(machine.program)?;
        outcomes.push(staged.call(
            &machine.program.transition(intent.transition).name,
            &parameters,
        ));
    }
    machine.state = staged.state;
    Ok(outcomes)
}

/// Publish a successful state only after its typed intent has been appended and,
/// when requested, synced. An I/O failure never publishes the staged state.
/// Failed transition attempts are logged as ordered intents as well.
pub fn execute_logged(
    machine: &mut Machine<'_>,
    wal: &mut Wal,
    intent: &Intent,
    durable: bool,
) -> Result<std::result::Result<(), RuntimeError>> {
    execute_logged_with_trace(machine, wal, intent, durable, false)
}

/// Logged execution with the same statement and failure trace as a direct
/// `Machine::execute(..., true)` call. Diagnostics become visible only after
/// append and optional synchronization succeed.
pub fn execute_logged_traced(
    machine: &mut Machine<'_>,
    wal: &mut Wal,
    intent: &Intent,
    durable: bool,
) -> Result<std::result::Result<(), RuntimeError>> {
    execute_logged_with_trace(machine, wal, intent, durable, true)
}

fn execute_logged_with_trace(
    machine: &mut Machine<'_>,
    wal: &mut Wal,
    intent: &Intent,
    durable: bool,
    record_trace: bool,
) -> Result<std::result::Result<(), RuntimeError>> {
    let parameters = intent.parameters(machine.program)?;
    let mut staged = Machine::with_state(machine.program, machine.state.clone())?;
    let outcome = staged
        .execute(
            &machine.program.transition(intent.transition).name,
            &parameters,
            record_trace,
        )
        .map(|_| ());
    append_intent(wal, machine.program, intent)?;
    if durable {
        wal.sync()?;
    }
    if outcome.is_ok() {
        machine.state = staged.state;
    }
    machine.last_bindings = staged.last_bindings;
    machine.last_trace = staged.last_trace;
    Ok(outcome)
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid("truncated scalar or envelope"))?;
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four scalar bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight scalar bytes"),
        ))
    }

    fn typed(&mut self, program: &CheckedProgram, ty: &ValueType) -> Result<Value> {
        let width = program.value_wire_width(ty).map_err(invalid)?;
        value::decode_value(program, self.take(width)?, ty).map_err(invalid)
    }

    fn aggregate(&mut self, ty: &ScalarType) -> Result<BigInt> {
        let width = usize::from(ty.aggregate_width() / 8);
        let bytes = self.take(width)?;
        let mut value = BigInt::from_bytes_le(Sign::Plus, bytes);
        if ty.signed() && bytes[width - 1] & 128 != 0 {
            value -= BigInt::from(1u8) << (width * 8);
        }
        Ok(value)
    }

    fn bitmap(&mut self, bits: &mut [bool]) -> Result<()> {
        for chunk in bits.chunks_mut(64) {
            let word = self.u64()?;
            if chunk.len() < 64 && word >> chunk.len() != 0 {
                return Err(invalid("nonzero unused bitmap bits"));
            }
            for (index, bit) in chunk.iter_mut().enumerate() {
                *bit = word >> index & 1 != 0;
            }
        }
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn done(self) -> Result<()> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(invalid("unexpected trailing bytes"))
        }
    }
}
