use num_bigint::BigInt;
use vera_compiler::{
    ir::{CheckedProgram, TypeDefinitionKind},
    runtime::eval_function,
    types::{ScalarType, ValueType},
    value::{coerce_value, decode_value, encode_value, value_from_json, value_to_json, Value},
};
fn checked(s: &str) -> CheckedProgram {
    vera_compiler::semantic::analyze(vera_compiler::parse_source(s).unwrap()).unwrap()
}
fn reject(s: &str, code: &str) {
    let e = vera_compiler::semantic::analyze(vera_compiler::parse_source(s).unwrap()).unwrap_err();
    assert!(e.iter().any(|e| e.code == code), "{e:?}");
}
fn int(x: u64) -> Value {
    Value::Int(BigInt::from(x))
}
const TYPES: &str =
    "record Pair {a:u8;b:u16;} record Empty {} union Message {Small:u8;Pair:Pair;Empty:Empty;}";
#[test]
fn records_and_unions_are_nominal_and_reusable_in_functions() {
    let p=checked(&format!("{TYPES} function make()->Pair body {{yield record<Pair>(b:513,a:7);}} function wrap(x:Pair)->Message body {{yield variant<Message,Pair>(x);}} function read(x:Message)->u16 require pair:is_variant<Message,Pair>(x); body {{yield field(payload<Message,Pair>(x),b);}}"));
    let pair = eval_function(&p, p.function_id("make").unwrap(), &[]).unwrap();
    let message = eval_function(&p, p.function_id("wrap").unwrap(), &[pair]).unwrap();
    assert_eq!(
        eval_function(&p, p.function_id("read").unwrap(), &[message]).unwrap(),
        int(513)
    );
    reject(
        "record A{x:u8;} record B{x:u8;} function f()->A body {yield record<B>(x:1);}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "record A{x:u8;} function f(x:A)->A body {yield add(x,x);}",
        "D_BAD_OPERAND",
    );
}
#[test]
fn complete_constructor_shape_and_type_names_are_checked() {
    for source in [
        "record Pair{a:u8;b:u16;} function f()->Pair body {yield record<Pair>(a:1);}",
        "record Pair{a:u8;} function f()->Pair body {yield record<Pair>();}",
    ] {
        reject(source, "D_RECORD_FIELDS");
    }
    reject(
        "record Pair{a:u8;} function f()->Pair body {yield record<Pair>(a:1,a:2);}",
        "D_DUPLICATE",
    );
    reject(
        "record Pair{a:u8;} function f()->Pair body {yield record<Pair>(b:1);}",
        "D_UNKNOWN_FIELD",
    );
    reject(
        "record Pair{a:u8;} function f(x:Pair)->u8 body {yield field(x,b);}",
        "D_UNKNOWN_FIELD",
    );
    reject(
        "union U{A:u8;} function f()->U body {yield variant<U,B>(1);}",
        "D_UNKNOWN_VARIANT",
    );
    reject(
        "record Pair{a:u8;} function f(x:Pair)->u8 body {yield payload<Pair,A>(x);}",
        "D_TYPE_MISMATCH",
    );
}
#[test]
fn finite_type_graphs_allow_forward_references_and_empty_records() {
    let p=checked("record Outer{inner:Later;} union Later{Unit:Empty;} record Empty{} function unit()->Later body {yield variant<Later,Unit>(record<Empty>());}");
    assert_eq!(
        p.value_wire_width(&ValueType::Named(p.type_id("Outer").unwrap()))
            .unwrap(),
        4
    );
    assert!(eval_function(&p, p.function_id("unit").unwrap(), &[]).is_ok());
    reject("record R{self:R;}", "D_TYPE_CYCLE");
    reject("record R{u:U;} union U{R:R;}", "D_TYPE_CYCLE");
    reject("union U{}", "D_EMPTY_UNION");
    reject("record R{missing:Unknown;}", "D_TYPE_MISMATCH");
}
#[test]
fn canonical_codec_has_stable_tags_and_rejects_inactive_payload_bytes() {
    let p = checked(TYPES);
    let ty = ValueType::Named(p.type_id("Message").unwrap());
    let value = value_from_json(&p, &serde_json::json!({"tag":"Small","payload":7}), &ty).unwrap();
    let bytes = encode_value(&p, &value, &ty).unwrap();
    assert_eq!(bytes, vec![0, 0, 0, 0, 7, 0, 0]);
    assert_eq!(decode_value(&p, &bytes, &ty).unwrap(), value);
    let mut invalid = bytes.clone();
    invalid[6] = 1;
    assert!(decode_value(&p, &invalid, &ty).is_err());
    invalid = bytes.clone();
    invalid[0] = 3;
    assert!(decode_value(&p, &invalid, &ty).is_err());
    assert!(decode_value(&p, &bytes[..6], &ty).is_err());
    let pair = value_from_json(
        &p,
        &serde_json::json!({"tag":"Pair","payload":{"a":7,"b":513}}),
        &ty,
    )
    .unwrap();
    assert_eq!(
        encode_value(&p, &pair, &ty).unwrap(),
        vec![1, 0, 0, 0, 7, 1, 2]
    );
    assert_eq!(
        value_to_json(&p, &pair, &ty),
        serde_json::json!({"tag":"Pair","payload":{"a":7,"b":513}})
    );
}
#[test]
fn decoder_rejects_shapes_and_scalar_domains_not_only_tags() {
    let p = checked(TYPES);
    let pair = ValueType::Named(p.type_id("Pair").unwrap());
    for json in [
        serde_json::json!({"a":1}),
        serde_json::json!({"a":256,"b":0}),
        serde_json::json!({"a":1,"b":0,"extra":2}),
    ] {
        assert!(value_from_json(&p, &json, &pair).is_err());
    }
    assert!(coerce_value(&p, &int(0), &pair).is_err());
    let bad = Value::Record {
        ty: p.type_id("Pair").unwrap(),
        fields: vec![int(1)],
    };
    assert!(coerce_value(&p, &bad, &pair).is_err());
    let scalar = ValueType::Scalar(ScalarType::U16);
    assert_eq!(encode_value(&p, &int(513), &scalar).unwrap(), vec![1, 2]);
}
#[test]
fn unit_and_large_products_do_not_use_a_scalar_bitvector_carrier() {
    let fields = (0..100)
        .map(|i| format!("f{i}:bytes32;"))
        .collect::<String>();
    let p = checked(&format!("record Large{{{fields}}} record Unit{{}}"));
    assert_eq!(
        p.value_wire_width(&ValueType::Named(p.type_id("Large").unwrap()))
            .unwrap(),
        3200
    );
    let empty = ValueType::Named(p.type_id("Unit").unwrap());
    let value = value_from_json(&p, &serde_json::json!({}), &empty).unwrap();
    assert_eq!(encode_value(&p, &value, &empty).unwrap(), Vec::<u8>::new());
    assert_eq!(decode_value(&p, &[], &empty).unwrap(), value);
    assert!(matches!(
        p.type_definition(p.type_id("Large").unwrap()).kind,
        TypeDefinitionKind::Record(_)
    ));
}
#[test]
fn repeated_type_references_share_layout_computation_and_fail_on_real_overflow() {
    let mut source = "record T0{x:u8;}".to_string();
    for i in 1..40 {
        source.push_str(&format!("record T{i}{{a:T{};b:T{};}}", i - 1, i - 1));
    }
    let p = checked(&source);
    assert_eq!(
        p.value_wire_width(&ValueType::Named(p.type_id("T39").unwrap()))
            .unwrap(),
        1usize << 39
    );
    for i in 40..70 {
        source.push_str(&format!("record T{i}{{a:T{};b:T{};}}", i - 1, i - 1));
    }
    reject(&source, "D_TYPE_REPRESENTATION");
}

