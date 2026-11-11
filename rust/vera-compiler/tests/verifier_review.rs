//! Independent verifier regressions for frame preservation and failure reachability.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_transition, VerificationResult, VerifierConfig},
};
fn check(source: &str) -> VerificationResult {
    let program = analyze(parse_source(source).unwrap()).unwrap();
    verify_transition(&program, "go", &VerifierConfig::default())
}
fn verified(result: VerificationResult) {
    assert!(result.ok(), "{}", result.summary());
    assert!(result.witness.is_some());
}

#[test]
fn aliased_writes_retain_exact_old_reads_and_aggregate_deltas() {
    verified(check(
        r#"table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum];}
transition go(a:id64<T>,b:id64<T>) reads [T[*].v] writes [T[*].v]
require a_live:table_exists(T,a);require b_live:table_exists(T,b);
ensure restored:eq(T[b].v,old(T[b].v));
body {table_set(T,a,v,9);let %before:u8=old(T[b].v);table_set(T,b,v,%before);commit;}
"#,
    ));
}

#[test]
fn local_frame_rechecks_the_final_value_for_every_alias() {
    let result = check(
        r#"table T capacity 2 {key id:id64<T>;col v:u8;}
invariant bounded {forall k in T:le(T[k].v,10);}
transition go(a:id64<T>,b:id64<T>) reads [] writes [T[*].v]
require a_live:table_exists(T,a);require b_live:table_exists(T,b);
body {table_set(T,a,v,4);table_set(T,b,v,11);commit;}
"#,
    );
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(result.counterexample.is_some());
}

#[test]
fn writes_in_another_table_cannot_frame_out_a_dependent_invariant() {
    let result = check(
        r#"table A capacity 1 {key id:id64<A>;col v:u8;}
table B capacity 1 {key id:id64<B>;col owner:id64<A>;}
invariant owner_bound {forall b in B:le(A[table_get(B,b,owner)].v,10);}
transition go(a:id64<A>) reads [] writes [A[a].v]
require live:table_exists(A,a);
body {table_set(A,a,v,11);commit;}
"#,
    );
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(result.counterexample.unwrap().pre_state.tables[1].occupied[0]);
}

#[test]
fn inactive_select_and_fold_writes_do_not_change_the_post_state() {
    verified(check(
        r#"table T capacity 1 {key id:id64<T>;col v:u8 [tracked_sum];}
invariant bounded {forall k in T:le(T[k].v,10);}
transition go(k:id64<T>) reads [T[k].v] writes [T[k].v]
require live:table_exists(T,k);
ensure unchanged:eq(T[k].v,old(T[k].v));
body {let %a:u8=select(true,0,fold_bounded<1>(a:u8=0,i in [0..0]) {table_set(T,k,v,255);yield div(a,0);});
let %b:u8=fold_bounded<1>(a:u8=0,i in [2..1]) {table_set(T,k,v,255);yield div(a,0);};commit;}
"#,
    ));
}

#[test]
fn full_allocation_initializers_trap_before_the_resource_check() {
    let result = check(
        r#"table T capacity 1 {key id:id64<T>;col v:u8;}
invariant full {table_exists(T,id64<T>(1));}
transition go() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:div(1,0)});commit;}
"#,
    );
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unsat", "{}", result.summary());
}

#[test]
fn a_handled_full_allocation_prevents_later_traps() {
    let result = check(
        r#"table T capacity 1 {key id:id64<T>;col v:u8;}
invariant full {table_exists(T,id64<T>(1));}
transition go() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:0});let %v:u8=div(1,0);commit;}
"#,
    );
    assert_eq!(result.panic.status, "unsat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unsat", "{}", result.summary());
}

#[test]
fn a_new_allocation_never_becomes_live_in_an_old_key_subexpression() {
    let result = check(
        r#"table T capacity 1 {key id:id64<T>;col owner:id64<T>;col v:u8;}
transition go() reads [T[*].owner,T[*].v] writes [T[*].owner,T[*].v]
body {let %k:id64<T>=table_alloc(T,{owner:id64<T>(0),v:7});let %v:u8=old(T[table_get(T,%k,owner)].v);commit;}
"#,
    );
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unsat", "{}", result.summary());
}

