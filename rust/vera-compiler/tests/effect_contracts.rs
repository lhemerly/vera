//! Effect contracts reason about the exact ordered, transaction-local outbox.
use std::collections::BTreeMap;
use vera_compiler::{
    ir::CheckedProgram,
    parse_source,
    runtime::{eval_expression, Machine},
    semantic::analyze,
    value::Value,
    verifier::{verify_transition, VerificationResult, VerifierConfig},
};

fn program(source: &str) -> CheckedProgram {
    analyze(parse_source(source).unwrap()).unwrap_or_else(|e| panic!("{e:?}\n{source}"))
}
fn verify(source: &str) -> VerificationResult {
    verify_transition(&program(source), "go", &VerifierConfig::default())
}
fn accepted(result: &VerificationResult) {
    assert!(result.ok(), "{}", result.summary());
    assert!(
        result.witness.is_some(),
        "must replay a concrete committing witness"
    );
}
fn rejected(result: &VerificationResult, law: &str) {
    assert!(!result.ok(), "incorrect effect contract accepted");
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    let witness = result
        .counterexample
        .as_ref()
        .expect("replay-validated counterexample");
    assert!(
        witness.violated.iter().any(|name| name == law),
        "{witness:?}"
    );
}

const SOURCE: &str = r#"
effect First(value:u64, flag:bool);
effect Last(value:u64);
transition go(x:u64) reads [] writes []
 ensure count:eq(effect_count(),2);
 ensure first:effect_is(First,0);
 ensure last:effect_is(Last,1);
 ensure value:eq(effect_arg(First,0,value),x);
 ensure flag:eq(effect_arg(First,0,flag),true);
 ensure tail:eq(effect_arg(Last,1,value),x);
 ensure missing:eq(effect_is(First,2),false);
 ensure wrong:eq(effect_is(Last,0),false);
 body { emit First(x,true); emit Last(x); commit; }
"#;

#[test]
fn count_kind_order_and_all_payload_fields_are_proved() {
    accepted(&verify(SOURCE));
}

#[test]
fn missing_extra_wrong_kind_reordered_and_wrong_payload_are_rejected() {
    for (source, law) in [
        (SOURCE.replace("emit Last(x);", ""), "ensure count"),
        (
            SOURCE.replace("emit Last(x);", "emit Last(x); emit Last(x);"),
            "ensure count",
        ),
        (
            SOURCE.replace("emit Last(x);", "emit First(x,true);"),
            "ensure last",
        ),
        (
            SOURCE.replace(
                "emit First(x,true); emit Last(x);",
                "emit Last(x); emit First(x,true);",
            ),
            "ensure first",
        ),
        (
            SOURCE.replace("emit First(x,true);", "emit First(0,true);"),
            "ensure value",
        ),
        (
            SOURCE.replace("emit First(x,true);", "emit First(x,false);"),
            "ensure flag",
        ),
    ] {
        rejected(&verify(&source), law);
    }
}

#[test]
fn partial_payload_access_fails_closed_but_select_is_lazy() {
    rejected(&verify("effect E(x:u8); transition go() reads [] writes [] ensure bad:eq(effect_arg(E,0,x),0); body {commit;}"), "ensure bad");
    rejected(&verify("effect E(x:u8); effect F(x:u8); transition go() reads [] writes [] ensure bad:eq(effect_arg(E,0,x),0); body {emit F(0);commit;}"), "ensure bad");
    accepted(&verify("effect E(x:u8); transition go() reads [] writes [] ensure safe:select(effect_is(E,0),eq(effect_arg(E,0,x),0),true); body {commit;}"));
    // Boolean and is deliberately eager: a false type predicate is not a guard.
    rejected(&verify("effect E(x:u8); transition go() reads [] writes [] ensure bad:and(effect_is(E,0),eq(effect_arg(E,0,x),0)); body {commit;}"), "ensure bad");
}

fn loop_source(bound: usize) -> String {
    format!(
        r#"
effect Item(value:u64);
transition go(n:u64) reads [] writes []
 require small:lt(n,{bound});
 ensure count:eq(effect_count(),add(n,1));
 ensure values:fold_bounded<{bound}>(ok:bool=true,i in [0..n]) {{
   yield and(ok,select(effect_is(Item,i),eq(effect_arg(Item,i,value),i),false));
 }};
 body {{ let %count:u64=fold_bounded<{bound}>(acc:u64=0,i in [0..n]) {{
   emit Item(i); yield add(acc,1);
 }}; commit; }}
"#
    )
}

#[test]
fn symbolic_fold_length_and_payload_order_are_exact() {
    accepted(&verify(&loop_source(8)));
    rejected(
        &verify(&loop_source(8).replace("emit Item(i)", "emit Item(n)")),
        "ensure values",
    );
    accepted(&verify("effect E(); transition go() reads [] writes [] ensure none:eq(effect_count(),0); body {let %n:u8=fold_bounded<8>(a:u8=0,i in [2..1]) {emit E();yield a;};commit;}"));
}

