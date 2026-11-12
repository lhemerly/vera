//! Native SMT datatype proofs and independently replayed structured witnesses.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    value::Value,
    verifier::{verify_all, verify_transition, VerifierConfig},
};
fn program(source: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(source).expect("parse")).unwrap_or_else(|e| panic!("{e:?}\n{source}"))
}
fn verify(source: &str) -> vera_compiler::verifier::VerificationResult {
    verify_transition(&program(source), "go", &VerifierConfig::default())
}
fn accepted(source: &str) {
    let result = verify(source);
    assert!(result.ok(), "{}", result.summary());
    assert!(result.witness.is_some());
    assert!(result.smt_safety.starts_with("(set-logic ALL)"));
}

#[test]
fn structured_inputs_have_native_constructor_witnesses_and_exact_leaf_domains() {
    let source="record Info {n:u64;ok:bool;tag:enum {A,B,C};} transition go(r:Info) reads [] writes [] ensure tags:or(eq(field(r,tag),A),or(eq(field(r,tag),B),eq(field(r,tag),C)));ensure wrong:eq(field(r,n),0);body {commit;}";
    let r = verify(source);
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
    let ce = r.counterexample.unwrap();
    let Value::Record { fields, .. } = &ce.params["r"] else {
        panic!("typed record lost")
    };
    assert_eq!(fields.len(), 3);
    assert!(matches!(fields[1], Value::Bool(_)));
    assert!(fields[2].as_u64().unwrap() < 3);
    assert_ne!(fields[0].as_u64(), Some(0));
    assert_eq!(ce.violated, vec!["ensure wrong"]);
}

#[test]
fn record_function_contract_and_outbox_preserve_nominal_values() {
    accepted("record Pair {x:u64;flag:bool;} function same(p:Pair)->Pair ensure exact:eq(result,p);body {yield p;} effect Saved(p:Pair);transition go(p:Pair) reads [] writes [] ensure payload:eq(effect_arg(Saved,0,p),p);body {emit Saved(same(p));commit;}");
}

#[test]
fn union_payload_requires_variant_guard_and_nested_values_replay() {
    let types = "record R {n:u8;tag:enum {A,B};} union U {Left:R;Right:bool;}";
    accepted(&format!("{types} transition go(u:U) reads [] writes [] ensure safe:select(is_variant<U,Left>(u),or(eq(field(payload<U,Left>(u),tag),A),eq(field(payload<U,Left>(u),tag),B)),true);body {{commit;}}"));
    let result=verify(&format!("{types} transition go(u:U) reads [] writes [] ensure invalid:eq(field(payload<U,Left>(u),n),0);body {{commit;}}"));
    assert_eq!(result.safety.status, "sat", "{}", result.summary());
    assert!(matches!(
        result.counterexample.as_ref().unwrap().params["u"],
        Value::Variant { .. }
    ));
    let partial=verify(&format!("{types} transition go() reads [] writes [] ensure invalid:eq(field(payload<U,Left>(variant<U,Right>(true)),n),0);body {{commit;}}"));
    assert_eq!(partial.safety.status, "sat", "{}", partial.summary());
    assert!(partial
        .counterexample
        .unwrap()
        .failed_ensures
        .iter()
        .any(|e| matches!(
            &e.outcome,
            vera_compiler::verifier::EnsureOutcome::Error { .. }
        )));
}

#[test]
fn record_table_updates_and_old_values_are_proved_without_flattening() {
    accepted("record R {n:u64;flag:bool;} table T capacity 2 {key id:id64<T>;col r:R;} invariant bounds {forall k in T:le(field(T[k].r,n),10);} transition go(k:id64<T>) reads [T[k].r] writes [T[k].r] require live:table_exists(T,k);ensure saved:eq(field(T[k].r,n),7);ensure flag:eq(field(T[k].r,flag),field(old(T[k].r),flag));body {let %r:R=table_get(T,k,r);table_set(T,k,r,record<R>(n:7,flag:field(%r,flag)));commit;}");
}

#[test]
fn scalar_aggregate_carriers_survive_field_and_payload_projection() {
    accepted("record R {n:u64;} union U {Data:u64;Other:bool;} table T capacity 1 {key id:id64<T>;col n:u64 [tracked_sum];} transition go(r:R,u:U) reads [] writes [] ensure field:eq(add(sum(T.n),field(r,n)),add(sum(T.n),field(r,n)));ensure payload:select(is_variant<U,Data>(u),eq(add(sum(T.n),payload<U,Data>(u)),add(sum(T.n),payload<U,Data>(u))),true);body {commit;}");
}

#[test]
fn forward_type_dependencies_and_canonical_empty_record_are_supported() {
    accepted("record Outer {inner:Inner;empty:Empty;} record Empty {} record Inner {flag:bool;} transition go(o:Outer) reads [] writes [] ensure rebuilt:eq(o,record<Outer>(empty:record<Empty>(),inner:record<Inner>(flag:field(field(o,inner),flag))));body {commit;}");
}

