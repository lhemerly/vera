use vera_compiler::{
    analyze,
    ir::CheckedProgram,
    parse_source,
    verifier::{verify_all, verify_operation, verify_transition, VerifierConfig},
};
fn checked(s: &str) -> CheckedProgram {
    analyze(parse_source(s).unwrap()).unwrap()
}
const SOURCE: &str = include_str!("fixtures/components.vera");
#[test]
fn independent_operations_compose_sequential_old_state_and_effect_suffixes() {
    let p = checked(SOURCE);
    let operation = verify_operation(&p, "Counter.inc", &VerifierConfig::default());
    assert!(operation.ok(), "{}", operation.summary());
    let caller = verify_transition(&p, "twice", &VerifierConfig::default());
    assert!(caller.ok(), "{}", caller.summary());
    assert_eq!(caller.contract_dependencies.len(), 1);
    assert_eq!(caller.contract_dependencies[0].function, "Counter.inc");
    assert!(caller.contract_dependencies[0].ok);
}
#[test]
fn missing_operation_requirements_are_replayed_traps() {
    let p = checked(SOURCE);
    let r = verify_transition(&p, "bad", &VerifierConfig::default());
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    assert_eq!(
        r.counterexample.unwrap().failure.unwrap().code,
        "ERR_OPERATION_PRECONDITION"
    );
}
#[test]
fn invalid_operation_summary_is_never_admitted() {
    let p = checked(&SOURCE.replace(
        "ensure count:eq(effect_count(),1);",
        "ensure count:eq(effect_count(),2);",
    ));
    let r = verify_operation(&p, "Counter.inc", &VerifierConfig::default());
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
    assert_eq!(r.counterexample.unwrap().target_kind, "operation");
    let r = verify_transition(&p, "twice", &VerifierConfig::default());
    assert!(r
        .diagnostics
        .iter()
        .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
}
#[test]
fn operation_independence_does_not_assume_unrelated_global_invariants() {
    let source = SOURCE.to_owned() + "invariant external {forall k in T:le(T[k].v,5);}";
    let p = checked(&source);
    let r = verify_operation(&p, "Counter.inc", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
    let r = verify_transition(&p, "twice", &VerifierConfig::default());
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
}
#[test]
fn uncalled_operations_are_included_in_full_verification() {
    let source = SOURCE.split("transition twice").next().unwrap();
    let p = checked(source);
    let r = verify_all(&p, &VerifierConfig::default());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].target_kind, "operation");
    assert!(r[0].ok(), "{}", r[0].summary());
}
#[test]
fn frame_preserves_other_rows_and_tracks_aggregate_exactly() {
    let source = SOURCE
        .replace("col v:u8;", "col v:u8 [tracked_sum];")
        .replace(
            "ensure count:eq(effect_count(),3);",
            "ensure count:eq(effect_count(),3);ensure total:eq(sum(T.v),add(old_sum(T.v),2));",
        );
    let p = checked(&source);
    let r = verify_transition(&p, "twice", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
}
#[test]
fn weak_operation_summary_fails_closed_when_concrete_execution_disagrees() {
    let source = SOURCE.replace(
        "ensure exact:eq(table_get(T,k,v),add(old(T[k].v),1));",
        "ensure weak:true;",
    );
    let p = checked(&source);
    let r = verify_transition(&p, "twice", &VerifierConfig::default());
    assert!(!r.ok());
    assert!(
        r.safety.status == "unknown" || r.panic.status == "unknown",
        "{}",
        r.summary()
    );
}
#[test]
fn second_call_requirements_use_invocation_entry_old_state() {
    let source = SOURCE.replace(
        "require room:lt(table_get(T,k,v),10);",
        "require room:lt(table_get(T,k,v),10);require entry:eq(old(T[k].v),table_get(T,k,v));",
    );
    let p = checked(&source);
    let r = verify_transition(&p, "twice", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
}
#[test]
fn framed_summary_does_not_expand_operation_loop_bodies() {
    let mut sizes = Vec::new();
    for n in [1, 64] {
        let source=SOURCE.replace("body {let %v:u8=table_get(T,k,v);",&format!("body {{let %noise:u64=fold_bounded<{n}>(acc:u64=0,i in [0..{}]){{yield add(acc,1);}};let %v:u8=table_get(T,k,v);",n-1));
        let p = checked(&source);
        let r = verify_transition(&p, "twice", &VerifierConfig::default());
        assert!(r.ok(), "{}", r.summary());
        sizes.push(r.encoding_metrics.unwrap().retained_smt_bytes);
    }
    assert_eq!(sizes[0], sizes[1]);
}

#[test]
fn foreign_component_invariants_and_dependencies_are_not_operation_assumptions() {
    let source = SOURCE.to_owned() + "table Foreign capacity 1 {key id:id64<Foreign>;col v:u8;} function wrong()->u8 ensure false_summary:eq(result,1);body {yield 0;} component Other owns [Foreign] { invariant foreign {eq(wrong(),1);} }";
    let p = checked(&source);
    let r = verify_operation(&p, "Counter.inc", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
    assert!(r.contract_dependencies.is_empty());
}

#[test]
fn partial_declared_read_and_write_frames_are_rejected_before_admission() {
    for (reads, writes, mode) in [
        ("[]", "[T[id64<T>(div(k,0))].v]", "write"),
        ("[T[id64<T>(div(k,0))].v]", "[]", "read"),
    ] {
        let source=format!("table T capacity 2 {{key id:id64<T>;col v:u8;}}component C owns [T] {{operation f(k:u64) reads {reads} writes {writes} effects [] body {{commit;}}}}transition go(k:u64) reads [T[*].v] writes [T[*].v] body {{call C.f(k);commit;}}");
        let p = checked(&source);
        let result = verify_operation(&p, "C.f", &VerifierConfig::default());
        assert_eq!(result.panic.status, "sat", "{}", result.summary());
        assert!(!result.ok());
        let evidence = result.counterexample.unwrap();
        assert!(evidence
            .law
            .contains(&format!("undefined {mode} frame key")));
        assert!(evidence.failure.is_none());
        assert_eq!(evidence.trace[0].kind, "frame_contract");
        let caller = verify_transition(&p, "go", &VerifierConfig::default());
        assert!(caller
            .diagnostics
            .iter()
            .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
    }
}
#[test]
fn requirements_may_guard_compound_frame_key_definedness() {
    let p=checked("table T capacity 2 {key id:id64<T>;col v:u8;}component C owns [T] {operation f(k:u64) reads [T[id64<T>(div(1,k))].v] writes [T[id64<T>(div(1,k))].v] effects [] require nonzero:ne(k,0);body {commit;}}transition go(k:u64) reads [T[*].v] writes [T[*].v] require nonzero:ne(k,0);body {call C.f(k);commit;}");
    for result in verify_all(&p, &VerifierConfig::default()) {
        assert!(result.ok(), "{}", result.summary());
    }
}
