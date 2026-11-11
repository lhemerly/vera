use std::collections::BTreeMap;

use num_bigint::BigInt;
use serde_json::json;
use vera_compiler::projection::build_affordance_frame;
use vera_compiler::runtime::Machine;
use vera_compiler::value::Value;

fn int(value: u64) -> Value {
    Value::Int(BigInt::from(value))
}
fn args(name: &str, value: u64) -> BTreeMap<String, Value> {
    BTreeMap::from([(name.into(), int(value))])
}

#[test]
fn authorized_projection_exposes_values_and_state_dependent_intent_bounds() {
    let program = vera_compiler::semantic::analyze(
        vera_compiler::parse_source(include_str!("../../../examples/counter.vera")).unwrap(),
    )
    .unwrap();
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &args("initial", 7)).unwrap();
    let frame =
        build_affordance_frame(&program, &machine.state, "CounterView", &args("counter", 4))
            .unwrap();
    assert!(frame.allowed);
    let json = frame.to_json();
    assert_eq!(
        json["state_slice"]["Counter.value[4]"],
        json!({"id":4,"live":true,"value":7})
    );
    assert_eq!(json["affordances"][0]["transition"], "increase");
    assert_eq!(json["affordances"][0]["enabled"], true);
    assert_eq!(
        json["affordances"][0]["bounds"]["delta"]["max"],
        999999993u64
    );
    assert_eq!(json["affordances"][0]["bounds"]["counter"], json!({}));
    assert_eq!(machine.state.tables[0].columns[0][0], int(7));
}

#[test]
fn stale_handles_deny_access_or_produce_an_explicit_missing_slice() {
    let source = r#"module Views;
table T capacity 2 {key id:id64<T>;col v:u64;}
transition create() reads [] writes [T[*].v] body {let %new:id64<T>=table_alloc(T,{v:7});commit;}
projection Protected(k:id64<T>) {allow_if:table_exists(T,k);expose:[T[k].v];enable_intents:[];}
projection Public(k:id64<T>) {allow_if:true;expose:[T[k].v,T.v];enable_intents:[];}
"#;
    let program =
        vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap();
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &BTreeMap::new()).unwrap();
    let denied =
        build_affordance_frame(&program, &machine.state, "Protected", &args("k", 4)).unwrap();
    assert!(!denied.allowed);
    assert!(denied.state_slice.is_empty());
    assert!(denied.affordances.is_empty());
    let frame = build_affordance_frame(&program, &machine.state, "Public", &args("k", 4))
        .unwrap()
        .to_json();
    assert_eq!(
        frame["state_slice"]["T.v[4]"],
        json!({"id":4,"live":false,"value":null})
    );
    assert_eq!(frame["state_slice"]["T.v"], json!([{"id":2,"v":7}]));
}

#[test]
fn projection_parameters_are_strict_and_undefined_allow_if_fails_closed() {
    let program = vera_compiler::semantic::analyze(
        vera_compiler::parse_source(
            r#"module Deny;
table T capacity 1 {key id:id64<T>;col v:i64;}
transition noop() reads [] writes [] body {commit;}
projection View(k:id64<T>) {allow_if:eq(table_get(T,k,v),0);expose:[T[k].v];enable_intents:[];}
"#,
        )
        .unwrap(),
    )
    .unwrap();
    let machine = Machine::new(&program).unwrap();
    assert!(build_affordance_frame(&program, &machine.state, "View", &BTreeMap::new()).is_err());
    let frame = build_affordance_frame(&program, &machine.state, "View", &args("k", 1)).unwrap();
    assert!(!frame.allowed);
    assert!(frame.state_slice.is_empty());
}

#[test]
fn an_id_column_cannot_overwrite_the_handle_in_a_wildcard_projection() {
    let source = r#"module IdColumn;
table T capacity 2 {key handle:id64<T>;col id:u64;}
transition create() reads [] writes [T[*].id]
body {let %new:id64<T>=table_alloc(T,{id:99});commit;}
projection View() {allow_if:true;expose:[T.id];enable_intents:[];}
"#;
    let program =
        vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap();
    let mut machine = Machine::new(&program).unwrap();
    machine.call("create", &BTreeMap::new()).unwrap();
    let frame = build_affordance_frame(&program, &machine.state, "View", &BTreeMap::new())
        .unwrap()
        .to_json();
    assert_eq!(
        frame["state_slice"]["T.id"],
        json!([{"id":2,"columns":{"id":99}}])
    );
}
