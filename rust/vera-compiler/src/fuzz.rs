//! Deterministic bounded bug search over the native transaction engine.
use std::collections::BTreeMap;

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use serde::Serialize;

use crate::{
    ir::{CheckedProgram, Transition},
    runtime::{
        check_invariants, eval_expression, rebuild_indexes, recompute_aggregates, FailureKind,
        Machine, RuntimeError, State,
    },
    types::ScalarType,
    value::Value,
};

#[derive(Clone, Debug)]
pub struct FuzzConfig {
    pub steps: usize,
    pub threads: usize,
    pub seed: u64,
    /// Maximum initially seeded rows, never a replacement table capacity.
    pub seed_rows: Option<usize>,
}
impl Default for FuzzConfig {
    fn default() -> Self {
        Self {
            steps: 64,
            threads: 256,
            seed: 12345,
            seed_rows: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub transition: String,
    pub params: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Violation {
    pub thread: usize,
    pub step: usize,
    pub law: String,
    pub initial_state: State,
    pub trace: Vec<Attempt>,
}
#[derive(Clone, Debug, Serialize)]
pub struct FuzzResult {
    pub ok: bool,
    pub steps: usize,
    pub threads: usize,
    pub executed: u64,
    pub committed: u64,
    pub violations: Vec<Violation>,
}

/// xoshiro256** seeded by splitmix64; shared with the compute furnaces.
pub struct Xoshiro256 {
    state: [u64; 4],
}
impl Xoshiro256 {
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut state = [0; 4];
        for word in &mut state {
            x = x.wrapping_add(0x9e3779b97f4a7c15);
            *word = crate::types::mix64(x);
        }
        Self { state }
    }
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.state;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

fn seed(
    program: &CheckedProgram,
    rng: &mut Xoshiro256,
    limit: Option<usize>,
) -> Result<State, RuntimeError> {
    let mut state = State::empty(program)?;
    for table in program.tables() {
        let count = 4
            .min(table.capacity as usize / 4)
            .min(limit.unwrap_or(usize::MAX));
        let data = &mut state.tables[table.id.0];
        for slot in 0..count {
            data.occupied[slot] = true;
            data.generations[slot] = 1;
            for column in &table.columns {
                let number = match &column.ty {
                    ScalarType::Bool => {
                        data.columns[column.id.index][slot] = Value::Bool(false);
                        continue;
                    }
                    ScalarType::Id64(other) => {
                        BigInt::from(1u64 << program.table(*other).key_bits())
                    }
                    ScalarType::Dec64 | ScalarType::Dec128 => BigInt::from(rng.below(900_000_000)),
                    _ => BigInt::from(if column.unique { slot + 1 } else { 0 }),
                };
                data.columns[column.id.index][slot] = Value::Int(number);
            }
        }
    }
    recompute_aggregates(program, &mut state)?;
    rebuild_indexes(program, &mut state)?;
    Ok(state)
}

fn sample(
    program: &CheckedProgram,
    state: &State,
    transition: &Transition,
    rng: &mut Xoshiro256,
) -> BTreeMap<String, Value> {
    transition
        .params
        .iter()
        .map(|param| {
            let value = match &param.ty {
                ScalarType::Bool => Value::Bool(rng.below(2) == 1),
                ScalarType::Id64(id) => {
                    let table = program.table(*id);
                    let data = &state.tables[id.0];
                    let live: Vec<_> = (0..data.occupied.len())
                        .filter_map(|slot| data.key_at(table, slot))
                        .collect();
                    Value::Int(BigInt::from(if live.is_empty() {
                        rng.below(table.capacity)
                    } else {
                        live[rng.below(live.len() as u64) as usize]
                    }))
                }
                ScalarType::Enum(tags) => Value::Int(BigInt::from(rng.below(tags.len() as u64))),
                ScalarType::Bytes32 => {
                    let bytes: Vec<_> = (0..32).map(|_| rng.below(256) as u8).collect();
                    Value::Int(BigInt::from_bytes_le(num_bigint::Sign::Plus, &bytes))
                }
                ty => {
                    let number = match rng.below(16) {
                        0 => ty.min_value(),
                        1 => ty.max_value(),
                        2 => BigInt::from(0),
                        3 => BigInt::from(1),
                        4 if ty.signed() => BigInt::from(-1),
                        _ if ty.is_decimal() => {
                            let raw = BigInt::from(rng.below(2_000_000_000));
                            if rng.below(2) == 0 {
                                -raw
                            } else {
                                raw
                            }
                        }
                        _ if ty.signed() => BigInt::from(rng.next_u64() as i64),
                        _ => BigInt::from(
                            rng.below(ty.max_value().to_u64().unwrap_or(u64::MAX).max(2)),
                        ),
                    };
                    Value::Int(number)
                }
            };
            (param.name.clone(), value)
        })
        .collect()
}

fn attempt(
    machine: &mut Machine<'_>,
    item: &Attempt,
) -> Result<(Option<String>, bool), RuntimeError> {
    let pre = machine.state.clone();
    match machine.execute(&item.transition, &item.params, false) {
        Err(error) if error.kind == FailureKind::Precondition => Err(error),
        Err(error) if error.kind == FailureKind::Trap => Ok((Some("panic freedom".into()), false)),
        Err(_) => Ok((None, false)),
        Ok(execution) => {
            if let Err(error) = check_invariants(machine.program, &machine.state, &pre) {
                return Ok((Some(error.code), true));
            }
            let transition = machine.program.transition(
                machine
                    .program
                    .transition_id(&item.transition)
                    .expect("known transition"),
            );
            for ensure in &transition.ensures {
                if eval_expression(
                    machine.program,
                    &machine.state,
                    &pre,
                    &execution.bindings,
                    ensure.value,
                )
                .ok()
                    != Some(Value::Bool(true))
                {
                    return Ok((Some(format!("ensure {}", ensure.name)), true));
                }
            }
            Ok((None, true))
        }
    }
}

fn reproduces(program: &CheckedProgram, initial: &State, trace: &[Attempt], law: &str) -> bool {
    let Ok(mut machine) = Machine::with_state(program, initial.clone()) else {
        return false;
    };
    trace.iter().any(|item| {
        attempt(&mut machine, item)
            .ok()
            .and_then(|(law, _)| law)
            .as_deref()
            == Some(law)
    })
}

fn shrink(
    program: &CheckedProgram,
    initial: &State,
    mut trace: Vec<Attempt>,
    law: &str,
) -> Vec<Attempt> {
    let mut chunk = trace.len() / 2;
    while chunk > 0 {
        let mut changed = false;
        for start in (0..trace.len()).step_by(chunk) {
            let mut candidate = trace.clone();
            candidate.drain(start..(start + chunk).min(trace.len()));
            if !candidate.is_empty() && reproduces(program, initial, &candidate, law) {
                trace = candidate;
                changed = true;
                break;
            }
        }
        if !changed {
            chunk /= 2;
        }
    }
    for index in 0..trace.len() {
        let names: Vec<_> = trace[index].params.keys().cloned().collect();
        for name in names {
            let transition = program.transition(
                program
                    .transition_id(&trace[index].transition)
                    .expect("known transition"),
            );
            let ty = &transition
                .params
                .iter()
                .find(|p| p.name == name)
                .expect("known parameter")
                .ty;
            if !matches!(
                ty,
                ScalarType::U8
                    | ScalarType::U16
                    | ScalarType::U32
                    | ScalarType::U64
                    | ScalarType::I64
                    | ScalarType::Dec64
                    | ScalarType::Dec128
            ) {
                continue;
            }
            let Value::Int(value) = &trace[index].params[&name] else {
                continue;
            };
            let sign = value.sign();
            let mut lower = BigInt::from(0);
            let mut upper = if sign == num_bigint::Sign::Minus {
                -value
            } else {
                value.clone()
            };
            let mut zero = trace.clone();
            zero[index]
                .params
                .insert(name.clone(), Value::Int(BigInt::from(0)));
            if reproduces(program, initial, &zero, law) {
                trace = zero;
                continue;
            }
            while lower < upper {
                let mid: BigInt = (&lower + &upper) / 2;
                let mut candidate = trace.clone();
                candidate[index].params.insert(
                    name.clone(),
                    Value::Int(if sign == num_bigint::Sign::Minus {
                        -&mid
                    } else {
                        mid.clone()
                    }),
                );
                if reproduces(program, initial, &candidate, law) {
                    trace = candidate;
                    upper = mid;
                } else {
                    lower = mid + 1;
                }
            }
        }
    }
    trace
}

pub fn fuzz_module(
    program: &CheckedProgram,
    config: &FuzzConfig,
) -> Result<FuzzResult, RuntimeError> {
    let mut result = FuzzResult {
        ok: true,
        steps: config.steps,
        threads: config.threads,
        executed: 0,
        committed: 0,
        violations: Vec::new(),
    };
    if program.transitions().is_empty() {
        return Ok(result);
    }
    for thread in 0..config.threads {
        let mut rng = Xoshiro256::new(
            config
                .seed
                .wrapping_mul(1_000_003)
                .wrapping_add(thread as u64),
        );
        let mut machine = seed(program, &mut rng, config.seed_rows)
            .and_then(|state| Machine::with_state(program, state))
            .or_else(|_| Machine::new(program))?;
        let initial = machine.state.clone();
        let mut trace = Vec::new();
        for step in 0..config.steps {
            let transition =
                &program.transitions()[rng.below(program.transitions().len() as u64) as usize];
            let item = Attempt {
                transition: transition.name.clone(),
                params: sample(program, &machine.state, transition, &mut rng),
            };
            let outcome = attempt(&mut machine, &item);
            if outcome
                .as_ref()
                .is_err_and(|e| e.kind == FailureKind::Precondition)
            {
                continue;
            }
            result.executed += 1;
            let (law, committed) = outcome?;
            if committed {
                result.committed += 1;
            }
            trace.push(item);
            if let Some(law) = law {
                result.ok = false;
                let trace = shrink(program, &initial, trace, &law);
                result.violations.push(Violation {
                    thread,
                    step,
                    law,
                    initial_state: initial,
                    trace,
                });
                break;
            }
        }
    }
    Ok(result)
}
