//! Structured values at runtime and at the canonical host/persistence boundary.
use num_bigint::BigInt;
use serde_json::json;
use std::collections::BTreeMap;
use vera_compiler::{
    ir::{CheckedProgram, VariantId},
    parse_source,
    persistence::{decode_state, encode_state, schema_fingerprint, Intent},
    projection::build_affordance_frame,
    runtime::{eval_function, validate_state, FailureKind, Machine},
    semantic::analyze,
    types::ValueType,
    value::{decode_value, encode_value, value_from_json, value_to_json, Value},
};
const TYPES: &str = "record Pair {a:u8;b:u16;} union Message {Small:u8;Pair:Pair;}";
fn checked(body: &str) -> CheckedProgram {
    analyze(parse_source(&format!("{TYPES}{body}")).unwrap()).unwrap()
}
fn ty(p: &CheckedProgram, name: &str) -> ValueType {
    ValueType::Named(p.type_id(name).unwrap())
}
fn from(p: &CheckedProgram, name: &str, value: serde_json::Value) -> Value {
    value_from_json(p, &value, &ty(p, name)).unwrap()
}
fn int(n: u64) -> Value {
    Value::Int(BigInt::from(n))
}

#[test]
fn constructors_evaluate_in_written_order_but_store_in_declaration_order() {
    let p=checked("effect Mark(n:u8);effect Packed(p:Pair);transition go() reads [] writes [] body {let %p:Pair=record<Pair>(b:fold_bounded<1>(x:u16=0,i in [0..0]) {emit Mark(2);yield 9;},a:fold_bounded<1>(x:u8=0,i in [0..0]) {emit Mark(1);yield 7;});emit Packed(%p);commit;}");
    let mut m = Machine::new(&p).unwrap();
    m.call("go", &BTreeMap::new()).unwrap();
    assert_eq!(m.state.outbox[0].args, vec![int(2)]);
    assert_eq!(m.state.outbox[1].args, vec![int(1)]);
    assert_eq!(
        m.state.outbox[2].args[0],
        from(&p, "Pair", json!({"a":7,"b":9}))
    );
}

#[test]
fn wrong_active_payload_traps_atomically_and_lazy_select_is_safe() {
    let p=checked("effect Seen(v:Message);transition bad(v:Message) reads [] writes [] body {emit Seen(v);let %x:u8=payload<Message,Small>(v);commit;} transition safe(v:Message) reads [] writes [] body {let %x:u8=select(is_variant<Message,Small>(v),payload<Message,Small>(v),0);emit Seen(v);commit;}");
    let v = from(&p, "Message", json!({"tag":"Pair","payload":{"a":1,"b":2}}));
    let args = BTreeMap::from([("v".into(), v)]);
    let mut m = Machine::new(&p).unwrap();
    let before = m.state.clone();
    let error = m.call("bad", &args).unwrap_err();
    assert_eq!(error.kind, FailureKind::Trap);
    assert_eq!(error.code, "ERR_VARIANT_ACCESS");
    assert_eq!(m.state, before);
    m.call("safe", &args).unwrap();
    assert_eq!(m.state.outbox.len(), 1);
}

#[test]
fn canonical_bytes_use_packed_fields_u32_tags_and_zero_inactive_padding() {
    let p = checked("");
    let pair = from(&p, "Pair", json!({"b":770,"a":1}));
    assert_eq!(
        encode_value(&p, &pair, &ty(&p, "Pair")).unwrap(),
        vec![1, 2, 3]
    );
    let small = from(&p, "Message", json!({"tag":"Small","payload":7}));
    let bytes = encode_value(&p, &small, &ty(&p, "Message")).unwrap();
    assert_eq!(bytes, vec![0, 0, 0, 0, 7, 0, 0]);
    assert_eq!(decode_value(&p, &bytes, &ty(&p, "Message")).unwrap(), small);
    let mut bad = bytes.clone();
    bad[6] = 1;
    assert!(decode_value(&p, &bad, &ty(&p, "Message")).is_err());
    bad = bytes.clone();
    bad[0] = 2;
    assert!(decode_value(&p, &bad, &ty(&p, "Message")).is_err());
    assert!(decode_value(&p, &bytes[..6], &ty(&p, "Message")).is_err());
    let mut extra = bytes;
    extra.push(0);
    assert!(decode_value(&p, &extra, &ty(&p, "Message")).is_err());
}

