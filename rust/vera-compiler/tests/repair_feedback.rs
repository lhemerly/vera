//! Machine-readable repair evidence must describe concrete replay, not a model's claim.
use serde_json::json;
use vera_compiler::{
    parse_source,
    semantic::analyze,
    value::Value,
    verifier::{verify_transition, EnsureOutcome, VerifierConfig},
};

fn result(source: &str) -> vera_compiler::verifier::VerificationResult {
    let program = analyze(parse_source(source).unwrap()).unwrap();
    verify_transition(&program, "go", &VerifierConfig::default())
}

#[test]
fn failed_ensure_json_distinguishes_false_from_invalid_effect_access() {
    let source = r#"
effect E(value:u64, flag:bool);
transition go() reads [] writes []
 ensure wrong:eq(effect_arg(E,0,value),8);
 ensure absent:eq(effect_arg(E,1,value),7);
 body {emit E(7,true);commit;}
"#;
    let result = result(source);
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    let evidence = result.counterexample.as_ref().unwrap();
    assert_eq!(evidence.violated, vec!["ensure wrong", "ensure absent"]);
    assert_eq!(evidence.failed_ensures.len(), 2);
    assert!(matches!(
        evidence.failed_ensures[0].outcome,
        EnsureOutcome::Value {
            value: Value::Bool(false)
        }
    ));
    let EnsureOutcome::Error { error } = &evidence.failed_ensures[1].outcome else {
        panic!("missing error detail")
    };
    assert_eq!(error.code, "ERR_EFFECT_ACCESS");
    let error_span = error.span.unwrap();
    assert_eq!(
        &source[error_span.start..error_span.end],
        "effect_arg(E,1,value)"
    );
    let ensure_span = evidence.failed_ensures[1].span;
    assert_eq!(
        &source[ensure_span.start..ensure_span.end],
        "eq(effect_arg(E,1,value),7)"
    );
    let data = serde_json::to_value(evidence).unwrap();
    assert_eq!(
        data["failed_ensures"][0]["outcome"],
        json!({"kind":"value","value":{"Bool":false}})
    );
    assert_eq!(data["failed_ensures"][1]["outcome"]["kind"], "error");
    assert_eq!(
        data["failed_ensures"][1]["outcome"]["error"]["code"],
        "ERR_EFFECT_ACCESS"
    );
    assert_eq!(
        data["emitted_effects"],
        json!([{
            "index":0, "effect":"E", "arguments":{
                "value":{"type":"u64","value":7},
                "flag":{"type":"bool","value":true}
            }
        }])
    );
    assert!(evidence
        .directive
        .contains("emission count, type, order and payload"));
    // Non-vacuity means an execution committed, not that its postconditions held.
    let witness = result.witness.unwrap();
    assert_eq!(witness.failed_ensures.len(), 2);
    assert_eq!(witness.emitted_effects.len(), 1);
}

#[test]
fn successful_witness_reports_exact_order_and_source_level_types() {
    let source = r#"
table Account capacity 1 {key id:id64<Account>;col n:u8;}
effect Start(account:id64<Account>, amount:dec64, hash:bytes32);
effect End();
transition go() reads [] writes []
 ensure count:eq(effect_count(),2);
 body {emit Start(id64<Account>(0),1.0,0x1234);emit End();commit;}
"#;
    let result = result(source);
    assert!(result.ok(), "{}", result.summary());
    let witness = result.witness.unwrap();
    assert!(witness.failed_ensures.is_empty());
    let data = serde_json::to_value(witness).unwrap();
    let effects = data["emitted_effects"].as_array().unwrap();
    assert_eq!(effects.len(), 2);
    assert_eq!(effects[0]["effect"], "Start");
    assert_eq!(effects[1], json!({"index":1,"effect":"End","arguments":{}}));
    assert_eq!(
        effects[0]["arguments"]["account"],
        json!({"type":"id64<Account>","value":0})
    );
    assert_eq!(
        effects[0]["arguments"]["amount"],
        json!({"type":"dec64","value":100000000})
    );
    assert_eq!(effects[0]["arguments"]["hash"]["type"], "bytes32");
    assert_eq!(
        effects[0]["arguments"]["hash"]["value"],
        format!("0x3412{}", "00".repeat(30))
    );
}

#[test]
fn trapped_replay_never_reports_rolled_back_effects_as_committed() {
    let source = "effect E();transition go(x:u8) reads [] writes [] body {emit E();let %bad:u8=add(x,1);commit;}";
    let result = result(source);
    assert_eq!(result.panic.status, "sat", "{}", result.summary());
    let witness = result.counterexample.unwrap();
    assert!(witness.emitted_effects.is_empty());
    assert!(witness.failed_ensures.is_empty());
    assert_eq!(witness.failure.unwrap().code, "ERR_ARITH_OVERFLOW");
    assert!(witness.trace.iter().any(|step| step.kind == "effect"));
}
