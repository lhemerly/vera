use std::collections::BTreeMap;

use num_bigint::BigInt;
use vera_compiler::ir::{BindingOrigin, CheckedProgram};
use vera_compiler::runtime::{
    check_invariants, recompute_aggregates, validate_state, FailureKind, Machine, State,
};
use vera_compiler::value::Value;

fn checked(source: &str) -> CheckedProgram {
    vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap()
}

fn int(value: i128) -> Value {
    Value::Int(BigInt::from(value))
}

fn args<const N: usize>(values: [(&str, Value); N]) -> BTreeMap<String, Value> {
    values
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect()
}

const ATOMIC: &str = r#"module Atomic;
table T capacity 4 {key id:id64<T>;col v:i64 [tracked_sum];col code:u64 [unique];}
effect Changed(k:id64<T>,v:i64);
transition create(v:i64,code:u64,accept:bool)
reads [] writes [T[*].v,T[*].code]
body {let %k:id64<T>=table_alloc(T,{v:v,code:code});emit Changed(%k,v);guard(accept,ERR_REJECT);commit;}
"#;

#[test]
fn commits_and_reverts_cover_columns_indexes_generations_sums_and_effects() {
    let program = checked(ATOMIC);
    let mut machine = Machine::new(&program).unwrap();
    machine
        .call(
            "create",
            &args([
                ("v", int(7)),
                ("code", int(11)),
                ("accept", Value::Bool(true)),
            ]),
        )
        .unwrap();
    assert_eq!(machine.state.tables[0].columns[0][0], int(7));
    assert_eq!(machine.state.tables[0].generations[0], 1);
    assert_eq!(machine.state.tables[0].aggregates[0], Some(BigInt::from(7)));
    assert_eq!(machine.state.outbox[0].args, vec![int(4), int(7)]);
    let before = machine.state.clone();
    let error = machine
        .execute(
            "create",
            &args([
                ("v", int(9)),
                ("code", int(12)),
                ("accept", Value::Bool(false)),
            ]),
            true,
        )
        .unwrap_err();
    assert_eq!(error.code, "ERR_REJECT");
    assert_eq!(error.kind, FailureKind::Guard);
    assert_eq!(machine.state, before);
    assert!(machine.last_trace.iter().any(|step| step.kind == "effect"));
    let error = machine
        .call(
            "create",
            &args([
                ("v", int(9)),
                ("code", int(11)),
                ("accept", Value::Bool(true)),
            ]),
        )
        .unwrap_err();
    assert_eq!(error.code, "ERR_DUPLICATE_KEY");
    assert_eq!(error.kind, FailureKind::Resource);
    assert_eq!(machine.state, before);
    validate_state(&program, &machine.state).unwrap();
}

#[test]
fn requires_are_ordered_select_is_lazy_and_boolean_operands_are_eager() {
    let program = checked(
        r#"module Evaluation;
table T capacity 1 {key id:id64<T>;col v:i64;}
transition ordered(allowed:bool) reads [] writes []
require allowed:allowed;require unsafe:eq(div(1,0),0);body {commit;}
transition lazy() reads [] writes [] body {let %v:i64=select(true,1,div(1,0));commit;}
transition eager() reads [] writes [] body {let %v:bool=and(false,eq(div(1,0),0));commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    assert_eq!(
        machine
            .call("ordered", &args([("allowed", Value::Bool(false))]))
            .unwrap_err()
            .kind,
        FailureKind::Precondition
    );
    assert_eq!(
        machine
            .call("ordered", &args([("allowed", Value::Bool(true))]))
            .unwrap_err()
            .kind,
        FailureKind::Trap
    );
    machine.call("lazy", &BTreeMap::new()).unwrap();
    assert_eq!(
        machine.call("eager", &BTreeMap::new()).unwrap_err().code,
        "ERR_ARITH_OVERFLOW"
    );
}

#[test]
fn old_values_and_aggregates_are_an_immutable_transaction_start_snapshot() {
    let program = checked(
        r#"module Old;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
transition create() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:7});commit;}
transition change(k:id64<T>) reads [T[*].v] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,9);let %old:i64=old(T[k].v);
guard(eq(%old,7),ERR_OLD);guard(eq(old_sum(T.v),7),ERR_SUM);commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &BTreeMap::new()).unwrap();
    let execution = machine
        .execute("change", &args([("k", int(2))]), true)
        .unwrap();
    let old = program
        .bindings()
        .iter()
        .find(|binding| binding.name == "%old" && binding.origin == BindingOrigin::Local)
        .unwrap();
    assert_eq!(execution.bindings[&old.id], int(7));
    assert_eq!(machine.state.tables[0].columns[0][0], int(9));
    assert_eq!(machine.state.tables[0].aggregates[0], Some(BigInt::from(9)));
}

