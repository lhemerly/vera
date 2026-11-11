//! Solver answers are accepted only when they describe the complete semantics.

use num_bigint::BigInt;
use vera_compiler::{
    ir::CheckedProgram,
    parse_source,
    runtime::{validate_state, FailureKind, Machine},
    semantic::analyze,
    value::Value,
    verifier::{verify_all, verify_transition, VerificationResult, VerifierConfig},
};

fn program(source: &str) -> CheckedProgram {
    analyze(parse_source(source).expect("parse source"))
        .unwrap_or_else(|errors| panic!("semantic errors: {errors:?}\n{source}"))
}

fn verify(source: &str, name: &str) -> VerificationResult {
    verify_transition(&program(source), name, &VerifierConfig::default())
}

fn verified(result: &VerificationResult) {
    assert!(result.ok(), "{}", result.summary());
    assert!(
        result.witness.is_some(),
        "non-vacuity requires a concrete committing witness"
    );
}

const UNIQUE: &str = r#"
table Account capacity 8 {
  key id : id64<Account>;
  col balance : dec64 [tracked_sum];
  col acct_no : bytes32 [unique];
}
invariant non_negative { forall k in Account : ge(Account[k].balance, 0); }
invariant bounded { forall k in Account : le(Account[k].balance, 1000000000); }
transition create(balance : dec64, acct_no : bytes32)
  reads [] writes [Account[*].balance, Account[*].acct_no]
  require non_negative : ge(balance, 0);
  require bounded : le(balance, 1000000000);
  body {
    let %id : id64<Account> = table_alloc(Account, { balance : balance, acct_no : acct_no });
    commit;
  }
transition lookup(acct_no : bytes32)
  reads [Account[*].acct_no, Account[*].balance] writes []
  body {
    guard(table_exists_unique(Account, acct_no, acct_no), ERR_MISSING_ROW);
    let %id : id64<Account> = table_lookup(Account, acct_no, acct_no);
    let %bal : dec64 = table_get(Account, %id, balance);
    guard(ge(%bal, 0), ERR_INSUFFICIENT);
    commit;
  }
"#;

#[test]
fn original_unique_lookup_regression_is_native_and_universal() {
    let checked = program(UNIQUE);
    for result in verify_all(&checked, &VerifierConfig::default()) {
        verified(&result);
        for script in [&result.smt_safety, &result.smt_panic, &result.smt_liveness] {
            assert!(script.starts_with("(set-logic QF_AUFBV)"));
            assert!(script.ends_with("(check-sat)\n"));
            let solver = z3::Solver::new();
            solver.from_string(script.as_str());
            assert_ne!(solver.check(), z3::SatResult::Unknown);
        }
        let witness = result.witness.as_ref().unwrap();
        validate_state(&checked, &witness.pre_state).unwrap();
        Machine::with_state(&checked, witness.pre_state.clone())
            .unwrap()
            .call(&result.transition, &witness.params)
            .unwrap();
    }
}

#[test]
fn overflow_counterexample_is_validated_and_replays_the_trap() {
    let source = "transition go(x:u8) reads [] writes [] body {let %y:u8=add(x,1);commit;}";
    let checked = program(source);
    let result = verify_transition(&checked, "go", &VerifierConfig::default());
    assert!(!result.ok());
    assert_eq!(result.safety.status, "unsat", "{}", result.summary());
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    let witness = result.counterexample.unwrap();
    assert!(witness.failure.as_ref().unwrap().span.is_some());
    assert_eq!(witness.params["x"], Value::Int(BigInt::from(255)));
    let error = Machine::with_state(&checked, witness.pre_state)
        .unwrap()
        .call("go", &witness.params)
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::Trap);
    assert_eq!(error.code, "ERR_ARITH_OVERFLOW");
}

#[test]
fn violated_ensure_is_reported_with_a_native_committing_trace() {
    let result = verify(
        "transition go(x:u8) reads [] writes [] ensure wrong:eq(x,0); body {commit;}",
        "go",
    );
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    let counterexample = result.counterexample.unwrap();
    assert_eq!(counterexample.violated, ["ensure wrong"]);
    assert!(counterexample
        .trace
        .iter()
        .any(|step| step.kind == "commit"));
}