#[test]
fn json_and_native_admission_reject_wrong_shapes_tags_and_payload_domains() {
    let p = checked("transition go(v:Message) reads [] writes [] body {commit;}");
    for bad in [
        json!({"a":1}),
        json!({"a":1,"b":2,"extra":0}),
        json!({"a":256,"b":2}),
        json!({"a":1,"other":2}),
    ] {
        assert!(value_from_json(&p, &bad, &ty(&p, "Pair")).is_err());
    }
    for bad in [
        json!({"tag":"Other","payload":7}),
        json!({"tag":"Small","payload":{"a":1,"b":2}}),
        json!({"tag":"Small","payload":256}),
        json!({"tag":"Small","payload":7,"extra":0}),
    ] {
        assert!(value_from_json(&p, &bad, &ty(&p, "Message")).is_err());
    }
    let invalid = Value::Variant {
        ty: p.type_id("Message").unwrap(),
        tag: VariantId(99),
        payload: Box::new(int(0)),
    };
    assert_eq!(
        Machine::new(&p)
            .unwrap()
            .call("go", &BTreeMap::from([("v".into(), invalid)]))
            .unwrap_err()
            .kind,
        FailureKind::Input
    );
}

#[test]
fn records_and_unions_round_trip_through_intents_snapshots_and_projections() {
    let p=checked("table T capacity 2 {key id:id64<T>;col pair:Pair;col message:Message;} effect Seen(v:Message); transition create(p:Pair,v:Message) reads [] writes [T[*].pair,T[*].message] body {let %k:id64<T>=table_alloc(T,{pair:p,message:v});emit Seen(v);commit;} projection View(k:id64<T>) {allow_if:table_exists(T,k);expose:[T[k].pair,T[k].message];enable_intents:[];}");
    let pair = json!({"a":3,"b":900});
    let message = json!({"tag":"Pair","payload":pair.clone()});
    let args = BTreeMap::from([
        ("p".into(), from(&p, "Pair", pair.clone())),
        ("v".into(), from(&p, "Message", message.clone())),
    ]);
    let intent = Intent::from_parameters(&p, "create", &args).unwrap();
    let (tag, bytes) = intent.encode(&p).unwrap();
    assert_eq!(Intent::decode(&p, tag, &bytes).unwrap(), intent);
    let mut m = Machine::new(&p).unwrap();
    m.call("create", &args).unwrap();
    let bytes = encode_state(&p, &m.state).unwrap();
    assert_eq!(decode_state(&p, &bytes).unwrap(), m.state);
    let view = build_affordance_frame(
        &p,
        &m.state,
        "View",
        &BTreeMap::from([("k".into(), int(2))]),
    )
    .unwrap()
    .to_json();
    assert_eq!(view["state_slice"]["T.pair[2]"]["value"], pair);
    assert_eq!(view["state_slice"]["T.message[2]"]["value"], message);
    let mut invalid = m.state.clone();
    if let Value::Variant { tag, .. } = &mut invalid.outbox[0].args[0] {
        *tag = VariantId(99);
    }
    assert!(validate_state(&p, &invalid).is_err());
    assert!(encode_state(&p, &invalid).is_err());
}

#[test]
fn schema_uses_type_names_and_definitions_not_resolved_type_ids() {
    let body = "table T capacity 1 {key id:id64<T>;col value:Message;}";
    let p = checked(body);
    let shifted =
        analyze(parse_source(&format!("record Unrelated {{unused:bool;}}{TYPES}{body}")).unwrap())
            .unwrap();
    assert_ne!(p.type_id("Message"), shifted.type_id("Message"));
    assert_eq!(schema_fingerprint(&p), schema_fingerprint(&shifted));
    let changed =
        analyze(parse_source(&format!("{}{body}", TYPES.replace("a:u8", "a:u16"))).unwrap())
            .unwrap();
    assert_ne!(schema_fingerprint(&p), schema_fingerprint(&changed));
}

