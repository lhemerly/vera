//! Ordered, optimistic execution of typed intents over owned state snapshots.
//!
//! Workers read one immutable pre-state and execute on private copies. Publication
//! is always in input order. A candidate is reused only while the complete state
//! still equals its pre-state; otherwise the intent executes against the latest
//! committed state. This includes effects, generations, indexes and aggregates.

use std::{
    mem::size_of,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

use crate::{
    ir::CheckedProgram,
    persistence::Intent,
    runtime::{validate_state, FailureKind, Machine, RuntimeError, State},
    value::Value,
};

/// Additional memory available for state snapshots, excluding caller storage.
pub const DEFAULT_MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_WORKERS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchConfig {
    /// One disables speculation. Larger requests are capped at 32 workers and
    /// reduced further when the snapshot budget requires it.
    pub workers: usize,
    pub max_snapshot_bytes: u64,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            workers: thread::available_parallelism()
                .map_or(1, |count| count.get())
                .min(MAX_WORKERS),
            max_snapshot_bytes: DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BatchStats {
    pub speculative_executions: usize,
    pub reused_candidates: usize,
    pub reexecutions: usize,
    pub serial_executions: usize,
    pub committed: usize,
    pub rejected: usize,
    pub worker_failures: usize,
    pub peak_workers: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BatchReport {
    pub results: Vec<Result<(), RuntimeError>>,
    pub stats: BatchStats,
}

fn resource(code: &str) -> RuntimeError {
    RuntimeError::new(code, FailureKind::Resource)
}

/// Execute typed intents in input order with bounded optimistic speculation.
/// Initial-state and setup failures leave the entire state unchanged. Language
/// failures reject only their own intent, exactly as consecutive Machine calls.
pub fn run_batch(
    program: &CheckedProgram,
    state: &mut State,
    intents: &[Intent],
) -> Result<Vec<Result<(), RuntimeError>>, RuntimeError> {
    run_batch_with_config(program, state, intents, BatchConfig::default())
        .map(|report| report.results)
}

/// The deterministic reference path, with no worker threads or stale candidates.
pub fn run_batch_serial(
    program: &CheckedProgram,
    state: &mut State,
    intents: &[Intent],
) -> Result<Vec<Result<(), RuntimeError>>, RuntimeError> {
    run_batch_with_config(
        program,
        state,
        intents,
        BatchConfig {
            workers: 1,
            ..BatchConfig::default()
        },
    )
    .map(|report| report.results)
}

/// Execute with an explicit worker and snapshot-memory budget. An exhausted
/// budget rejects the remaining suffix without discarding committed prefixes.
/// A thread creation failure falls back to serial execution at ordered commit.
pub fn run_batch_with_config(
    program: &CheckedProgram,
    state: &mut State,
    intents: &[Intent],
    config: BatchConfig,
) -> Result<BatchReport, RuntimeError> {
    if config.workers == 0 {
        return Err(RuntimeError::new("ERR_BATCH_CONFIG", FailureKind::Input));
    }
    if intents.is_empty() {
        return Ok(BatchReport::default());
    }
    // Check the admission budget before validation, whose invariant evaluator
    // also owns a staged snapshot. Setup failures precede every publication.
    if snapshot_bytes(state)
        .checked_mul(2)
        .is_none_or(|bytes| bytes > config.max_snapshot_bytes)
    {
        return Err(resource("ERR_BATCH_STATE_LIMIT"));
    }
    catch_unwind(AssertUnwindSafe(|| validate_state(program, state)))
        .map_err(|_| resource("ERR_BATCH_PANIC"))??;
    let mut report = BatchReport::default();
    report
        .results
        .try_reserve_exact(intents.len())
        .map_err(|_| resource("ERR_BATCH_ALLOCATION"))?;
    let mut offset = 0;
    while offset < intents.len() {
        let bytes = snapshot_bytes(state);
        if bytes
            .checked_mul(2)
            .is_none_or(|bytes| bytes > config.max_snapshot_bytes)
        {
            let error = resource("ERR_BATCH_STATE_LIMIT");
            for _ in offset..intents.len() {
                record(&mut report, Err(error.clone()));
            }
            break;
        }
        // A parallel chunk retains one pre-state plus at most two copies per
        // worker (the Machine's pre-state and its private staged transaction).
        let affordable = (config.max_snapshot_bytes / bytes.max(1)).saturating_sub(1) / 2;
        let workers = config
            .workers
            .min(MAX_WORKERS)
            .min(usize::try_from(affordable).unwrap_or(usize::MAX))
            .min(intents.len() - offset);
        if workers < 2 {
            report.stats.serial_executions += 1;
            report.stats.peak_workers = report.stats.peak_workers.max(1);
            let candidate = attempt(program, state, &intents[offset], config.max_snapshot_bytes);
            publish(state, candidate, &mut report);
            offset += 1;
            continue;
        }
        let base = match catch_unwind(AssertUnwindSafe(|| state.clone())) {
            Ok(base) => base,
            Err(_) => {
                report.stats.worker_failures += 1;
                for _ in offset..intents.len() {
                    record(&mut report, Err(resource("ERR_BATCH_PANIC")));
                }
                break;
            }
        };
        let chunk = &intents[offset..offset + workers];
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let (candidates, started) = thread::scope(|scope| {
            let mut handles = Vec::new();
            if handles.try_reserve_exact(workers).is_err() {
                return (None, 0);
            }
            let mut started = 0;
            for intent in chunk {
                let pre = &base;
                let active = &active;
                let peak = &peak;
                handles.push(
                    thread::Builder::new()
                        .spawn_scoped(scope, move || {
                            let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                            peak.fetch_max(current, Ordering::Relaxed);
                            let candidate =
                                attempt(program, pre, intent, config.max_snapshot_bytes);
                            active.fetch_sub(1, Ordering::Relaxed);
                            candidate
                        })
                        .ok(),
                );
                started += usize::from(handles.last().is_some_and(Option::is_some));
            }
            let mut candidates = Vec::new();
            if candidates.try_reserve_exact(workers).is_err() {
                // Join every started worker even when retaining its result is
                // unavailable; no staged result has reached the caller.
                for handle in handles.into_iter().flatten() {
                    let _ = handle.join();
                }
                return (None, started);
            }
            for handle in handles {
                candidates.push(
                    handle.map(|handle| handle.join().unwrap_or_else(|_| Candidate::panicked())),
                );
            }
            (Some(candidates), started)
        });
        report.stats.peak_workers = report.stats.peak_workers.max(peak.load(Ordering::Relaxed));
        report.stats.speculative_executions += started;
        report.stats.worker_failures += workers - started;
        let Some(candidates) = candidates else {
            for intent in chunk {
                report.stats.serial_executions += 1;
                let candidate = attempt(program, state, intent, config.max_snapshot_bytes);
                publish(state, candidate, &mut report);
            }
            offset += workers;
            continue;
        };
        for (intent, candidate) in chunk.iter().zip(candidates) {
            let candidate = match candidate {
                Some(candidate) => {
                    if *state == base {
                        report.stats.reused_candidates += 1;
                        candidate
                    } else {
                        if candidate
                            .result
                            .as_ref()
                            .is_err_and(|error| error.code == "ERR_BATCH_PANIC")
                        {
                            report.stats.worker_failures += 1;
                        }
                        report.stats.reexecutions += 1;
                        report.stats.serial_executions += 1;
                        attempt(program, state, intent, config.max_snapshot_bytes)
                    }
                }
                None => {
                    report.stats.serial_executions += 1;
                    attempt(program, state, intent, config.max_snapshot_bytes)
                }
            };
            publish(state, candidate, &mut report);
        }
        offset += workers;
    }
    Ok(report)
}

struct Candidate {
    state: Option<State>,
    result: Result<(), RuntimeError>,
}

impl Candidate {
    fn panicked() -> Self {
        Self {
            state: None,
            result: Err(resource("ERR_BATCH_PANIC")),
        }
    }
}

fn attempt(
    program: &CheckedProgram,
    state: &State,
    intent: &Intent,
    max_snapshot_bytes: u64,
) -> Candidate {
    if snapshot_bytes(state)
        .checked_mul(2)
        .is_none_or(|bytes| bytes > max_snapshot_bytes)
    {
        return Candidate {
            state: None,
            result: Err(resource("ERR_BATCH_STATE_LIMIT")),
        };
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let transition = program
            .transitions()
            .get(intent.transition.0)
            .ok_or_else(|| RuntimeError::new("ERR_UNKNOWN_TRANSITION", FailureKind::Input))?;
        let parameters = intent
            .parameters(program)
            .map_err(|_| RuntimeError::new("ERR_INVALID_PARAMS", FailureKind::Input))?;
        // Admission is performed once before the batch. Machine execution itself
        // intentionally does not assert post-laws, matching ordinary call().
        let mut machine = Machine {
            program,
            state: state.clone(),
            last_bindings: Default::default(),
            last_trace: Vec::new(),
        };
        machine.call(&transition.name, &parameters)?;
        Ok::<State, RuntimeError>(machine.state)
    })) {
        Ok(Ok(state)) => Candidate {
            state: Some(state),
            result: Ok(()),
        },
        Ok(Err(error)) => Candidate {
            state: None,
            result: Err(error),
        },
        Err(_) => Candidate::panicked(),
    }
}

fn publish(state: &mut State, candidate: Candidate, report: &mut BatchReport) {
    report.stats.peak_workers = report.stats.peak_workers.max(1);
    if candidate.result.is_ok() {
        // Successful execution always owns a complete candidate state.
        if let Some(staged) = candidate.state {
            *state = staged;
        } else {
            record(report, Err(resource("ERR_BATCH_PANIC")));
            return;
        }
    }
    if candidate
        .result
        .as_ref()
        .is_err_and(|error| error.code == "ERR_BATCH_PANIC")
    {
        report.stats.worker_failures += 1;
    }
    record(report, candidate.result);
}

fn record(report: &mut BatchReport, result: Result<(), RuntimeError>) {
    if result.is_ok() {
        report.stats.committed += 1;
    } else {
        report.stats.rejected += 1;
    }
    report.results.push(result);
}

fn value_bytes(value: &Value) -> u64 {
    size_of::<Value>() as u64
        + match value {
            Value::Bool(_) => 0,
            Value::Bytes(bytes) => bytes.len() as u64,
            Value::Record { fields, .. } => fields.iter().fold(0_u64, |total, value| {
                total.saturating_add(value_bytes(value))
            }),
            Value::Variant { payload, .. } => value_bytes(payload),
            // Include allocator slack and a complete last integer limb.
            Value::Int(value) => value.bits().div_ceil(8).saturating_add(32),
        }
}

/// Conservative live allocation estimate for a freshly cloned native State.
fn snapshot_bytes(state: &State) -> u64 {
    let mut bytes = size_of::<State>() as u64;
    for table in &state.tables {
        bytes = bytes
            .saturating_add(size_of_val(table) as u64)
            .saturating_add(table.occupied.len() as u64)
            .saturating_add((table.generations.len() as u64).saturating_mul(4));
        for column in &table.columns {
            bytes = bytes.saturating_add(size_of_val(column) as u64);
            for value in column {
                bytes = bytes.saturating_add(value_bytes(value));
            }
        }
        for aggregate in &table.aggregates {
            bytes = bytes.saturating_add(size_of_val(aggregate) as u64);
            if let Some(value) = aggregate {
                bytes = bytes.saturating_add(value.bits().div_ceil(8).saturating_add(32));
            }
        }
        for index in &table.indexes {
            bytes = bytes.saturating_add(size_of_val(index) as u64);
            if let Some(index) = index {
                bytes = bytes
                    .saturating_add(
                        (index.keys.len() as u64).saturating_mul(size_of::<usize>() as u64),
                    )
                    .saturating_add(index.used.len() as u64);
            }
        }
    }
    for effect in &state.outbox {
        bytes = bytes.saturating_add(size_of_val(effect) as u64);
        for value in &effect.args {
            bytes = bytes.saturating_add(value_bytes(value));
        }
    }
    bytes.max(1)
}