#[test]
fn missing_and_stale_handles_are_traps_unless_guarded() {
    let table = "table T capacity 2 {key id:id64<T>;col v:u8;}";
    let unsafe_source = format!("{table} transition go(k:id64<T>) reads [T[k].v] writes [] body {{let %v:u8=table_get(T,k,v);commit;}}");
    let result = verify(&unsafe_source, "go");
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    let safe_source = format!("{table} transition go(k:id64<T>) reads [T[k].v] writes [] require live:table_exists(T,k); body {{let %v:u8=table_get(T,k,v);commit;}}");
    verified(&verify(&safe_source, "go"));
}

#[test]
fn eager_booleans_and_lazy_select_have_distinct_reachability() {
    let table = "table T capacity 1 {key id:id64<T>;col v:u8;}";
    let eager = format!("{table} transition go(k:id64<T>) reads [T[k].v] writes [] body {{guard(and(false,eq(table_get(T,k,v),0)),ERR_STOP);commit;}}");
    assert_eq!(verify(&eager, "go").panic.status, "sat");
    let lazy = format!("{table} transition go(k:id64<T>) reads [T[k].v] writes [] body {{let %v:u8=select(table_exists(T,k),table_get(T,k,v),0);commit;}}");
    verified(&verify(&lazy, "go"));
}

#[test]
fn source_order_requires_do_not_hide_an_earlier_trap() {
    let first = verify("transition go() reads [] writes [] require trap:eq(div(1,0),0); require stop:false; body {commit;}", "go");
    assert_eq!(first.panic.status, "sat", "{}", first.summary());
    assert_eq!(first.liveness.status, "unsat");
    let second = verify("transition go() reads [] writes [] require stop:false; require trap:eq(div(1,0),0); body {commit;}", "go");
    assert_eq!(second.panic.status, "unsat", "{}", second.summary());
    assert_eq!(second.liveness.status, "unsat");
}

#[test]
fn later_allocation_exhaustion_cannot_erase_an_earlier_trap() {
    let source = "table T capacity 1 {key id:id64<T>;col v:u8;} invariant full {table_exists(T,id64<T>(1));} transition go() reads [] writes [T[*].v] body {let %bad:u8=div(1,0);let %id:id64<T> = table_alloc(T,{v:0});commit;}";
    let result = verify(source, "go");
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unsat");
    assert!(result.counterexample.unwrap().pre_state.tables[0].occupied[0]);
}

#[test]
fn aggregate_narrowing_at_let_store_effect_and_fold_boundaries_is_checked() {
    let table = "table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum];}";
    let bodies = [
        "let %v:u8=sum(T.v);",
        "table_set(T,k,v,sum(T.v));",
        "let %id:id64<T> = table_alloc(T,{v:sum(T.v)});",
        "emit E(sum(T.v));",
        "let %v:u8=fold_bounded<1>(a:u8=sum(T.v),i in [0..0]) {yield a;};",
        "let %v:u8=fold_bounded<1>(a:u8=0,i in [0..0]) {yield sum(T.v);};",
    ];
    for body in bodies {
        let source = format!("{table} effect E(x:u8);transition go(k:id64<T>) reads [T[*].v] writes [T[*].v] require live:table_exists(T,k);body {{{body}commit;}}");
        let result = verify(&source, "go");
        assert_eq!(result.panic.status, "sat", "{body}\n{}", result.summary());
    }
}

#[test]
fn decimal_exactness_and_signed_minimum_division_are_checked() {
    verified(&verify("transition go(x:dec64) reads [] writes [] require x_is_one:eq(x,1.0);body {let %p:dec64=mul(x,2.0);let %q:dec64=div(%p,2.0);commit;}", "go"));
    for body in [
        "let %x:dec64=mul(0.00000001,0.00000001);",
        "let %x:dec128=div(1.0,3.0);",
        "let %x:i64=div(-9223372036854775808,-1);",
        "let %x:i64=rem(-9223372036854775808,-1);",
    ] {
        let result = verify(
            &format!("transition go() reads [] writes [] body {{{body}commit;}}"),
            "go",
        );
        assert_eq!(result.panic.status, "sat", "{body}\n{}", result.summary());
        assert_eq!(result.liveness.status, "unsat");
    }
}

