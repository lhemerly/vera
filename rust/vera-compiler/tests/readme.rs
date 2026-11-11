//! Keep the introductory program and its stated behavior executable.

use std::collections::BTreeMap;

use vera_compiler::{
    analyze, parse_source,
    runtime::{recompute_aggregates, FailureKind, Machine, State},
    value::Value,
    verifier::{verify_all, VerifierConfig},
};

#[test]
fn readme_program_verifies_and_behaves_as_described() {
    let markdown = include_str!("../../../README.md");
    let source = markdown
        .split_once("```vera\n")
        .expect("README contains its introductory VERA program")
        .1
        .split_once("\n```")
        .expect("README program has a closing fence")
        .0;
    let program = analyze(parse_source(source).expect("README syntax is valid"))
        .expect("README program is well typed");
    for result in verify_all(&program, &VerifierConfig::default()) {
        assert!(result.ok(), "{}", result.summary());
    }

    let table = program.table_id("Counter").unwrap();
    let column = program.table(table).column_id("value").unwrap();
    let key = 1u64 << program.table(table).key_bits();
    let mut state = State::empty(&program).unwrap();
    state.tables[table.0].occupied[0] = true;
    state.tables[table.0].generations[0] = 1;
    state.tables[table.0].columns[column.index][0] = Value::Int(7.into());
    recompute_aggregates(&program, &mut state).unwrap();
    let mut machine = Machine::with_state(&program, state).unwrap();
    let mut params = BTreeMap::from([
        ("counter".into(), Value::Int(key.into())),
        ("delta".into(), Value::Int(3.into())),
    ]);
    machine.call("increase", &params).unwrap();
    assert_eq!(
        machine.state.tables[table.0].columns[column.index][0],
        Value::Int(10.into())
    );
    let event = &machine.state.outbox[0];
    assert_eq!(program.effect(event.effect).name, "Increased");
    assert_eq!(event.args, [Value::Int(key.into()), Value::Int(3.into())]);

    let before = machine.state.clone();
    params.insert("delta".into(), Value::Int(991.into()));
    let error = machine.call("increase", &params).unwrap_err();
    assert_eq!(error.kind, FailureKind::Guard);
    assert_eq!(error.code, "ERR_LIMIT");
    assert_eq!(machine.state, before);
}