#[test]
fn weak_structured_summary_never_turns_spurious_sat_into_proof() {
    let p=program("record R {n:u8;} function zero(x:R)->R ensure weak:true;body {yield record<R>(n:0);} transition go(x:R) reads [] writes [] ensure actual:eq(field(zero(x),n),0);body {commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert_eq!(r.safety.status, "unknown", "{}", r.summary());
    assert!(r.counterexample.is_none());
    let results = verify_all(&p, &VerifierConfig::default());
    assert!(results[0].ok(), "{}", results[0].summary());
}

#[test]
fn datatype_validity_schema_scales_with_variants_not_variant_combinations() {
    let mut previous = 0;
    for count in [4, 8, 16] {
        let variants = (0..count)
            .map(|i| format!("V{i}:Leaf;"))
            .collect::<String>();
        let source=format!("record Leaf {{tag:enum {{A,B,C}};}} union U {{{variants}}} record Pair {{left:U;right:U;}} transition go(p:Pair) reads [] writes [] ensure identity:eq(p,p);body {{commit;}}");
        let r = verify(&source);
        assert!(r.ok(), "{}", r.summary());
        assert_eq!(r.smt_safety.matches("(declare-datatypes").count(), 1);
        assert_eq!(r.smt_safety.matches("(define-fun valid_").count(), 3);
        let size = r.smt_safety.len();
        if previous > 0 {
            assert!(
                size < previous * 3,
                "variant schema expanded superlinearly: {size} vs {previous}"
            );
        }
        previous = size;
    }
}

#[test]
fn nested_variant_model_reconstruction_preserves_signed_and_enum_payloads() {
    let r=verify("record R {n:i64;flag:bool;tag:enum {A,B};} union U {Data:R;Empty:bool;} transition go(u:U) reads [] writes [] require data:is_variant<U,Data>(u);require n:eq(field(payload<U,Data>(u),n),-9223372036854775808);require flag:field(payload<U,Data>(u),flag);require tag:eq(field(payload<U,Data>(u),tag),B);ensure wrong:eq(field(payload<U,Data>(u),n),0);body {commit;}");
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
    let ce = r.counterexample.unwrap();
    let Value::Variant { payload, .. } = &ce.params["u"] else {
        panic!("missing variant")
    };
    let Value::Record { fields, .. } = &**payload else {
        panic!("missing record payload")
    };
    assert_eq!(fields[0], Value::Int(i64::MIN.into()));
    assert_eq!(fields[1], Value::Bool(true));
    assert_eq!(fields[2], Value::Int(1.into()));
}

#[test]
fn structured_unique_keys_use_logical_datatype_equality() {
    accepted("record Key {n:u8;flag:bool;} table T capacity 2 {key id:id64<T>;col code:Key [unique];} transition go(code:Key) reads [T[*].code] writes [] require found:table_exists_unique(T,code,code);ensure value:eq(table_get(T,table_lookup(T,code,code),code),code);body {commit;}");
}

#[test]
fn physical_index_failure_with_tight_helper_is_not_reported_as_summary_weakness() {
    let mut source=String::from("table T capacity 32 {key id:id64<T>;col code:bytes32 [unique];} function zero()->u8 ensure exact:eq(result,0);body {yield 0;}");
    for slot in 0..32u64 {
        let key = 32 + slot;
        let code = u128::from(slot + 1) << 64;
        source.push_str(&format!("invariant row_{slot} {{and(table_exists(T,id64<T>({key})),eq(T[id64<T>({key})].code,{code}));}}"));
    }
    source.push_str("transition go() reads [] writes [] body {let %zero:u8=zero();commit;}");
    let r = verify(&source);
    assert_eq!(r.liveness.status, "unknown", "{}", r.summary());
    let reason = r.liveness.reason.unwrap();
    assert!(reason.contains("index materialization"), "{reason}");
    assert!(!reason.contains("contract summary"), "{reason}");
}

#[test]
fn function_repair_returns_source_named_record_and_union_json() {
    for (ty, body, expected) in [
        (
            "R",
            "record<R>(n:7,flag:true)",
            serde_json::json!({"n":7,"flag":true}),
        ),
        (
            "U",
            "variant<U,Data>(record<R>(n:7,flag:true))",
            serde_json::json!({"tag":"Data","payload":{"n":7,"flag":true}}),
        ),
    ] {
        let source = format!("record R {{n:u8;flag:bool;}} union U {{Data:R;Other:bool;}} function bad()->{ty} ensure wrong:false;body {{yield {body};}}");
        let p = program(&source);
        let results = verify_all(&p, &VerifierConfig::default());
        let r = &results[0];
        assert_eq!(r.safety.status, "sat", "{}", r.summary());
        let ce = r.counterexample.as_ref().unwrap();
        assert!(ce.return_value.is_some());
        let json = serde_json::to_value(ce).unwrap();
        assert_eq!(
            json["typed_return_value"],
            serde_json::json!({"type":ty,"value":expected})
        );
        let wrong = ce
            .failed_ensures
            .iter()
            .find(|e| e.name == "wrong")
            .unwrap();
        assert_eq!(wrong.span, p.expr(p.functions()[0].ensures[0].value).span);
        assert!(matches!(
            wrong.outcome,
            vera_compiler::verifier::EnsureOutcome::Value {
                value: Value::Bool(false)
            }
        ));
    }
    let r = verify("transition go() reads [] writes [] body {commit;}");
    assert!(r.witness.unwrap().typed_return_value.is_none());
    let p = program("function bad(x:u8)->u8 body {yield div(1,x);}");
    let results = verify_all(&p, &VerifierConfig::default());
    let ce = results[0].counterexample.as_ref().unwrap();
    assert!(ce.failure.is_some());
    assert!(ce.return_value.is_none());
    assert!(ce.typed_return_value.is_none());
}
