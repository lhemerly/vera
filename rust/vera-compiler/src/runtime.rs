//! Native execution of the resolved VERA program.
//!
//! A transaction owns a staged state and an immutable transaction-start state.
//! A successful commit publishes the complete staged value; every failure drops
//! it, including occupancy, generations, indexes, aggregates and effects.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use num_bigint::BigInt;
use num_traits::{One, Zero};
use serde::{Deserialize, Serialize};

use crate::diagnostic::Span;
use crate::ir::{
    BindingId, CheckedProgram, ColumnId, EffectId, ExprId, ExprKind, Fold, Param, Snapshot, Stmt,
    StmtKind, Table, TableId, Transition,
};
use crate::types::{mix64, Domain, ScalarType, MAX_PROBES};
use crate::value::{checked_binary, checked_unary, coerce, low_u64, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FailureKind {
    Input,
    Precondition,
    Guard,
    Trap,
    Resource,
    State,
    Invariant,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeError {
    pub code: String,
    pub kind: FailureKind,
    pub span: Option<Span>,
}

impl RuntimeError {
    pub fn new(code: impl Into<String>, kind: FailureKind) -> Self {
        Self {
            code: code.into(),
            kind,
            span: None,
        }
    }

    fn at(mut self, span: Span) -> Self {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    pub fn handled(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Guard | FailureKind::Precondition | FailureKind::Resource
        )
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(span) = self.span {
            write!(f, "{}:{}:{}", self.code, span.line, span.column)
        } else {
            f.write_str(&self.code)
        }
    }
}
impl std::error::Error for RuntimeError {}

fn invalid_state() -> RuntimeError {
    RuntimeError::new("ERR_INVALID_STATE", FailureKind::State)
}
fn arithmetic() -> RuntimeError {
    RuntimeError::new("ERR_ARITH_OVERFLOW", FailureKind::Trap)
}
fn missing_row() -> RuntimeError {
    RuntimeError::new("ERR_MISSING_ROW", FailureKind::Trap)
}
fn resource(code: &str) -> RuntimeError {
    RuntimeError::new(code, FailureKind::Resource)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniqueIndex {
    pub keys: Vec<usize>,
    pub used: Vec<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableState {
    pub occupied: Vec<bool>,
    pub generations: Vec<u32>,
    pub columns: Vec<Vec<Value>>,
    pub aggregates: Vec<Option<BigInt>>,
    pub indexes: Vec<Option<UniqueIndex>>,
}

impl TableState {
    pub fn slot_of(&self, key: u64) -> usize {
        (key & self.occupied.len().saturating_sub(1) as u64) as usize
    }

    pub fn valid(&self, info: &Table, key: u64) -> bool {
        let bits = info.key_bits() + info.generation_bits();
        if bits < 64 && key >> bits != 0 {
            return false;
        }
        let slot = (key & (info.capacity - 1)) as usize;
        self.occupied.get(slot) == Some(&true)
            && self
                .generations
                .get(slot)
                .is_some_and(|generation| u64::from(*generation) == key >> info.key_bits())
    }

    pub fn key_at(&self, info: &Table, slot: usize) -> Option<u64> {
        if self.occupied.get(slot) != Some(&true) {
            return None;
        }
        Some((u64::from(*self.generations.get(slot)?) << info.key_bits()) | slot as u64)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRecord {
    pub effect: EffectId,
    pub args: Vec<Value>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub tables: Vec<TableState>,
    pub outbox: Vec<EffectRecord>,
}

/// Interpreter storage is more expensive than the generated fixed-width SoA.
/// Reject allocations beyond this host budget explicitly; never shrink tables.
pub const DEFAULT_MAX_STATE_BYTES: u64 = 256 * 1024 * 1024;

pub fn ensure_state_budget(program: &CheckedProgram, max_bytes: u64) -> Result<(), RuntimeError> {
    let mut estimate = 0u128;
    for table in program.tables() {
        let scalar_storage = (std::mem::size_of::<Value>() + 32) as u128;
        estimate += u128::from(table.capacity) * (5 + scalar_storage * table.columns.len() as u128);
        estimate += table.columns.len() as u128 * 128 + 256;
        for buckets in table.layout.unique_buckets.iter().flatten() {
            estimate += u128::from(*buckets) * (std::mem::size_of::<usize>() + 1) as u128;
        }
        if estimate > u128::from(max_bytes) {
            return Err(resource("ERR_STATE_LIMIT"));
        }
    }
    Ok(())
}

fn filled<T: Clone>(length: usize, value: T) -> Result<Vec<T>, RuntimeError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .map_err(|_| resource("ERR_STATE_ALLOCATION"))?;
    values.resize(length, value);
    Ok(values)
}

impl State {
    /// Construct representation-correct empty tables for trusted setup assembly.
    /// Use `Machine::with_state` to admit a completed initial state.
    pub fn empty(program: &CheckedProgram) -> Result<Self, RuntimeError> {
        Self::empty_with_limit(program, DEFAULT_MAX_STATE_BYTES)
    }

    /// Explicit host budget for applications with larger supported memory.
    pub fn empty_with_limit(
        program: &CheckedProgram,
        max_bytes: u64,
    ) -> Result<Self, RuntimeError> {
        ensure_state_budget(program, max_bytes)?;
        let mut tables = Vec::new();
        for table in program.tables() {
            let capacity =
                usize::try_from(table.capacity).map_err(|_| resource("ERR_STATE_ALLOCATION"))?;
            let mut columns = Vec::new();
            let mut aggregates = Vec::new();
            let mut indexes = Vec::new();
            for column in &table.columns {
                columns.push(filled(capacity, Value::zero(&column.ty))?);
                aggregates.push(column.tracked_sum.then(BigInt::zero));
                indexes.push(match table.layout.unique_buckets[column.id.index] {
                    Some(buckets) => {
                        let buckets = usize::try_from(buckets)
                            .map_err(|_| resource("ERR_STATE_ALLOCATION"))?;
                        Some(UniqueIndex {
                            keys: filled(buckets, 0)?,
                            used: filled(buckets, false)?,
                        })
                    }
                    None => None,
                });
            }
            tables.push(TableState {
                occupied: filled(capacity, false)?,
                generations: filled(capacity, 0)?,
                columns,
                aggregates,
                indexes,
            });
        }
        Ok(Self {
            tables,
            outbox: Vec::new(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    pub span: Span,
    pub kind: String,
    pub text: String,
    pub value: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct Execution {
    pub bindings: BTreeMap<BindingId, Value>,
    pub trace: Vec<TraceStep>,
}

pub struct Machine<'p> {
    pub program: &'p CheckedProgram,
    pub state: State,
    pub last_bindings: BTreeMap<BindingId, Value>,
    pub last_trace: Vec<TraceStep>,
}

impl<'p> Machine<'p> {
    pub fn new(program: &'p CheckedProgram) -> Result<Self, RuntimeError> {
        Self::with_state(program, State::empty(program)?)
    }

    pub fn with_state(program: &'p CheckedProgram, state: State) -> Result<Self, RuntimeError> {
        validate_state(program, &state)?;
        Ok(Self {
            program,
            state,
            last_bindings: BTreeMap::new(),
            last_trace: Vec::new(),
        })
    }

    pub fn from_state(program: &'p CheckedProgram, state: State) -> Result<Self, RuntimeError> {
        Self::with_state(program, state)
    }

    pub fn validate_state(&self) -> Result<(), RuntimeError> {
        validate_state(self.program, &self.state)
    }
    pub fn recompute_aggregates(&mut self) -> Result<(), RuntimeError> {
        recompute_aggregates(self.program, &mut self.state)
    }
    pub fn rebuild_indexes(&mut self) -> Result<(), RuntimeError> {
        rebuild_indexes(self.program, &mut self.state)
    }

    pub fn call(&mut self, name: &str, args: &BTreeMap<String, Value>) -> Result<(), RuntimeError> {
        self.execute(name, args, false).map(|_| ())
    }

    /// Execute language semantics without automatically asserting post-laws.
    /// Proof gates, replay and fuzzing evaluate post-laws separately.
    pub fn execute(
        &mut self,
        name: &str,
        args: &BTreeMap<String, Value>,
        record_trace: bool,
    ) -> Result<Execution, RuntimeError> {
        self.last_bindings.clear();
        self.last_trace.clear();
        let transition = self
            .program
            .transition_id(name)
            .map(|id| self.program.transition(id))
            .ok_or_else(|| RuntimeError::new("ERR_UNKNOWN_TRANSITION", FailureKind::Input))?;
        let bindings = bind_params(&transition.params, args)?;
        let mut staged = self.state.clone();
        let (result, bindings, mut trace) = {
            let mut evaluator = Evaluator {
                program: self.program,
                state: &mut staged,
                pre: &self.state,
                bindings,
                trace: Vec::new(),
                record_trace,
            };
            let result = evaluator.transition(transition);
            (result, evaluator.bindings, evaluator.trace)
        };
        if let Err(error) = &result {
            if record_trace {
                trace.push(TraceStep {
                    span: error.span.unwrap_or(transition.span),
                    kind: format!("{:?}", error.kind).to_lowercase(),
                    text: error.code.clone(),
                    value: None,
                });
            }
        }
        self.last_bindings = bindings.clone();
        self.last_trace = trace.clone();
        result?;
        self.state = staged;
        Ok(Execution { bindings, trace })
    }
}

pub fn bind_params(
    params: &[Param],
    args: &BTreeMap<String, Value>,
) -> Result<BTreeMap<BindingId, Value>, RuntimeError> {
    let invalid = || RuntimeError::new("ERR_INVALID_PARAMS", FailureKind::Input);
    if args.len() != params.len() || params.iter().any(|param| !args.contains_key(&param.name)) {
        return Err(invalid());
    }
    params
        .iter()
        .map(|param| {
            let value = coerce(&args[&param.name], &param.ty).map_err(|_| invalid())?;
            Ok((param.binding, value))
        })
        .collect()
}

pub fn bucket_of(value: &Value, buckets: usize) -> usize {
    (mix64(low_u64(value)) & (buckets as u64 - 1)) as usize
}

fn table_state(state: &State, table: TableId) -> Result<&TableState, RuntimeError> {
    state.tables.get(table.0).ok_or_else(invalid_state)
}

fn row_value(state: &State, column: ColumnId, slot: usize) -> Result<Value, RuntimeError> {
    table_state(state, column.table)?
        .columns
        .get(column.index)
        .and_then(|values| values.get(slot))
        .cloned()
        .ok_or_else(invalid_state)
}

pub fn unique_lookup(
    program: &CheckedProgram,
    state: &State,
    column: ColumnId,
    value: &Value,
) -> Result<Option<usize>, RuntimeError> {
    let table = table_state(state, column.table)?;
    let buckets = program.table(column.table).layout.unique_buckets[column.index]
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(invalid_state)?;
    let index = table
        .indexes
        .get(column.index)
        .and_then(Option::as_ref)
        .ok_or_else(invalid_state)?;
    if index.keys.len() != buckets || index.used.len() != buckets {
        return Err(invalid_state());
    }
    let mut position = bucket_of(value, buckets);
    for distance in 0..MAX_PROBES {
        if !index.used[position] {
            return Ok(None);
        }
        let slot = index.keys[position];
        let resident = table
            .columns
            .get(column.index)
            .and_then(|values| values.get(slot))
            .ok_or_else(invalid_state)?;
        if resident == value {
            return Ok((table.occupied.get(slot) == Some(&true)).then_some(slot));
        }
        let home = bucket_of(resident, buckets);
        if (position.wrapping_sub(home) & (buckets - 1)) < distance {
            return Ok(None);
        }
        position = position.wrapping_add(1) & (buckets - 1);
    }
    Ok(None)
}

fn unique_insert(
    program: &CheckedProgram,
    state: &mut State,
    column: ColumnId,
    slot: usize,
) -> Result<(), RuntimeError> {
    let value = row_value(state, column, slot)?;
    if unique_lookup(program, state, column, &value)?.is_some() {
        return Err(resource("ERR_DUPLICATE_KEY"));
    }
    let table = state
        .tables
        .get_mut(column.table.0)
        .ok_or_else(invalid_state)?;
    let values = table.columns.get(column.index).ok_or_else(invalid_state)?;
    let index = table
        .indexes
        .get_mut(column.index)
        .and_then(Option::as_mut)
        .ok_or_else(invalid_state)?;
    let buckets = index.keys.len();
    if buckets == 0 || !buckets.is_power_of_two() || index.used.len() != buckets {
        return Err(invalid_state());
    }
    let mut position = bucket_of(&value, buckets);
    let mut distance = 0usize;
    let mut incoming = slot;
    for _ in 0..MAX_PROBES {
        if !index.used[position] {
            index.used[position] = true;
            index.keys[position] = incoming;
            return Ok(());
        }
        let resident = index.keys[position];
        let home = bucket_of(values.get(resident).ok_or_else(invalid_state)?, buckets);
        let resident_distance = position.wrapping_sub(home) & (buckets - 1);
        if resident_distance < distance {
            index.keys[position] = incoming;
            incoming = resident;
            distance = resident_distance;
        }
        position = position.wrapping_add(1) & (buckets - 1);
        distance += 1;
    }
    Err(resource("ERR_INDEX_SATURATED"))
}

fn validate_rows(program: &CheckedProgram, state: &State) -> Result<(), RuntimeError> {
    if state.tables.len() != program.tables().len() {
        return Err(invalid_state());
    }
    for info in program.tables() {
        let table = table_state(state, info.id)?;
        let capacity = usize::try_from(info.capacity).map_err(|_| invalid_state())?;
        if table.occupied.len() != capacity
            || table.generations.len() != capacity
            || table.columns.len() != info.columns.len()
        {
            return Err(invalid_state());
        }
        for column in &info.columns {
            let values = &table.columns[column.id.index];
            if values.len() != capacity {
                return Err(invalid_state());
            }
            for (slot, value) in values.iter().enumerate() {
                coerce(value, &column.ty).map_err(|_| invalid_state())?;
                if !table.occupied[slot] && value != &Value::zero(&column.ty) {
                    return Err(invalid_state());
                }
            }
        }
    }
    Ok(())
}

pub fn recompute_aggregates(
    program: &CheckedProgram,
    state: &mut State,
) -> Result<(), RuntimeError> {
    validate_rows(program, state)?;
    let mut staged = state.clone();
    for info in program.tables() {
        let table = &mut staged.tables[info.id.0];
        table.aggregates = vec![None; info.columns.len()];
        for column in &info.columns {
            if column.tracked_sum {
                let mut total = BigInt::zero();
                for (slot, value) in table.columns[column.id.index].iter().enumerate() {
                    if table.occupied[slot] {
                        total += value.int().map_err(|_| invalid_state())?;
                    }
                }
                table.aggregates[column.id.index] = Some(total);
            }
        }
    }
    *state = staged;
    Ok(())
}

pub fn rebuild_indexes(program: &CheckedProgram, state: &mut State) -> Result<(), RuntimeError> {
    validate_rows(program, state)?;
    let mut staged = state.clone();
    for info in program.tables() {
        staged.tables[info.id.0].indexes = vec![None; info.columns.len()];
        for column in &info.columns {
            if let Some(buckets) = info.layout.unique_buckets[column.id.index] {
                let buckets = usize::try_from(buckets).map_err(|_| invalid_state())?;
                staged.tables[info.id.0].indexes[column.id.index] = Some(UniqueIndex {
                    keys: filled(buckets, 0)?,
                    used: filled(buckets, false)?,
                });
                for slot in 0..staged.tables[info.id.0].occupied.len() {
                    if staged.tables[info.id.0].occupied[slot] {
                        unique_insert(program, &mut staged, column.id, slot)?;
                    }
                }
            }
        }
    }
    *state = staged;
    Ok(())
}

pub fn validate_state(program: &CheckedProgram, state: &State) -> Result<(), RuntimeError> {
    validate_rows(program, state)?;
    for info in program.tables() {
        let table = &state.tables[info.id.0];
        if table.aggregates.len() != info.columns.len() || table.indexes.len() != info.columns.len()
        {
            return Err(invalid_state());
        }
        for column in &info.columns {
            let aggregate = &table.aggregates[column.id.index];
            if column.tracked_sum {
                let mut exact = BigInt::zero();
                for (slot, value) in table.columns[column.id.index].iter().enumerate() {
                    if table.occupied[slot] {
                        exact += value.int().map_err(|_| invalid_state())?;
                    }
                }
                if aggregate.as_ref() != Some(&exact) {
                    return Err(invalid_state());
                }
            } else if aggregate.is_some() {
                return Err(invalid_state());
            }
            let index = &table.indexes[column.id.index];
            if let Some(buckets) = info.layout.unique_buckets[column.id.index] {
                let buckets = usize::try_from(buckets).map_err(|_| invalid_state())?;
                let index = index.as_ref().ok_or_else(invalid_state)?;
                if index.keys.len() != buckets || index.used.len() != buckets {
                    return Err(invalid_state());
                }
                let mut indexed = BTreeSet::new();
                for bucket in 0..buckets {
                    let slot = index.keys[bucket];
                    if !index.used[bucket] {
                        if slot != 0 {
                            return Err(invalid_state());
                        }
                        continue;
                    }
                    if table.occupied.get(slot) != Some(&true) || !indexed.insert(slot) {
                        return Err(invalid_state());
                    }
                }
                let live: BTreeSet<_> = table
                    .occupied
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, used)| used.then_some(slot))
                    .collect();
                if indexed != live {
                    return Err(invalid_state());
                }
                for slot in live {
                    if unique_lookup(
                        program,
                        state,
                        column.id,
                        &table.columns[column.id.index][slot],
                    )? != Some(slot)
                    {
                        return Err(invalid_state());
                    }
                }
            } else if index.is_some() {
                return Err(invalid_state());
            }
        }
    }
    for record in &state.outbox {
        let effect = program
            .effects()
            .get(record.effect.0)
            .ok_or_else(invalid_state)?;
        if effect.params.len() != record.args.len() {
            return Err(invalid_state());
        }
        for (param, value) in effect.params.iter().zip(&record.args) {
            coerce(value, &param.ty).map_err(|_| invalid_state())?;
        }
    }
    check_invariants(program, state, state)
}

pub fn eval_expression(
    program: &CheckedProgram,
    current: &State,
    pre: &State,
    bindings: &BTreeMap<BindingId, Value>,
    expression: ExprId,
) -> Result<Value, RuntimeError> {
    let mut staged = current.clone();
    Evaluator {
        program,
        state: &mut staged,
        pre,
        bindings: bindings.clone(),
        trace: Vec::new(),
        record_trace: false,
    }
    .value(expression, Snapshot::Current)
}

pub fn check_invariants(
    program: &CheckedProgram,
    current: &State,
    pre: &State,
) -> Result<(), RuntimeError> {
    let mut staged = current.clone();
    let mut evaluator = Evaluator {
        program,
        state: &mut staged,
        pre,
        bindings: BTreeMap::new(),
        trace: Vec::new(),
        record_trace: false,
    };
    for invariant in program.invariants() {
        let error = || {
            RuntimeError::new(
                format!("ERR_INVARIANT_{}", invariant.name),
                FailureKind::Invariant,
            )
            .at(invariant.span)
        };
        if let Some((table, binding)) = invariant.quantifier {
            let info = program.table(table);
            let rows = table_state(current, table)?;
            for slot in 0..rows.occupied.len() {
                if let Some(key) = rows.key_at(info, slot) {
                    evaluator
                        .bindings
                        .insert(binding, Value::Int(BigInt::from(key)));
                    if evaluator.value(invariant.condition, Snapshot::Current).ok()
                        != Some(Value::Bool(true))
                    {
                        return Err(error());
                    }
                }
            }
            evaluator.bindings.remove(&binding);
        } else if evaluator.value(invariant.condition, Snapshot::Current).ok()
            != Some(Value::Bool(true))
        {
            return Err(error());
        }
    }
    Ok(())
}

struct Evaluator<'p, 's> {
    program: &'p CheckedProgram,
    state: &'s mut State,
    pre: &'s State,
    bindings: BTreeMap<BindingId, Value>,
    trace: Vec<TraceStep>,
    record_trace: bool,
}

impl Evaluator<'_, '_> {
    fn log(&mut self, span: Span, kind: &str, text: String, value: Option<Value>) {
        if self.record_trace {
            self.trace.push(TraceStep {
                span,
                kind: kind.into(),
                text,
                value,
            });
        }
    }

    fn snapshot(&self, state: Snapshot) -> &State {
        match state {
            Snapshot::Pre => self.pre,
            Snapshot::Current => self.state,
        }
    }

    fn effective(context: Snapshot, state: Snapshot) -> Snapshot {
        if context == Snapshot::Pre {
            Snapshot::Pre
        } else {
            state
        }
    }

    fn value(&mut self, expression: ExprId, context: Snapshot) -> Result<Value, RuntimeError> {
        let expr = self.program.expr(expression).clone();
        let result = self.value_inner(expr.kind, &expr.domain, context);
        result.map_err(|error| error.at(expr.span))
    }

    fn value_inner(
        &mut self,
        kind: ExprKind,
        domain: &Domain,
        context: Snapshot,
    ) -> Result<Value, RuntimeError> {
        match kind {
            ExprKind::Constant(value) => Ok(value),
            ExprKind::Binding(binding) => self
                .bindings
                .get(&binding)
                .cloned()
                .ok_or_else(|| RuntimeError::new("ERR_UNINITIALIZED", FailureKind::Trap)),
            ExprKind::Unary { op, operand } => {
                let value = self.value(operand, context)?;
                checked_unary(op, &value, domain).map_err(|_| arithmetic())
            }
            ExprKind::Binary { op, left, right } => {
                let a = self.value(left, context)?;
                let b = self.value(right, context)?;
                checked_binary(op, &a, &b, domain).map_err(|_| arithmetic())
            }
            ExprKind::Select { condition, yes, no } => {
                let condition = self
                    .value(condition, context)?
                    .boolean()
                    .map_err(|_| arithmetic())?;
                self.value(if condition { yes } else { no }, context)
            }
            ExprKind::TableGet { column, key, state }
            | ExprKind::SlotRead { column, key, state } => {
                let trapping = matches!(kind, ExprKind::TableGet { .. });
                let state = Self::effective(context, state);
                let key = self.value(key, state)?.as_u64().ok_or_else(arithmetic)?;
                let info = self.program.table(column.table);
                let snapshot = self.snapshot(state);
                if trapping && !table_state(snapshot, column.table)?.valid(info, key) {
                    return Err(missing_row());
                }
                row_value(snapshot, column, (key & (info.capacity - 1)) as usize)
            }
            ExprKind::TableExists { table, key, state } => {
                let state = Self::effective(context, state);
                let key = self.value(key, state)?.as_u64();
                Ok(Value::Bool(key.is_some_and(|key| {
                    self.snapshot(state)
                        .tables
                        .get(table.0)
                        .is_some_and(|rows| rows.valid(self.program.table(table), key))
                })))
            }
            ExprKind::UniqueLookup {
                column,
                value,
                state,
                exists_only,
            } => {
                let state = Self::effective(context, state);
                let value = self.value(value, state)?;
                let found = unique_lookup(self.program, self.snapshot(state), column, &value)?;
                if exists_only {
                    return Ok(Value::Bool(found.is_some()));
                }
                let slot = found.ok_or_else(missing_row)?;
                let table = self.program.table(column.table);
                let key = table_state(self.snapshot(state), column.table)?
                    .key_at(table, slot)
                    .ok_or_else(missing_row)?;
                Ok(Value::Int(BigInt::from(key)))
            }
            ExprKind::Sum { column, state } => {
                let state = Self::effective(context, state);
                let value = table_state(self.snapshot(state), column.table)?
                    .aggregates
                    .get(column.index)
                    .and_then(Option::as_ref)
                    .cloned()
                    .ok_or_else(invalid_state)?;
                Ok(Value::Int(value))
            }
            ExprKind::KeyOf { value, .. } => {
                let value = self.value(value, context)?;
                Ok(Value::Int(BigInt::from(low_u64(&value))))
            }
            ExprKind::Fold(fold) => self.fold(&fold, context),
        }
    }

    fn fold(&mut self, fold: &Fold, context: Snapshot) -> Result<Value, RuntimeError> {
        let accumulator_type = self.program.binding(fold.accumulator).ty.clone();
        let index_type = self.program.binding(fold.index).ty.clone();
        let initial = self.value(fold.init, context)?;
        let mut accumulator = coerce(&initial, &accumulator_type).map_err(|_| arithmetic())?;
        let low = self.value(fold.lo, context)?;
        let low = coerce(&low, &index_type).map_err(|_| arithmetic())?;
        let high = self.value(fold.hi, context)?;
        let high = coerce(&high, &index_type).map_err(|_| arithmetic())?;
        let mut index = low.int().map_err(|_| arithmetic())?.clone();
        let high = high.int().map_err(|_| arithmetic())?.clone();
        let saved = self.bindings.clone();
        let result = (|| {
            for _ in 0..fold.limit {
                if index > high {
                    break;
                }
                self.bindings.clone_from(&saved);
                self.bindings.insert(fold.index, Value::Int(index.clone()));
                self.bindings.insert(fold.accumulator, accumulator.clone());
                for statement in &fold.body {
                    self.statement(statement, context)?;
                }
                let value = self.value(fold.yield_value, context)?;
                accumulator = coerce(&value, &accumulator_type).map_err(|_| arithmetic())?;
                if index >= index_type.max_value() {
                    break;
                }
                index += BigInt::one();
            }
            Ok(accumulator)
        })();
        self.bindings = saved;
        result
    }

    fn transition(&mut self, transition: &Transition) -> Result<(), RuntimeError> {
        for require in &transition.requires {
            let value = self.value(require.value, Snapshot::Pre)?;
            self.log(
                self.program.expr(require.value).span,
                "require",
                format!("require {} => {:?}", require.name, value),
                Some(value.clone()),
            );
            if value != Value::Bool(true) {
                return Err(RuntimeError::new("Precondition", FailureKind::Precondition)
                    .at(self.program.expr(require.value).span));
            }
        }
        for statement in &transition.body {
            self.statement(statement, Snapshot::Current)?;
        }
        Ok(())
    }

    fn statement(&mut self, statement: &Stmt, context: Snapshot) -> Result<(), RuntimeError> {
        self.statement_inner(&statement.kind, context, statement.span)
            .map_err(|error| error.at(statement.span))
    }

    fn statement_inner(
        &mut self,
        statement: &StmtKind,
        context: Snapshot,
        span: Span,
    ) -> Result<(), RuntimeError> {
        match statement {
            StmtKind::Let { binding, value } => {
                let value = self.value(*value, context)?;
                let value =
                    coerce(&value, &self.program.binding(*binding).ty).map_err(|_| arithmetic())?;
                self.bindings.insert(*binding, value.clone());
                self.log(
                    span,
                    "let",
                    format!("let {} = {:?}", self.program.binding(*binding).name, value),
                    Some(value),
                );
            }
            StmtKind::Guard { condition, code } => {
                let condition = self
                    .value(*condition, context)?
                    .boolean()
                    .map_err(|_| arithmetic())?;
                self.log(
                    span,
                    "guard",
                    format!("guard({code}) => {condition}"),
                    Some(Value::Bool(condition)),
                );
                if !condition {
                    return Err(RuntimeError::new(code, FailureKind::Guard));
                }
            }
            StmtKind::TableSet { column, key, value } => {
                let key = self.value(*key, context)?.as_u64().ok_or_else(arithmetic)?;
                let value = self.value(*value, context)?;
                let table = self.program.table(column.table);
                if !table_state(self.state, column.table)?.valid(table, key) {
                    return Err(missing_row());
                }
                let value =
                    coerce(&value, &self.program.column(*column).ty).map_err(|_| arithmetic())?;
                let slot = (key & (table.capacity - 1)) as usize;
                let old = row_value(self.state, *column, slot)?;
                let rows = &mut self.state.tables[column.table.0];
                if self.program.column(*column).tracked_sum {
                    let previous = rows.aggregates[column.index]
                        .as_ref()
                        .ok_or_else(invalid_state)?;
                    let next = previous - old.int().map_err(|_| arithmetic())?
                        + value.int().map_err(|_| arithmetic())?;
                    check_aggregate(&next, &self.program.column(*column).ty, table.capacity)?;
                    rows.aggregates[column.index] = Some(next);
                }
                rows.columns[column.index][slot] = value.clone();
                self.log(
                    span,
                    "write",
                    format!(
                        "table_set {}[{}].{} = {:?}",
                        table.name,
                        key,
                        self.program.column(*column).name,
                        value
                    ),
                    Some(value),
                );
            }
            StmtKind::Allocate {
                table,
                binding,
                fields,
            } => {
                let info = self.program.table(*table);
                let mut values = vec![None; info.columns.len()];
                // Initializers are evaluated and narrowed in source order,
                // before table-full or generation-exhaustion handled reverts.
                for (column, expression) in fields {
                    let value = self.value(*expression, context)?;
                    let value = coerce(&value, &self.program.column(*column).ty)
                        .map_err(|_| arithmetic())?;
                    values[column.index] = Some(value);
                }
                let slot = table_state(self.state, *table)?
                    .occupied
                    .iter()
                    .position(|used| !used)
                    .ok_or_else(|| resource("ERR_TABLE_FULL"))?;
                let generation = self.state.tables[table.0].generations[slot];
                let max = if info.generation_bits() == 32 {
                    u32::MAX
                } else {
                    (1u32 << info.generation_bits()) - 1
                };
                if generation == max {
                    return Err(resource("ERR_GENERATION_EXHAUSTED"));
                }
                self.state.tables[table.0].occupied[slot] = true;
                self.state.tables[table.0].generations[slot] = generation + 1;
                for column in &info.columns {
                    let value = values[column.id.index].take().ok_or_else(invalid_state)?;
                    let rows = &mut self.state.tables[table.0];
                    if column.tracked_sum {
                        let next = rows.aggregates[column.id.index]
                            .as_ref()
                            .ok_or_else(invalid_state)?
                            + value.int().map_err(|_| arithmetic())?;
                        check_aggregate(&next, &column.ty, info.capacity)?;
                        rows.aggregates[column.id.index] = Some(next);
                    }
                    rows.columns[column.id.index][slot] = value;
                    if column.unique {
                        unique_insert(self.program, self.state, column.id, slot)?;
                    }
                }
                let key = (u64::from(generation + 1) << info.key_bits()) | slot as u64;
                let value = Value::Int(BigInt::from(key));
                self.bindings.insert(*binding, value.clone());
                self.log(
                    span,
                    "allocate",
                    format!("table_alloc {} => {key}", info.name),
                    Some(value),
                );
            }
            StmtKind::Emit { effect, args } => {
                let info = self.program.effect(*effect);
                if info.params.len() != args.len() {
                    return Err(invalid_state());
                }
                let mut values = Vec::new();
                for (parameter, expression) in info.params.iter().zip(args) {
                    let value = self.value(*expression, context)?;
                    values.push(coerce(&value, &parameter.ty).map_err(|_| arithmetic())?);
                }
                self.log(
                    span,
                    "effect",
                    format!("emit {}({values:?})", info.name),
                    None,
                );
                self.state.outbox.push(EffectRecord {
                    effect: *effect,
                    args: values,
                });
            }
            StmtKind::Eval(value) => {
                self.value(*value, context)?;
            }
            StmtKind::Commit => self.log(span, "commit", "commit".into(), None),
        }
        Ok(())
    }
}

fn check_aggregate(value: &BigInt, ty: &ScalarType, capacity: u64) -> Result<(), RuntimeError> {
    if value < &(ty.min_value() * capacity) || value > &(ty.max_value() * capacity) {
        Err(arithmetic())
    } else {
        Ok(())
    }
}