#[test]
fn structured_unique_keys_compare_full_canonical_values_and_rollback_duplicates() {
    let p=checked("table T capacity 4 {key id:id64<T>;col code:Message [unique];} transition create(v:Message) reads [] writes [T[*].code] body {let %k:id64<T>=table_alloc(T,{code:v});commit;} transition lookup(v:Message) reads [T[*].code] writes [] require found:table_exists_unique(T,code,v); body {let %k:id64<T>=table_lookup(T,code,v);commit;}");
    let mut m = Machine::new(&p).unwrap();
    for v in [
        json!({"tag":"Small","payload":1}),
        json!({"tag":"Pair","payload":{"a":1,"b":0}}),
    ] {
        let args = BTreeMap::from([("v".into(), from(&p, "Message", v))]);
        m.call("create", &args).unwrap();
        m.call("lookup", &args).unwrap();
        let before = m.state.clone();
        assert_eq!(
            m.call("create", &args).unwrap_err().code,
            "ERR_DUPLICATE_KEY"
        );
        assert_eq!(m.state, before);
    }
    validate_state(&p, &m.state).unwrap();
    assert_eq!(
        decode_state(&p, &encode_state(&p, &m.state).unwrap()).unwrap(),
        m.state
    );
}

#[test]
fn pure_functions_accept_and_return_nominal_values() {
    let p=checked("function wrap(p:Pair)->Message body {yield variant<Message,Pair>(p);} function unwrap(v:Message)->Pair require right:is_variant<Message,Pair>(v); body {yield payload<Message,Pair>(v);}");
    let pair = from(&p, "Pair", json!({"a":5,"b":99}));
    let wrapped = eval_function(
        &p,
        p.function_id("wrap").unwrap(),
        std::slice::from_ref(&pair),
    )
    .unwrap();
    assert_eq!(
        eval_function(&p, p.function_id("unwrap").unwrap(), &[wrapped]).unwrap(),
        pair
    );
}

#[test]
fn native_fuzzer_samples_nested_structured_parameters() {
    let p=checked("effect Seen(v:Message);transition go(v:Message) reads [] writes [] ensure same:eq(effect_arg(Seen,0,v),v);body {emit Seen(v);commit;}");
    let result = vera_compiler::fuzz::fuzz_module(
        &p,
        &vera_compiler::fuzz::FuzzConfig {
            steps: 16,
            threads: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.ok);
    assert_eq!(result.committed, 32);
}

#[test]
fn cli_accepts_structured_json_parameters_and_emits_named_json() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let source=format!("{TYPES}effect Seen(v:Message);transition go(v:Message) reads [] writes [] body {{emit Seen(v);commit;}}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args([
            "run",
            "-",
            "--unverified",
            "--json",
            "--transition",
            "go",
            "--param",
            r#"v={"tag":"Pair","payload":{"a":2,"b":3}}"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(json.to_string().contains(r#""tag":"Pair""#), "{json}");
    let p = checked("");
    let value = from(&p, "Pair", json!({"a":2,"b":3}));
    assert_eq!(
        value_to_json(&p, &value, &ty(&p, "Pair")),
        json!({"a":2,"b":3})
    );
}

#[test]
fn corrupted_padding_is_rejected_inside_intents_and_checksummed_snapshots() {
    let p=checked("table T capacity 1 {key id:id64<T>;col v:Message;} transition create(v:Message) reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:v});commit;}");
    let args = BTreeMap::from([(
        "v".into(),
        from(&p, "Message", json!({"tag":"Small","payload":7})),
    )]);
    let intent = Intent::from_parameters(&p, "create", &args).unwrap();
    let (tag, mut bytes) = intent.encode(&p).unwrap();
    bytes[32 + 6] = 1;
    assert!(Intent::decode(&p, tag, &bytes).is_err());
    let mut m = Machine::new(&p).unwrap();
    m.call("create", &args).unwrap();
    let mut snapshot = encode_state(&p, &m.state).unwrap();
    // Header 40 bytes, one bitmap word, one generation, then the seven-byte union.
    snapshot[40 + 8 + 4 + 6] = 1;
    let end = snapshot.len() - 4;
    let checksum = vera_compiler::persistence::checksum(&snapshot[..end]);
    snapshot[end..].copy_from_slice(&checksum.to_le_bytes());
    assert!(decode_state(&p, &snapshot).is_err());
}

#[test]
fn projection_exact_parameter_hints_use_nominal_field_names() {
    let p=checked("transition receive(p:Pair) reads [] writes [] require fixed:eq(p,record<Pair>(a:3,b:4));body {commit;} projection View() {allow_if:true;expose:[];enable_intents:[receive when (true)];}");
    let m = Machine::new(&p).unwrap();
    let frame = build_affordance_frame(&p, &m.state, "View", &BTreeMap::new())
        .unwrap()
        .to_json();
    assert_eq!(
        frame["affordances"][0]["bounds"]["p"]["exact"],
        json!({"a":3,"b":4})
    );
}