#[test]
fn allocation_initializers_observe_source_order_and_mutating_folds() {
    let program = checked(
        r#"module FieldOrder;
table T capacity 2 {key id:id64<T>;col v:i64;}
table R capacity 2 {key id:id64<R>;col first:i64;col second:i64;}
transition create() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:0});commit;}
transition go(k:id64<T>) reads [T[k].v] writes [T[k].v,R[*].first,R[*].second]
require live:table_exists(T,k);
body {let %r:id64<R>=table_alloc(R,{
second:fold_bounded<1>(acc:i64=0,i in [0..0]) {table_set(T,k,v,7);yield 0;},
first:table_get(T,k,v)});commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &BTreeMap::new()).unwrap();
    machine.call("go", &args([("k", int(2))])).unwrap();
    assert_eq!(machine.state.tables[0].columns[0][0], int(7));
    assert_eq!(machine.state.tables[1].columns[0][0], int(7));
    assert_eq!(machine.state.tables[1].columns[1][0], int(0));
}

#[test]
fn nested_folds_preserve_scopes_and_inclusive_ranges_do_not_wrap() {
    let program = checked(
        r#"module Folds;
table T capacity 4 {key id:id64<T>;col v:u64;}
transition nested() reads [] writes [T[*].v] body {
let %total:u64=fold_bounded<3>(acc:u64=0,i in [0..2]) {
  let %inner:u64=fold_bounded<2>(inside:u64=0,j in [0..1]) {yield add(inside,1);};
  yield add(acc,%inner);
};let %new:id64<T>=table_alloc(T,{v:%total});commit;}
transition range(lo:u8,hi:u8) reads [] writes [T[*].v] body {
let %total:u64=fold_bounded<4>(acc:u64=0,i in [lo..hi]) {yield add(acc,1);};
let %new:id64<T>=table_alloc(T,{v:%total});commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("nested", &BTreeMap::new()).unwrap();
    machine
        .call("range", &args([("lo", int(255)), ("hi", int(255))]))
        .unwrap();
    machine
        .call("range", &args([("lo", int(5)), ("hi", int(4))]))
        .unwrap();
    assert_eq!(
        machine.state.tables[0].columns[0][..3],
        [int(6), int(1), int(0)]
    );
}

#[test]
fn arithmetic_before_a_full_allocation_remains_a_trap() {
    let program = checked(
        r#"module Full;
table T capacity 1 {key id:id64<T>;col v:i64;}
transition fill() reads [] writes [T[*].v] body {let %new:id64<T>=table_alloc(T,{v:0});commit;}
transition trap() reads [] writes [T[*].v] body {let %q:i64=div(1,0);let %new:id64<T>=table_alloc(T,{v:%q});commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("fill", &BTreeMap::new()).unwrap();
    let before = machine.state.clone();
    assert_eq!(
        machine.call("trap", &BTreeMap::new()).unwrap_err().code,
        "ERR_ARITH_OVERFLOW"
    );
    assert_eq!(
        machine.call("fill", &BTreeMap::new()).unwrap_err().code,
        "ERR_TABLE_FULL"
    );
    assert_eq!(machine.state, before);
}

#[test]
fn capacity_one_generation_exhaustion_reverts_without_wrapping() {
    let program = checked(
        r#"module Generation;
table T capacity 1 {key id:id64<T>;col v:u64;}
transition create() reads [] writes [T[*].v] body {let %new:id64<T>=table_alloc(T,{v:1});commit;}
"#,
    );
    let mut state = State::empty(&program).unwrap();
    state.tables[0].generations[0] = u32::MAX;
    let mut machine = Machine::with_state(&program, state.clone()).unwrap();
    assert_eq!(
        machine.call("create", &BTreeMap::new()).unwrap_err().code,
        "ERR_GENERATION_EXHAUSTED"
    );
    assert_eq!(machine.state, state);
}

#[test]
fn robin_hood_probe_saturation_restores_the_complete_state() {
    let program = checked(
        r#"module Index;
table T capacity 32 {key id:id64<T>;col code:u64 [unique];}
transition create(code:u64) reads [] writes [T[*].code] body {let %new:id64<T>=table_alloc(T,{code:code});commit;}
"#,
    );
    // These distinct values share bucket zero under the language's mix64 hash
    // and its 64-bucket index. The seventeenth exceeds the 16-probe contract.
    let colliding = [
        0, 69, 143, 165, 205, 286, 304, 369, 505, 507, 515, 582, 731, 861, 892, 893, 1014,
    ];
    let mut machine = Machine::new(&program).unwrap();
    for value in &colliding[..16] {
        machine
            .call("create", &args([("code", int(*value))]))
            .unwrap();
    }
    let before = machine.state.clone();
    let error = machine
        .call("create", &args([("code", int(colliding[16]))]))
        .unwrap_err();
    assert_eq!(error.code, "ERR_INDEX_SATURATED");
    assert_eq!(error.kind, FailureKind::Resource);
    assert_eq!(machine.state, before);
    validate_state(&program, &machine.state).unwrap();
}

#[test]
fn external_state_admission_checks_derived_data_domains_and_effects() {
    let program = checked(ATOMIC);
    let mut machine = Machine::new(&program).unwrap();
    machine
        .call(
            "create",
            &args([
                ("v", int(7)),
                ("code", int(11)),
                ("accept", Value::Bool(true)),
            ]),
        )
        .unwrap();
    let valid = machine.state.clone();
    let mut bad_sum = valid.clone();
    bad_sum.tables[0].aggregates[0] = Some(BigInt::from(8));
    assert!(validate_state(&program, &bad_sum).is_err());
    let mut bad_free_value = valid.clone();
    bad_free_value.tables[0].columns[0][1] = int(1);
    assert!(validate_state(&program, &bad_free_value).is_err());
    let mut bad_effect = valid.clone();
    bad_effect.outbox[0].args[1] = Value::Bool(false);
    assert!(validate_state(&program, &bad_effect).is_err());
    let mut bad_index = valid.clone();
    let index = bad_index.tables[0].indexes[1].as_mut().unwrap();
    let used = index.used.iter().position(|used| *used).unwrap();
    index.keys[used] = 1;
    assert!(validate_state(&program, &bad_index).is_err());
    assert_eq!(machine.state, valid);
}

#[test]
fn checked_scalar_parameters_are_validated_before_any_transaction_work() {
    let program = checked(
        r#"module Params;
table T capacity 1 {key id:id64<T>;col v:u8;}
transition create(value:u8) reads [] writes [T[*].v] body {let %new:id64<T>=table_alloc(T,{v:value});commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    let before = machine.state.clone();
    for params in [
        args([("value", int(256))]),
        args([("value", int(-1))]),
        args([("value", Value::Bool(true))]),
        BTreeMap::new(),
    ] {
        assert_eq!(
            machine.call("create", &params).unwrap_err().code,
            "ERR_INVALID_PARAMS"
        );
        assert_eq!(machine.state, before);
    }
}

#[test]
fn unverified_execution_preserves_a_committed_counterexample_for_replay() {
    let program = checked(
        r#"module Witness;
table T capacity 1 {key id:id64<T>;col v:i64 [tracked_sum];}
invariant nonnegative {forall k in T:ge(T[k].v,0);}
transition create() reads [] writes [T[*].v] body {let %new:id64<T>=table_alloc(T,{v:0});commit;}
transition break_it(k:id64<T>) reads [] writes [T[k].v]
require live:table_exists(T,k);body {table_set(T,k,v,-1);commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &BTreeMap::new()).unwrap();
    let before = machine.state.clone();
    machine
        .execute("break_it", &args([("k", int(1))]), true)
        .unwrap();
    assert_eq!(machine.state.tables[0].columns[0][0], int(-1));
    assert_eq!(
        check_invariants(&program, &machine.state, &before)
            .unwrap_err()
            .code,
        "ERR_INVARIANT_nonnegative"
    );
}

#[test]
fn valid_nonempty_state_needs_no_valid_empty_intermediate() {
    let program = checked(
        r#"module Initial;
table T capacity 1 {key id:id64<T>;col v:i64 [tracked_sum];}
invariant total {eq(sum(T.v),7);}
transition noop() reads [] writes [] body {commit;}
"#,
    );
    assert!(Machine::new(&program).is_err());
    let mut state = State::empty(&program).unwrap();
    state.tables[0].occupied[0] = true;
    state.tables[0].generations[0] = 23;
    state.tables[0].columns[0][0] = int(7);
    recompute_aggregates(&program, &mut state).unwrap();
    let mut machine = Machine::with_state(&program, state).unwrap();
    machine.call("noop", &BTreeMap::new()).unwrap();
    assert_eq!(machine.state.tables[0].generations[0], 23);
}

#[test]
fn huge_declared_capacity_reports_host_resource_limit_without_truncating() {
    let program = checked("module Large;table T capacity 4294967296 {key id:id64<T>;col v:u64;}transition noop() reads [] writes [] body {commit;}");
    let error = State::empty(&program).unwrap_err();
    assert_eq!(error.code, "ERR_STATE_LIMIT");
    assert_eq!(error.kind, FailureKind::Resource);
    assert_eq!(program.tables()[0].capacity, 1u64 << 32);
}

#[test]
fn full_width_secondary_keys_remain_distinct_and_new_rows_are_immediately_readable() {
    let program = checked(
        r#"module WideKeys;
table T capacity 4 {key id:id64<T>;col code:bytes32 [unique];col amount:dec128 [tracked_sum];}
effect Found(k:id64<T>,amount:dec128);
transition create(code:bytes32,amount:dec128) reads [T[*].code] writes [T[*].code,T[*].amount]
body {guard(not(table_exists_unique(T,code,code)),ERR_DUPLICATE);
let %new:id64<T>=table_alloc(T,{code:code,amount:amount});
let %found:id64<T>=table_lookup(T,code,code);guard(eq(%new,%found),ERR_INDEX);commit;}
transition lookup(code:bytes32) reads [T[*].code,T[*].amount] writes []
require present:table_exists_unique(T,code,code);
body {let %found:id64<T>=table_lookup(T,code,code);let %amount:dec128=table_get(T,%found,amount);emit Found(%found,%amount);commit;}
transition unchecked(code:bytes32) reads [T[*].code] writes []
body {let %found:id64<T>=table_lookup(T,code,code);commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    let first = int(1);
    let second = Value::Int((BigInt::from(1u8) << 255usize) + 1u8);
    let amount = int(2_000_000_000_000_000_001);
    machine
        .call(
            "create",
            &args([("code", first.clone()), ("amount", amount.clone())]),
        )
        .unwrap();
    machine
        .call(
            "create",
            &args([("code", second.clone()), ("amount", int(3))]),
        )
        .unwrap();
    machine.call("lookup", &args([("code", first)])).unwrap();
    machine.call("lookup", &args([("code", second)])).unwrap();
    assert_eq!(machine.state.outbox[0].args, vec![int(4), amount]);
    assert_eq!(machine.state.outbox[1].args, vec![int(5), int(3)]);
    let before = machine.state.clone();
    assert_eq!(
        machine
            .call("unchecked", &args([("code", int(9))]))
            .unwrap_err()
            .code,
        "ERR_MISSING_ROW"
    );
    assert_eq!(machine.state, before);
}

#[test]
fn widened_aggregate_arithmetic_and_scalar_narrowing_have_distinct_domains() {
    let program = checked(
        r#"module WideSum;
table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
transition fill(value:u64) reads [] writes [T[*].v] body {let %new:id64<T>=table_alloc(T,{v:value});commit;}
transition total() reads [T[*].v] writes [] body {let %valid:bool=eq(mul(sum(T.v),0),0);guard(%valid,ERR_SUM);commit;}
transition narrow() reads [T[*].v] writes [] body {let %narrow:u64=sum(T.v);commit;}
transition set(k:id64<T>) reads [T[*].v] writes [T[k].v] body {table_set(T,k,v,sum(T.v));commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    for _ in 0..2 {
        machine
            .call("fill", &args([("value", int(u64::MAX as i128))]))
            .unwrap();
    }
    assert_eq!(
        machine.state.tables[0].aggregates[0],
        Some(BigInt::from(u64::MAX) * 2u8)
    );
    machine.call("total", &BTreeMap::new()).unwrap();
    let before = machine.state.clone();
    assert_eq!(
        machine.call("narrow", &BTreeMap::new()).unwrap_err().code,
        "ERR_ARITH_OVERFLOW"
    );
    assert_eq!(machine.state, before);
    assert_eq!(
        machine
            .call("set", &args([("k", int(4))]))
            .unwrap_err()
            .code,
        "ERR_MISSING_ROW"
    );
    assert_eq!(machine.state, before);
}

#[test]
fn exact_decimal_execution_rejects_fractional_remainders_atomically() {
    let program = checked(
        r#"module Decimal;
table T capacity 2 {key id:id64<T>;col v:dec128;}
transition exact() reads [] writes [T[*].v] body {
let %product:dec128=mul(1.000000000000000001,2.0);let %new:id64<T>=table_alloc(T,{v:%product});commit;}
transition inexact() reads [] writes [T[*].v] body {
let %new:id64<T>=table_alloc(T,{v:0});let %value:dec128=div(1.0,3.0);table_set(T,%new,v,%value);commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("exact", &BTreeMap::new()).unwrap();
    assert_eq!(
        machine.state.tables[0].columns[0][0],
        int(2_000_000_000_000_000_002)
    );
    let before = machine.state.clone();
    assert_eq!(
        machine.call("inexact", &BTreeMap::new()).unwrap_err().code,
        "ERR_ARITH_OVERFLOW"
    );
    assert_eq!(machine.state, before);
}

#[test]
fn a_pure_fold_in_an_old_key_uses_prestate_for_its_local_reads() {
    let program = checked(
        r#"module OldFold;
table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:i64;col result:i64;}
transition create() reads [] writes [T[*].owner,T[*].v,T[*].result]
body {let %new:id64<T>=table_alloc(T,{owner:id64<T>(2),v:7,result:0});commit;}
transition go(k:id64<T>) reads [T[k].owner,T[*].v] writes [T[k].owner,T[k].result]
require live:table_exists(T,k);
body {table_set(T,k,owner,id64<T>(3));
let %v:i64=old(T[fold_bounded<1>(a:id64<T>=k,i in [0..0]) {
  let %owner:id64<T>=table_get(T,k,owner);guard(true,ERR_REJECT);yield %owner;
}].v);table_set(T,k,result,%v);commit;}
"#,
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &BTreeMap::new()).unwrap();
    machine.call("go", &args([("k", int(2))])).unwrap();
    assert_eq!(machine.state.tables[0].columns[0][0], int(3));
    assert_eq!(machine.state.tables[0].columns[2][0], int(7));
}