#[test]
fn folds_stop_at_the_index_maximum_without_wrapping() {
    let source = "transition go(lo:u8,hi:u8) reads [] writes [] require low:eq(lo,255);require high:eq(hi,255);body {let %v:u8=fold_bounded<8>(a:u8=0,i in [lo..hi]) {yield add(a,1);};guard(eq(%v,1),ERR_WRONG_ITERATION_COUNT);commit;}";
    verified(&verify(source, "go"));
}

#[test]
fn fold_allocation_and_scoped_mutation_are_verified() {
    let source = "table T capacity 8 {key id:id64<T>;col v:i64;col flag:bool;} transition go() reads [T[*].v] writes [T[*].v,T[*].flag] body {let %v:i64=fold_bounded<4>(a:i64=0,i in [0..3]) {let %id:id64<T> = table_alloc(T,{v:i,flag:false});let %x:i64=table_get(T,%id,v);yield add(a,%x);};commit;}";
    verified(&verify(source, "go"));
}

#[test]
fn exact_aggregates_include_rows_outside_the_transition_footprint() {
    let source = "table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum];} invariant total {le(sum(T.v),10);} transition go(k:id64<T>) reads [T[k].v] writes [T[k].v] require live:table_exists(T,k);body {table_set(T,k,v,10);commit;}";
    let result = verify(source, "go");
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    let witness = result.counterexample.unwrap();
    assert!(witness.violated.iter().any(|law| law == "invariant total"));
    assert_eq!(witness.pre_state.tables[0].occupied, [true, true]);
}

#[test]
fn timeout_and_model_resource_limits_fail_closed() {
    let checked = program("table T capacity 4294967296 {key id:id64<T>;col v:u8;} transition go() reads [] writes [] body {commit;}");
    let result = verify_transition(&checked, "go", &VerifierConfig::default());
    assert!(!result.ok());
    assert!(result
        .diagnostics
        .iter()
        .any(|error| error.code == "D_VERIFY_RESOURCE"));
    let checked = program("transition go() reads [] writes [] body {commit;}");
    let result = verify_transition(
        &checked,
        "go",
        &VerifierConfig {
            timeout_ms: 0,
            ..VerifierConfig::default()
        },
    );
    assert!(!result.ok());
    assert_eq!(result.safety.status, "unknown");
    assert_eq!(result.liveness.status, "unknown");
}

#[test]
fn counterexample_inputs_use_exact_wide_values() {
    let expected = BigInt::from(1_u8) << 200_u32;
    let source = format!(
        "transition go(x:bytes32) reads [] writes [] ensure different:ne(x,0x{});body {{commit;}}",
        expected.to_str_radix(16)
    );
    let result = verify(&source, "go");
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    let witness = result.counterexample.unwrap();
    assert_eq!(witness.params["x"], Value::Int(expected));
}

#[test]
fn shipped_verified_programs_prove_and_the_buggy_transfer_has_a_counterexample() {
    for source in [
        include_str!("../../../examples/counter.vera"),
        include_str!("../../../examples/bank.vera"),
    ] {
        let checked = program(source);
        for result in verify_all(&checked, &VerifierConfig::default()) {
            verified(&result);
        }
    }
    let checked = program(include_str!("../../../examples/bank_buggy.vera"));
    let result = verify_transition(&checked, "transfer", &VerifierConfig::default());
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(result.counterexample.is_some());
}

#[test]
fn widened_values_in_unique_queries_compare_mathematically() {
    let source = "table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum,unique];} transition go() reads [T[*].v] writes [] body {guard(table_exists_unique(T,v,sum(T.v)),ERR_MISSING);let %k:id64<T> = table_lookup(T,v,sum(T.v));commit;}";
    verified(&verify(source, "go"));
}
