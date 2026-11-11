use num_bigint::BigInt;
use vera_compiler::{
    batch::{run_batch, run_batch_serial, run_batch_with_config, BatchConfig},
    ir::{CheckedProgram, TransitionId},
    persistence::Intent,
    runtime::{validate_state, FailureKind, Machine, State},
    value::Value,
};

fn checked(source: &str) -> CheckedProgram {
    vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap()
}

fn int(value: i128) -> Value {
    Value::Int(value.into())
}

fn intent<const N: usize>(
    program: &CheckedProgram,
    name: &str,
    values: [(&str, Value); N],
) -> Intent {
    let parameters = values
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect();
    Intent::from_parameters(program, name, &parameters).unwrap()
}

fn config(workers: usize) -> BatchConfig {
    BatchConfig {
        workers,
        max_snapshot_bytes: 16 * 1024 * 1024,
    }
}

const PROGRAM: &str = r#"
module Batch;
table T capacity 4 {key id:id64<T>;col v:i64 [tracked_sum];col code:u64 [unique];}
effect Changed(k:id64<T>,value:i64);
transition create(value:i64,code:u64,accept:bool) reads [] writes [T[*].v,T[*].code]
 body {let %k:id64<T> =table_alloc(T,{v:value,code:code});emit Changed(%k,value);guard(accept,ERR_REJECT);commit;}
transition put(k:id64<T>,value:i64) reads [] writes [T[k].v]
 require live:table_exists(T,k);
 body {table_set(T,k,v,value);emit Changed(k,value);commit;}
transition copy(src:id64<T>,dst:id64<T>) reads [T[src].v] writes [T[dst].v]
 require a:table_exists(T,src);require b:table_exists(T,dst);
 body {let %v:i64=table_get(T,src,v);table_set(T,dst,v,%v);emit Changed(dst,%v);commit;}
transition rejected(k:id64<T>) reads [] writes [T[k].v]
 require live:table_exists(T,k);
 body {table_set(T,k,v,999);emit Changed(k,999);guard(false,ERR_REJECT);commit;}
"#;

fn initial(program: &CheckedProgram) -> State {
    let mut machine = Machine::new(program).unwrap();
    for (value, code) in [(7, 101), (9, 102)] {
        let create = intent(
            program,
            "create",
            [
                ("value", int(value)),
                ("code", int(code)),
                ("accept", Value::Bool(true)),
            ],
        );
        machine
            .call("create", &create.parameters(program).unwrap())
            .unwrap();
    }
    machine.state
}

