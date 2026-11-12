use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_all, verify_transition, VerifierConfig},
};
fn program(source: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(source).unwrap()).unwrap_or_else(|e| panic!("{e:?}\n{source}"))
}
const INC:&str="function inc(x:u8)->u8 require room:lt(x,255);ensure exact:eq(result,add(x,1));body {yield add(x,1);}";
fn accepted(source: &str) {
    let p = program(source);
    for result in verify_all(&p, &VerifierConfig::default()) {
        assert!(result.ok(), "{}", result.summary());
        assert!(result.witness.is_some());
    }
}
#[test]
fn independent_function_proof_and_typed_output_contract_compose() {
    let source=format!("{INC}effect E(value:u8);transition go(x:u8) reads [] writes [] require room:lt(x,255);ensure value:eq(effect_arg(E,0,value),add(x,1));body {{let %y:u8=inc(x);emit E(%y);commit;}}");
    let p = program(&source);
    let results = verify_all(&p, &VerifierConfig::default());
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].target_kind, "function");
    assert_eq!(results[1].target_kind, "transition");
    for r in &results {
        assert!(r.ok(), "{}", r.summary());
    }
    assert_eq!(results[1].contract_dependencies.len(), 1);
    assert_eq!(results[1].contract_dependencies[0].function, "inc");
    assert!(results[1].contract_dependencies[0].ok);
}
#[test]
fn missing_call_precondition_is_a_replayed_trap_not_an_assumption() {
    let p = program(&format!(
        "{INC}transition go(x:u8) reads [] writes [] body {{let %y:u8=inc(x);commit;}}"
    ));
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert!(!r.ok());
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    let ce = r.counterexample.unwrap();
    assert_eq!(ce.failure.unwrap().code, "ERR_FUNCTION_PRECONDITION");
}
#[test]
fn unproved_dependency_never_becomes_an_assumed_axiom() {
    let p=program("function wrong(x:u8)->u8 ensure bad:eq(result,1);body {yield 0;}transition go() reads [] writes [] ensure impossible:eq(wrong(0),1);body {commit;}");
    let results = verify_all(&p, &VerifierConfig::default());
    assert_eq!(results[0].safety.status, "sat");
    assert_eq!(
        results[0].counterexample.as_ref().unwrap().target_kind,
        "function"
    );
    assert!(results[0]
        .counterexample
        .as_ref()
        .unwrap()
        .return_value
        .is_some());
    assert!(results[1]
        .diagnostics
        .iter()
        .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
    assert!(!results[1].ok());
}
#[test]
fn function_totality_nonvacuity_and_result_definedness_are_independent() {
    for source in [
        "function f(x:u8)->u8 body {yield add(x,1);}",
        "function f(x:u8)->u8 require impossible:gt(x,255);body {yield x;}",
        "function f(x:u8)->u8 ensure bad:eq(result,div(x,0));body {yield x;}",
    ] {
        let p = program(source);
        let r = verify_all(&p, &VerifierConfig::default());
        assert_eq!(r.len(), 1);
        assert!(!r[0].ok(), "{source}");
    }
}
#[test]
fn inactive_calls_do_not_constrain_live_caller_paths() {
    accepted(&format!("{INC}transition go() reads [] writes [] ensure harmless:eq(select(false,inc(255),7),7);body {{let %x:u8=select(false,inc(255),7);commit;}}"));
}
#[test]
fn deterministic_congruence_is_preserved_for_weak_summaries() {
    accepted("function identity(x:u8)->u8 ensure weak:ge(result,x);body {yield x;}transition go(x:u8) reads [] writes [] ensure same:eq(identity(x),identity(x));body {commit;}");
}
#[test]
fn spurious_abstract_candidates_are_unknown_not_false_counterexamples_or_proofs() {
    let p=program("function zero(x:u8)->u8 ensure weak:true;body {yield 0;}transition go(x:u8) reads [] writes [] ensure actual:eq(zero(x),0);body {commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert!(!r.ok());
    assert_eq!(r.safety.status, "unknown", "{}", r.summary());
    assert!(r
        .safety
        .reason
        .as_ref()
        .unwrap()
        .contains("contract summary"));
    assert!(r.counterexample.is_none());
}
#[test]
fn forward_and_diamond_dependencies_are_proved_once_in_one_session() {
    let source="function top(x:u8)->u8 ensure exact:eq(result,x);body {yield left(right(x));}function left(x:u8)->u8 ensure exact:eq(result,x);body {yield leaf(x);}function right(x:u8)->u8 ensure exact:eq(result,x);body {yield leaf(x);}function leaf(x:u8)->u8 ensure exact:eq(result,x);body {yield x;}transition go(x:u8) reads [] writes [] ensure same:eq(top(x),x);body {commit;}";
    accepted(source);
    let p = program(source);
    let r = verify_all(&p, &VerifierConfig::default());
    assert_eq!(r.len(), 5);
    assert_eq!(r.iter().filter(|r| r.target_kind == "function").count(), 4);
    assert_eq!(r.last().unwrap().contract_dependencies.len(), 1);
}
#[test]
fn selected_transition_verification_proves_functions_used_only_by_invariants() {
    accepted("function yes(x:u8)->bool ensure exact:eq(result,true);body {yield true;}invariant stable {yes(0);}transition go() reads [] writes [] body {commit;}");
    let p=program("function yes(x:u8)->bool ensure exact:eq(result,true);body {yield true;}invariant stable {yes(0);}transition go() reads [] writes [] body {commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
}
#[test]
fn pure_function_proofs_do_not_allocate_or_encode_unrelated_state() {
    let p=program("table Huge capacity 4294967296 {key id:id64<Huge>;col v:u64;}function id(x:u8)->u8 ensure exact:eq(result,x);body {yield x;}");
    let r = verify_all(&p, &VerifierConfig::default());
    assert!(r[0].ok(), "{}", r[0].summary());
    assert_eq!(r[0].encoding_metrics.as_ref().unwrap().state_cells, 0);
}
#[test]
fn caller_encoding_size_does_not_scale_with_callee_body_unrolling() {
    let mut sizes = Vec::new();
    for n in [8, 128] {
        let source=format!("function same(x:u64)->u64 ensure exact:eq(result,x);body {{yield fold_bounded<{n}>(a:u64=x,i in [0..{last}]) {{yield a;}};}}transition go(x:u64) reads [] writes [] ensure same:eq(same(x),x);body {{commit;}}",last=n-1);
        let p = program(&source);
        let r = verify_transition(&p, "go", &VerifierConfig::default());
        assert!(r.ok(), "{}", r.summary());
        sizes.push((
            r.smt_safety.len(),
            r.encoding_metrics.unwrap().expression_steps,
        ));
    }
    assert_eq!(
        sizes[0], sizes[1],
        "caller must consume only summary, not callee body"
    );
}

#[test]
fn proof_evidence_keeps_direct_dag_edges_not_quadratic_transitive_copies() {
    let mut source =
        String::from("function f0(x:u8)->u8 ensure exact:eq(result,x);body {yield x;}");
    for i in 1..64 {
        source.push_str(&format!(
            "function f{i}(x:u8)->u8 ensure exact:eq(result,x);body {{yield f{}(x);}}",
            i - 1
        ));
    }
    let p = program(&source);
    let results = verify_all(&p, &VerifierConfig::default());
    assert_eq!(results.len(), 64);
    assert!(results.iter().all(|r| r.ok()));
    assert_eq!(
        results
            .iter()
            .map(|r| r.contract_dependencies.len())
            .sum::<usize>(),
        63
    );
}