#[test]
fn selected_effectful_branches_do_not_publish_unselected_events() {
    accepted(&verify(
        r#"effect E(x:u8); effect F(x:u8);
transition go(choose:bool) reads [] writes []
 ensure count:eq(effect_count(),1);
 ensure kind:select(choose,effect_is(E,0),effect_is(F,0));
 body {let %x:u8=select(choose,fold_bounded<1>(a:u8=0,i in [0..0]) {emit E(7);yield a;},fold_bounded<1>(b:u8=0,j in [0..0]) {emit F(8);yield b;});commit;}"#,
    ));
}

#[test]
fn payload_scalar_domains_and_type_inference_are_preserved() {
    accepted(&verify(
        r#"effect E(b:bool,byte:u8,small:u16,word:u32,big:u64,signed:i64,d:dec64,wide:dec128,s:sym32,hash:bytes32,tag:enum {A,B});
transition go() reads [] writes []
 ensure b:effect_arg(E,0,b);
 ensure byte:eq(effect_arg(E,0,byte),255);
 ensure small:eq(effect_arg(E,0,small),65535);
 ensure word:eq(effect_arg(E,0,word),4294967295);
 ensure big:eq(effect_arg(E,0,big),18446744073709551615);
 ensure signed:eq(effect_arg(E,0,signed),-9223372036854775808);
 ensure d:eq(effect_arg(E,0,d),1.0);
 ensure wide:eq(effect_arg(E,0,wide),1.0);
 ensure s:eq(effect_arg(E,0,s),"hello");
 ensure hash:eq(effect_arg(E,0,hash),0x1234);
 ensure tag:eq(effect_arg(E,0,tag),B);
 body {emit E(true,255,65535,4294967295,18446744073709551615,-9223372036854775808,1.0,1.0,"hello",0x1234,B);commit;}"#,
    ));
}

#[test]
fn effect_queries_are_restricted_to_ensures_and_checked_statically() {
    for source in [
        "effect E(); invariant no {eq(effect_count(),0); }",
        "effect E(); transition go() reads [] writes [] require no:eq(effect_count(),0); body {commit;}",
        "effect E(); transition go() reads [] writes [] body {let %x:u64=effect_count();commit;}",
        "effect E(); projection P() {allow_if:eq(effect_count(),0);expose:[];enable_intents:[];}",
    ] { assert!(analyze(parse_source(source).unwrap()).is_err(), "{source}"); }
    for expr in [
        "effect_is(Missing,0)",
        "eq(effect_arg(E,0,no),0)",
        "effect_is(E,true)",
        "effect_is(E,-1)",
    ] {
        let source = format!(
            "effect E(x:u8);transition go() reads [] writes [] ensure bad:{expr};body {{commit;}}"
        );
        assert!(analyze(parse_source(&source).unwrap()).is_err(), "{source}");
    }
}

#[test]
fn concrete_queries_ignore_old_outbox_and_reverts_remain_atomic() {
    let source = SOURCE.replace(
        "body { emit First(x,true);",
        "body { emit First(x,true); guard(ne(x,0),ERR_ZERO);",
    );
    let checked = program(&source);
    let mut machine = Machine::new(&checked).unwrap();
    let args = BTreeMap::from([("x".to_owned(), Value::Int(7.into()))]);
    machine.call("go", &args).unwrap();
    let before = machine.state.clone();
    let execution = machine.execute("go", &args, false).unwrap();
    for ensure in &checked.transitions()[0].ensures {
        assert_eq!(
            eval_expression(
                &checked,
                &machine.state,
                &before,
                &execution.bindings,
                ensure.value
            )
            .unwrap(),
            Value::Bool(true)
        );
    }
    let before = machine.state.clone();
    let args = BTreeMap::from([("x".to_owned(), Value::Int(0.into()))]);
    assert!(machine.call("go", &args).is_err());
    assert_eq!(machine.state, before);
}

#[test]
fn large_indices_are_total_for_kind_tests_not_wrapped() {
    accepted(&verify("effect E();transition go() reads [] writes [] ensure missing:eq(effect_is(E,18446744073709551615),false);body {emit E();commit;}"));
}

