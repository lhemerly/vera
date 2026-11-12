//! Independent adversarial checks of the modular proof admission boundary.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_all, verify_transition, VerifierConfig},
};

fn program(source: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(source).expect("parse")).unwrap_or_else(|e| panic!("{e:?}\n{source}"))
}
fn check(source: &str) -> vera_compiler::verifier::VerificationResult {
    verify_transition(&program(source), "go", &VerifierConfig::default())
}
fn good(source: &str) {
    let result = check(source);
    assert!(result.ok(), "{}", result.summary());
}

#[test]
fn unrelated_inconsistent_state_invariant_cannot_prove_false_function_contract() {
    let p = program("invariant impossible {false;} function bad()->u8 ensure wrong:eq(result,1);body {yield 0;} transition go() reads [] writes [] body {let %x:u8=bad();commit;}");
    let results = verify_all(&p, &VerifierConfig::default());
    assert_eq!(results[0].target_kind, "function");
    assert_eq!(results[0].safety.status, "sat", "{}", results[0].summary());
    assert!(!results[1].ok());
    assert!(results[1]
        .diagnostics
        .iter()
        .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
}

#[test]
fn function_proof_cache_never_survives_program_or_contract_change() {
    let valid = "function f()->u8 ensure value:eq(result,7);body {yield 7;} transition go() reads [] writes [] ensure value:eq(f(),7);body {commit;}";
    good(valid);
    let changed = check(&valid.replace("yield 7", "yield 8"));
    assert!(!changed.ok());
    assert!(changed
        .diagnostics
        .iter()
        .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
    good(valid);
}

#[test]
fn uf_congruence_does_not_equate_distinct_arguments_or_distinct_functions() {
    for source in [
        "function id(x:u8)->u8 ensure weak:true;body {yield x;} transition go() reads [] writes [] ensure wrong:eq(id(0),id(1));body {commit;}",
        "function a()->u8 ensure weak:true;body {yield 0;} function b()->u8 ensure weak:true;body {yield 1;} transition go() reads [] writes [] ensure wrong:eq(a(),b());body {commit;}",
    ] {
        let result = check(source);
        assert_eq!(result.safety.status, "sat", "{}", result.summary());
        assert!(result.counterexample.is_some());
    }
}

#[test]
fn summary_preconditions_respect_order_and_do_not_hide_prior_arithmetic() {
    let p = program(
        "function f(x:u8)->u8 require first:eq(div(1,x),1);require later:ne(x,0);body {yield x;}",
    );
    let results = verify_all(&p, &VerifierConfig::default());
    assert_eq!(results[0].panic.status, "sat", "{}", results[0].summary());
    // A false *earlier* requirement suppresses the later expression at runtime.
    good("function f(x:u8)->u8 require first:ne(x,0);require later:eq(div(1,x),1);ensure exact:eq(result,x);body {yield x;} transition go() reads [] writes [] ensure safe:eq(select(false,f(0),9),9);body {commit;}");
}

#[test]
fn nested_calls_restore_caller_and_sibling_parameter_bindings() {
    good("function inner(x:u8)->u8 ensure exact:eq(result,x);body {yield x;} function outer(x:u8)->u8 ensure exact:eq(result,x);body {yield select(eq(inner(9),9),inner(x),0);} transition go(x:u8) reads [] writes [] ensure same:eq(outer(x),x);body {commit;}");
}

#[test]
fn wrong_typed_return_or_bad_dependency_is_not_hidden_by_inactive_caller() {
    let result = check("function bad(x:u8)->u8 body {yield add(x,1);} transition go() reads [] writes [] ensure safe:eq(select(false,bad(255),0),0);body {commit;}");
    assert!(!result.ok());
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
}

#[test]
fn pure_function_contracts_cannot_import_state_or_effects_through_dead_code() {
    for forbidden in [
        "select(false,table_get(T,id64<T>(0),v),0)",
        "select(false,effect_count(),0)",
    ] {
        let source = format!("table T capacity 1 {{key id:id64<T>;col v:u64;}} function f()->u64 body {{yield {forbidden};}}");
        let errors = analyze(parse_source(&source).unwrap()).unwrap_err();
        assert!(errors.iter().any(|d| d.code == "D_FUNCTION_PURITY"));
    }
}

#[test]
fn dependent_requirements_are_checked_at_nested_call_not_assumed() {
    let p = program("function only_zero(x:u8)->u8 require zero:eq(x,0);ensure exact:eq(result,0);body {yield x;} function bad(x:u8)->u8 body {yield only_zero(x);} transition go(x:u8) reads [] writes [] body {let %x:u8=bad(x);commit;}");
    let results = verify_all(&p, &VerifierConfig::default());
    let bad = results.iter().find(|r| r.transition == "bad").unwrap();
    assert_eq!(bad.panic.status, "sat", "{}", bad.summary());
    let go = results.iter().find(|r| r.transition == "go").unwrap();
    assert!(!go.ok());
    assert!(go
        .diagnostics
        .iter()
        .any(|d| d.code == "D_VERIFY_DEPENDENCY"));
}
