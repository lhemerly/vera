use vera_compiler::{
    analyze, parse_source,
    verifier::{verify_all, verify_function, verify_operation, verify_transition, VerifierConfig},
};

#[test]
fn inventory_exposes_unmentioned_values_without_claiming_a_coverage_score() {
    let p=analyze(parse_source("effect E(value:u8); transition t(x:u8,y:u8) reads [] writes [] ensure tautology:eq(x,x); body {emit E(y);commit;}").unwrap()).unwrap();
    let r = verify_transition(&p, "t", &VerifierConfig::default());
    assert!(r.ok());
    let f = r.specification_footprint.unwrap();
    assert_eq!(f.mentioned, vec!["input:x"]);
    assert!(f.unmentioned.contains(&"input:y".into()));
    assert!(f.unmentioned.contains(&"effect_arg:E.value".into()));
    assert!(f.implementation_references.contains(&"input:y".into()));
    assert_eq!(f.committing_witness.status, "replayed");
    assert!(f.interpretation.contains("not_semantic"));
    assert!(f.unknowns.iter().any(|s| s.contains("tautological")));
    let json = serde_json::to_string(&f).unwrap();
    assert!(!json.contains("\"percentage\""));
    assert!(!json.contains("\"score\""));
}

#[test]
fn properties_map_state_effects_and_input_references() {
    let p = analyze(parse_source(include_str!("../../../examples/effect_counter.vera")).unwrap())
        .unwrap();
    let r = verify_transition(&p, "increase", &VerifierConfig::default());
    assert!(r.ok());
    let f = r.specification_footprint.unwrap();
    let after = f
        .properties
        .iter()
        .find(|p| p.name == "event_after")
        .unwrap();
    assert!(after
        .references
        .contains(&"effect_arg:Updated.after".into()));
    assert!(after.references.contains(&"effect:Updated".into()));
    assert!(after.references.contains(&"state:Counter.value".into()));
    assert!(after.references.contains(&"input:counter".into()));
    assert!(f.properties.iter().any(|p| p.kind == "invariant"));
    assert!(f.unmentioned.is_empty());
}

#[test]
fn function_footprint_is_target_local_and_reports_return() {
    let p =
        analyze(parse_source(include_str!("../../../examples/functions.vera")).unwrap()).unwrap();
    let r = verify_function(&p, "twice_increment", &VerifierConfig::default());
    let f = r.specification_footprint.unwrap();
    assert_eq!(f.universe_scope, "target_inputs_and_return");
    assert_eq!(f.inventory.len(), 2);
    assert!(f.mentioned.contains(&"result".into()));
    assert!(f
        .implementation_references
        .contains(&"function:increment".into()));
    assert!(f.unmentioned.is_empty());
}

#[test]
fn errors_and_component_contracts_remain_explicitly_unproved_by_mentions() {
    let p =
        analyze(parse_source(include_str!("../../../examples/components.vera")).unwrap()).unwrap();
    let r = verify_transition(&p, "twice", &VerifierConfig::default());
    let f = r.specification_footprint.unwrap();
    assert!(f.unmentioned.contains(&"error:ERR_REJECT".into()));
    assert!(f
        .implementation_references
        .contains(&"operation:Counter.inc".into()));
    let r = verify_operation(&p, "Counter.inc", &VerifierConfig::default());
    assert!(r
        .specification_footprint
        .unwrap()
        .properties
        .iter()
        .any(|p| p.kind == "invariant"));
    assert!(verify_all(&p, &VerifierConfig::default())
        .iter()
        .all(|r| r.specification_footprint.is_some()));
}

#[test]
fn failure_and_unknown_keep_honest_witness_evidence() {
    let p = analyze(
        parse_source("transition t(x:u8) reads [] writes [] ensure wrong:eq(x,0); body {commit;}")
            .unwrap(),
    )
    .unwrap();
    let r = verify_transition(&p, "t", &VerifierConfig::default());
    let f = r.specification_footprint.unwrap();
    assert_eq!(f.counterexample.status, "replayed");
    assert_eq!(f.joint_safety_status, "sat");
    let config = VerifierConfig {
        max_expression_steps: Some(0),
        ..VerifierConfig::default()
    };
    let r = verify_transition(&p, "t", &config);
    let f = r.specification_footprint.unwrap();
    assert_eq!(f.committing_witness.status, "not_established");
    assert_eq!(f.counterexample.status, "not_established");
    assert!(verify_transition(&p, "missing", &config)
        .specification_footprint
        .is_none());
}

#[test]
fn json_metadata_is_additive_and_deterministic() {
    let p = analyze(
        parse_source("transition t(x:u8) reads [] writes [] ensure same:eq(x,x); body {commit;}")
            .unwrap(),
    )
    .unwrap();
    let a = verify_transition(&p, "t", &VerifierConfig::default());
    let b = verify_transition(&p, "t", &VerifierConfig::default());
    assert_eq!(
        serde_json::to_value(&a.specification_footprint).unwrap(),
        serde_json::to_value(&b.specification_footprint).unwrap()
    );
    let json = serde_json::to_value(a).unwrap();
    assert!(json.get("safety").is_some());
    assert!(json.get("witness").is_some());
    assert_eq!(json["specification_footprint"]["schema_version"], 1);
}