#[test]
fn bounded_contract_encoding_does_not_enumerate_effect_sequences() {
    let mut previous = 0;
    for bound in [8, 16, 32, 64] {
        let source = format!(
            r#"effect Item(value:u64);
transition go(i:u64) reads [] writes [] require valid:lt(i,{bound});
 ensure count:eq(effect_count(),{bound});
 ensure kind:effect_is(Item,i);
 ensure payload:eq(effect_arg(Item,i,value),i);
 body {{let %zero:u64=0;let %n:u64=fold_bounded<{bound}>(a:u64=0,j in [%zero..{last}]) {{emit Item(j);yield a;}};commit;}}"#,
            last = bound - 1
        );
        let result = verify(&source);
        accepted(&result);
        let size = result.smt_safety.len();
        assert!(size < 2_000_000, "unexpected expansion: {size}");
        if previous != 0 {
            assert!(size < previous * 3, "nonlinear effect encoding");
        }
        previous = size;
    }
}

#[test]
fn wide_aggregate_indices_do_not_truncate_or_produce_ill_sorted_smt() {
    let source = r#"table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
effect E(x:u64);
transition go() reads [] writes []
 ensure kind:eq(effect_is(E,sum(T.v)),eq(sum(T.v),0));
 ensure payload:select(effect_is(E,sum(T.v)),eq(effect_arg(E,sum(T.v),x),7),true);
 body {emit E(7);commit;}"#;
    accepted(&verify(source));
    let checked = program(source);
    let mut machine = Machine::new(&checked).unwrap();
    machine.state.tables[0].occupied = vec![true, true];
    machine.state.tables[0].generations = vec![1, 1];
    machine.state.tables[0].columns[0] = vec![Value::Int(u64::MAX.into()), Value::Int(1.into())];
    machine.recompute_aggregates().unwrap();
    let pre = machine.state.clone();
    let execution = machine.execute("go", &BTreeMap::new(), false).unwrap();
    for ensure in &checked.transitions()[0].ensures {
        assert_eq!(
            eval_expression(
                &checked,
                &machine.state,
                &pre,
                &execution.bindings,
                ensure.value
            )
            .unwrap(),
            Value::Bool(true)
        );
    }
}

#[test]
fn nested_emitting_arguments_cannot_exceed_event_budget() {
    let checked=program("effect Inner();effect Outer(x:u8);transition go() reads [] writes [] ensure count:eq(effect_count(),2);body {emit Outer(fold_bounded<1>(a:u8=0,i in [0..0]) {emit Inner();yield a;});commit;}");
    let result = verify_transition(
        &checked,
        "go",
        &VerifierConfig {
            max_effect_events: 1,
            ..VerifierConfig::default()
        },
    );
    assert!(!result.ok());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "D_VERIFY_RESOURCE" && d.message.contains("max_effect_events")),
        "{}",
        result.summary()
    );
    accepted(&verify_transition(
        &checked,
        "go",
        &VerifierConfig {
            max_effect_events: 2,
            ..VerifierConfig::default()
        },
    ));
}

#[test]
fn contextual_query_names_remain_valid_existing_identifiers() {
    accepted(&verify("transition effect_arg(effect_count:u64) reads [] writes [] body {commit;} transition go(effect_is:u64) reads [] writes [] ensure same:eq(effect_is,effect_is);body {commit;}"));
}

#[test]
fn old_keys_cannot_observe_post_transaction_effects() {
    for key in [
        "effect_count()",
        "select(effect_is(E,0),0,1)",
        "effect_arg(E,0,value)",
        "fold_bounded<1>(a:u64=0,i in [0..0]) {yield effect_count();}",
    ] {
        let source=format!("table T capacity 2 {{key id:id64<T>;col index:u64;}}effect E(value:u64);transition go() reads [] writes [] ensure bad:eq(old(T[id64<T>({key})].index),0);body {{emit E(0);commit;}}");
        let errors = analyze(parse_source(&source).unwrap()).unwrap_err();
        assert!(
            errors.iter().any(|d| d.code == "D_EFFECT_CONTEXT"),
            "{errors:?}"
        );
    }
    accepted(&verify("table T capacity 1 {key id:id64<T>;col index:u64;}effect E(value:u64);transition go() reads [] writes [] ensure payload:select(effect_is(E,old(T[id64<T>(0)].index)),eq(effect_arg(E,old(T[id64<T>(0)].index),value),7),true);body {emit E(7);commit;}"));
}

#[test]
fn effect_contracts_participate_in_semantic_program_identity() {
    use vera_compiler::persistence::{program_fingerprint, Intent};
    let first = program(SOURCE);
    let changed = program(&SOURCE.replace(
        "ensure count:eq(effect_count(),2)",
        "ensure count:eq(effect_count(),3)",
    ));
    assert_ne!(program_fingerprint(&first), program_fingerprint(&changed));
    let intent = Intent::from_parameters(
        &first,
        "go",
        &BTreeMap::from([("x".into(), Value::Int(7.into()))]),
    )
    .unwrap();
    let (transition, bytes) = intent.encode(&first).unwrap();
    assert!(Intent::decode(&changed, transition, &bytes).is_err());
}