#[test]
fn signed_minimum_remainder_uses_the_operation_carrier_width() {
    let result = check(
        r#"table T capacity 1 {key id:id64<T>;col v:i64 [tracked_sum];}
transition go() reads [T[*].v] writes [] require zero:eq(sum(T.v),0);
body {let %v:i64=rem(add(sum(T.v),-170141183460469231731687303715884105728),-1);commit;}
"#,
    );
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unsat", "{}", result.summary());
}

#[test]
fn total_physical_reads_see_writes_through_stale_handle_aliases() {
    verified(check(
        r#"table T capacity 1 {key id:id64<T>;col v:u8;}
transition go(k:id64<T>) reads [T[*].v] writes [T[k].v]
require live:table_exists(T,k);
ensure raw_alias:and(eq(T[id64<T>(0)].v,9),eq(T[id64<T>(18446744073709551615)].v,9));
body {table_set(T,k,v,9);commit;}
"#,
    ));
}

#[test]
fn foreign_existence_is_rechecked_when_allocation_changes_occupancy() {
    let result = check(
        r#"table A capacity 1 {key id:id64<A>;col v:u8;}
table B capacity 1 {key id:id64<B>;col owner:id64<A>;}
invariant absent {forall b in B:not(table_exists(A,B[b].owner));}
transition go() reads [] writes [A[*].v] body {let %a:id64<A>=table_alloc(A,{v:0});commit;}
"#,
    );
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(result.counterexample.unwrap().pre_state.tables[1].occupied[0]);
}

#[test]
fn stable_foreign_existence_does_not_depend_on_foreign_column_writes() {
    verified(check(
        r#"table A capacity 1 {key id:id64<A>;col v:u8;}
table B capacity 1 {key id:id64<B>;col owner:id64<A>;}
invariant reference {forall b in B:table_exists(A,B[b].owner);}
transition go(a:id64<A>) reads [] writes [A[a].v]
require live:table_exists(A,a);body {table_set(A,a,v,255);commit;}
"#,
    ));
}

#[test]
fn a_changed_owner_cannot_use_the_stable_foreign_existence_frame() {
    let result = check(
        r#"table A capacity 1 {key id:id64<A>;col v:u8;}
table B capacity 1 {key id:id64<B>;col owner:id64<A>;}
invariant reference {forall b in B:table_exists(A,B[b].owner);}
transition go(b:id64<B>) reads [] writes [B[b].owner]
require live:table_exists(B,b);body {table_set(B,b,owner,id64<A>(2));commit;}
"#,
    );
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(result.counterexample.is_some());
}

#[test]
fn widened_decimal_division_checks_the_later_scalar_boundary() {
    let result = check(
        r#"table T capacity 1 {key id:id64<T>;col v:dec128 [tracked_sum];}
transition go() reads [T[*].v] writes [] require zero:eq(sum(T.v),0);
body {let %v:dec128=div(add(sum(T.v),170141183460469231731.687303715884105727),0.000000000000000001);commit;}
"#,
    );
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unsat", "{}", result.summary());
    verified(check(
        r#"table T capacity 1 {key id:id64<T>;col v:dec128 [tracked_sum];}
transition go() reads [T[*].v] writes [] require zero:eq(sum(T.v),0);
body {let %v:dec128=div(add(sum(T.v),1.0),0.000000000000000001);guard(eq(%v,1000000000000000000.0),ERR_BAD);commit;}
"#,
    ));
}

#[test]
fn rejecting_a_nonphysical_index_model_never_turns_into_a_proof() {
    // All keys hash from their low 64 bits, which are zero. Thirty-two live,
    // distinct bytes32 values therefore exceed the physical 16-probe index.
    // The logical unique abstraction admits exactly one complete row state;
    // replay rejects it, and blocking it must produce UNKNOWN rather than an
    // UNSAT answer for the original liveness query.
    let mut source =
        String::from("table T capacity 32 {key id:id64<T>;col code:bytes32 [unique];}");
    for slot in 0..32u64 {
        let key = 32 + slot;
        let code = u128::from(slot + 1) << 64;
        source.push_str(&format!("invariant row_{slot} {{and(table_exists(T,id64<T>({key})),eq(T[id64<T>({key})].code,{code}));}}"));
    }
    source.push_str("transition go() reads [] writes [] body {commit;}");
    let result = check(&source);
    assert!(!result.ok());
    assert_eq!(result.safety.status, "unsat", "{}", result.summary());
    assert_eq!(result.panic.status, "unsat", "{}", result.summary());
    assert_eq!(result.liveness.status, "unknown", "{}", result.summary());
    assert!(result.witness.is_none());
}