#[test]
fn nominal_mismatch_diagnostics_use_source_names_and_type_definitions_affect_identity() {
    let source="record Pair{x:u8;} record Money{x:u8;} function f()->Pair body {yield record<Money>(x:1);}";
    let errors =
        vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap_err();
    assert!(errors[0].message.contains("Pair") && errors[0].message.contains("Money"));
    assert!(!errors[0].message.contains("type#"));
    let base = vera_compiler::parse_source("record Pair{x:u8;y:u16;}")
        .unwrap()
        .canonical_json();
    for changed in [
        "record Pair{x:u16;y:u16;}",
        "record Pair{y:u16;x:u8;}",
        "record Money{x:u8;y:u16;}",
    ] {
        assert_ne!(
            base,
            vera_compiler::parse_source(changed)
                .unwrap()
                .canonical_json()
        );
    }
}

#[test]
fn type_layout_dag_does_not_expand_repeated_zero_width_values() {
    let mut source = "record T0{}".to_string();
    for i in 1..1000 {
        source.push_str(&format!("record T{i}{{a:T{};b:T{};}}", i - 1, i - 1));
    }
    let p = checked(&source);
    let ty = ValueType::Named(p.type_id("T999").unwrap());
    assert_eq!(p.value_wire_width(&ty).unwrap(), 0);
    assert_eq!(p.value_storage_width(&ty).unwrap(), 0);
}