#[test]
fn dependent_reads_writes_effects_and_failures_match_consecutive_machine_calls() {
    let program = checked(PROGRAM);
    let intents = vec![
        intent(&program, "put", [("k", int(4)), ("value", int(1))]),
        intent(&program, "copy", [("src", int(4)), ("dst", int(5))]),
        intent(&program, "put", [("k", int(5)), ("value", int(2))]),
        intent(&program, "rejected", [("k", int(4))]),
        intent(&program, "copy", [("src", int(5)), ("dst", int(4))]),
    ];
    let start = initial(&program);
    let mut reference = Machine::with_state(&program, start.clone()).unwrap();
    let expected = intents
        .iter()
        .map(|intent| {
            reference.call(
                &program.transition(intent.transition).name,
                &intent.parameters(&program).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    for workers in [1, 2, 4, 32] {
        let mut state = start.clone();
        let report =
            run_batch_with_config(&program, &mut state, &intents, config(workers)).unwrap();
        assert_eq!(report.results, expected);
        assert_eq!(state, reference.state);
        assert_eq!(report.stats.committed, 4);
        assert_eq!(report.stats.rejected, 1);
        assert!(report.stats.peak_workers <= workers.min(32));
        if workers > 1 {
            assert!(report.stats.reexecutions > 0 || report.stats.worker_failures > 0);
        }
        validate_state(&program, &state).unwrap();
    }
    let mut serial = start;
    assert_eq!(
        run_batch_serial(&program, &mut serial, &intents).unwrap(),
        expected
    );
    assert_eq!(serial, reference.state);
}

#[test]
fn allocation_conflicts_reexecute_and_rollback_every_state_component() {
    let program = checked(PROGRAM);
    let mut state = State::empty(&program).unwrap();
    let create = |value, code, accept| {
        intent(
            &program,
            "create",
            [
                ("value", int(value)),
                ("code", int(code)),
                ("accept", Value::Bool(accept)),
            ],
        )
    };
    let intents = vec![
        create(99, 11, false),
        create(7, 11, true),
        create(8, 11, true),
        create(9, 12, true),
        create(10, 13, true),
        create(11, 14, true),
        create(12, 15, true),
        intent(&program, "rejected", [("k", int(4))]),
    ];
    let report = run_batch_with_config(&program, &mut state, &intents, config(8)).unwrap();
    assert_eq!(
        report.results.iter().map(Result::is_ok).collect::<Vec<_>>(),
        [false, true, false, true, true, true, false, false]
    );
    assert_eq!(report.results[0].as_ref().unwrap_err().code, "ERR_REJECT");
    assert_eq!(
        report.results[2].as_ref().unwrap_err().code,
        "ERR_DUPLICATE_KEY"
    );
    assert_eq!(
        report.results[6].as_ref().unwrap_err().code,
        "ERR_TABLE_FULL"
    );
    assert_eq!(state.tables[0].generations, [1, 1, 1, 1]);
    assert_eq!(
        state.tables[0].columns[0],
        [int(7), int(9), int(10), int(11)]
    );
    assert_eq!(state.tables[0].aggregates[0], Some(BigInt::from(37)));
    assert_eq!(state.outbox.len(), 4);
    for (effect, key) in state.outbox.iter().zip(4..8) {
        assert_eq!(effect.args[0], int(key));
    }
    validate_state(&program, &state).unwrap();
}

#[test]
fn malformed_intents_are_ordered_input_failures_without_partial_execution() {
    let program=checked("module Typed;effect Selected(tag:enum{A,B});transition choose(tag:enum{A,B}) reads [] writes [] body {emit Selected(tag);commit;}");
    let mut state = State::empty(&program).unwrap();
    let id = program.transition_id("choose").unwrap();
    let intents = vec![
        Intent {
            transition: TransitionId(usize::MAX),
            args: vec![],
        },
        Intent {
            transition: id,
            args: vec![],
        },
        Intent {
            transition: id,
            args: vec![Value::Bool(true)],
        },
        Intent {
            transition: id,
            args: vec![int(2)],
        },
        Intent {
            transition: id,
            args: vec![Value::Int(BigInt::from(1u8) << 256usize)],
        },
        intent(&program, "choose", [("tag", int(1))]),
    ];
    let results = run_batch_with_config(&program, &mut state, &intents, config(6))
        .unwrap()
        .results;
    assert_eq!(
        results[0].as_ref().unwrap_err().code,
        "ERR_UNKNOWN_TRANSITION"
    );
    for result in &results[..5] {
        assert_eq!(result.as_ref().unwrap_err().kind, FailureKind::Input);
    }
    assert!(results[5].is_ok());
    assert_eq!(state.outbox.len(), 1);
    assert_eq!(state.outbox[0].args, [int(1)]);
}

#[test]
fn invalid_initial_state_and_configuration_fail_before_any_mutation() {
    let program = checked(PROGRAM);
    let mut state = initial(&program);
    let intents = [intent(&program, "put", [("k", int(4)), ("value", int(99))])];
    let before = state.clone();
    let error = run_batch_with_config(&program, &mut state, &intents, config(0)).unwrap_err();
    assert_eq!(error.kind, FailureKind::Input);
    assert_eq!(state, before);
    state.tables[0].aggregates[0] = None;
    let before = state.clone();
    let error = run_batch(&program, &mut state, &intents).unwrap_err();
    assert_eq!(error.kind, FailureKind::State);
    assert_eq!(state, before);
}

#[test]
fn exhausted_snapshot_budget_rejects_intents_without_allocating_workers() {
    let program = checked(PROGRAM);
    let mut state = initial(&program);
    let before = state.clone();
    let intents = [intent(&program, "put", [("k", int(4)), ("value", int(99))])];
    let error = run_batch_with_config(
        &program,
        &mut state,
        &intents,
        BatchConfig {
            workers: 32,
            max_snapshot_bytes: 0,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "ERR_BATCH_STATE_LIMIT");
    assert_eq!(state, before);
}

#[test]
fn failed_and_unchanging_intents_reuse_matching_prestate_candidates() {
    let program=checked("module Pure;transition check(accept:bool) reads [] writes [] body {guard(accept,ERR_REJECT);commit;}");
    let mut state = State::empty(&program).unwrap();
    let before = state.clone();
    let intents = (0..8)
        .map(|i| intent(&program, "check", [("accept", Value::Bool(i % 2 == 0))]))
        .collect::<Vec<_>>();
    let report = run_batch_with_config(&program, &mut state, &intents, config(8)).unwrap();
    assert_eq!(
        report.results.iter().map(Result::is_ok).collect::<Vec<_>>(),
        [true, false, true, false, true, false, true, false]
    );
    assert_eq!(report.stats.reexecutions, 0);
    assert_eq!(
        report.stats.reused_candidates,
        report.stats.speculative_executions
    );
    assert_eq!(state, before);
}

#[test]
fn mixed_batches_across_multiple_chunks_match_serial_reference() {
    let program = checked(PROGRAM);
    let start = initial(&program);
    let intents = (0..97)
        .map(|i| match i % 4 {
            0 => intent(&program, "put", [("k", int(4)), ("value", int(i))]),
            1 => intent(&program, "copy", [("src", int(4)), ("dst", int(5))]),
            2 => intent(&program, "rejected", [("k", int(5))]),
            _ => intent(&program, "copy", [("src", int(5)), ("dst", int(4))]),
        })
        .collect::<Vec<_>>();
    let mut serial = start.clone();
    let expected = run_batch_serial(&program, &mut serial, &intents).unwrap();
    for workers in [2, 7, 32] {
        let mut state = start.clone();
        let report =
            run_batch_with_config(&program, &mut state, &intents, config(workers)).unwrap();
        assert_eq!(report.results, expected);
        assert_eq!(state, serial);
        assert_eq!(report.results.len(), intents.len());
    }
}

#[test]
fn growing_effects_keep_committed_prefix_and_reject_resource_exhausted_suffix() {
    let program=checked("module Effects;effect Mark(value:u64);transition push(value:u64) reads [] writes [] body {emit Mark(value);commit;}");
    let mut state = State::empty(&program).unwrap();
    let intents = (0..30)
        .map(|i| intent(&program, "push", [("value", int(i))]))
        .collect::<Vec<_>>();
    let report = run_batch_with_config(
        &program,
        &mut state,
        &intents,
        BatchConfig {
            workers: 1,
            max_snapshot_bytes: 1024,
        },
    )
    .unwrap();
    let committed = report.stats.committed;
    assert!(committed > 0 && committed < intents.len());
    assert!(report.results[..committed].iter().all(Result::is_ok));
    assert!(report.results[committed..]
        .iter()
        .all(|result| result.as_ref().unwrap_err().code == "ERR_BATCH_STATE_LIMIT"));
    assert_eq!(state.outbox.len(), committed);
    for (i, effect) in state.outbox.iter().enumerate() {
        assert_eq!(effect.args, [int(i as i128)]);
    }
    validate_state(&program, &state).unwrap();
}

#[test]
fn empty_batch_preserves_state_and_has_no_execution_statistics() {
    let program = checked(PROGRAM);
    let mut state = initial(&program);
    let before = state.clone();
    let report = run_batch_with_config(&program, &mut state, &[], config(4)).unwrap();
    assert!(report.results.is_empty());
    assert_eq!(report.stats, Default::default());
    assert_eq!(state, before);
    assert!(run_batch(&program, &mut state, &[]).unwrap().is_empty());
}

#[test]
fn effects_alone_invalidate_speculation_and_preserve_log_order() {
    let program=checked("module Effects;effect Mark(value:u64);transition push(value:u64) reads [] writes [] body {emit Mark(value);commit;}");
    let mut state = State::empty(&program).unwrap();
    let intents = (0..12)
        .map(|i| intent(&program, "push", [("value", int(i))]))
        .collect::<Vec<_>>();
    let report = run_batch_with_config(&program, &mut state, &intents, config(4)).unwrap();
    assert!(report.results.iter().all(Result::is_ok));
    assert_eq!(state.outbox.len(), intents.len());
    for (i, effect) in state.outbox.iter().enumerate() {
        assert_eq!(effect.args, [int(i as i128)]);
    }
    assert!(report.stats.reexecutions > 0 || report.stats.worker_failures > 0);
}

#[test]
fn snapshot_budget_reduces_speculation_to_serial_without_rejecting_work() {
    let program = checked("module Pure;transition accept() reads [] writes [] body {commit;}");
    let mut state = State::empty(&program).unwrap();
    let intents = (0..20)
        .map(|_| intent(&program, "accept", []))
        .collect::<Vec<_>>();
    let state_bytes = std::mem::size_of::<State>() as u64;
    let report = run_batch_with_config(
        &program,
        &mut state,
        &intents,
        BatchConfig {
            workers: 32,
            max_snapshot_bytes: state_bytes * 2,
        },
    )
    .unwrap();
    assert!(report.results.iter().all(Result::is_ok));
    assert_eq!(report.stats.speculative_executions, 0);
    assert_eq!(report.stats.serial_executions, intents.len());
    assert_eq!(report.stats.peak_workers, 1);
}
