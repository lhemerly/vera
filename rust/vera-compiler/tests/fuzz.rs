use vera_compiler::{
    fuzz::{fuzz_module, FuzzConfig, Xoshiro256},
    parse_source,
    semantic::analyze,
};

#[test]
fn deterministic_rng_matches_shared_reference_sequence() {
    let mut rng = Xoshiro256::new(12345);
    assert_eq!(
        (0..4).map(|_| rng.next_u64()).collect::<Vec<_>>(),
        vec![
            13720838825685603483,
            2398916695208396998,
            17770384849984869256,
            891717726879801395
        ]
    );
}

#[test]
fn no_effect_commits_and_guard_reverts_have_distinct_counters() {
    for (body, committed) in [("commit;", 12), ("guard(false,ERR_STOP);commit;", 0)] {
        let program = analyze(
            parse_source(&format!(
                "transition go() reads [] writes [] body {{{body}}}"
            ))
            .unwrap(),
        )
        .unwrap();
        let result = fuzz_module(
            &program,
            &FuzzConfig {
                steps: 4,
                threads: 3,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(result.ok);
        assert_eq!(result.executed, 12);
        assert_eq!(result.committed, committed);
    }
}

#[test]
fn violation_shrinking_retains_replayable_nonzero_input() {
    let program=analyze(parse_source("effect Seen(x:u8);transition go(x:u8) reads [] writes [] ensure zero:eq(x,0);body {emit Seen(x);commit;}").unwrap()).unwrap();
    let result = fuzz_module(
        &program,
        &FuzzConfig {
            steps: 4,
            threads: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!result.ok);
    let violation = &result.violations[0];
    assert_eq!(violation.law, "ensure zero");
    assert_eq!(violation.trace.len(), 1);
    assert_eq!(violation.trace[0].params["x"].as_u64(), Some(1));
}

#[test]
fn precondition_rejections_are_not_executed_or_reported_as_bugs() {
    let program = analyze(
        parse_source("transition go() reads [] writes [] require no:false;body {commit;}").unwrap(),
    )
    .unwrap();
    let result = fuzz_module(
        &program,
        &FuzzConfig {
            steps: 8,
            threads: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.ok);
    assert_eq!(result.executed, 0);
    assert_eq!(result.committed, 0);
}

#[test]
fn seed_row_limit_preserves_declared_capacity_and_reports_arithmetic_traps() {
    let program=analyze(parse_source("table T capacity 32 {key id:id64<T>;col v:u8;}transition go(x:u8) reads [] writes [] body {let %v:u8=add(x,255);commit;}").unwrap()).unwrap();
    let result = fuzz_module(
        &program,
        &FuzzConfig {
            steps: 4,
            threads: 1,
            seed_rows: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!result.ok);
    let violation = &result.violations[0];
    assert_eq!(violation.law, "panic freedom");
    assert_eq!(violation.initial_state.tables[0].occupied.len(), 32);
    assert_eq!(
        violation.initial_state.tables[0]
            .occupied
            .iter()
            .filter(|v| **v)
            .count(),
        1
    );
}
