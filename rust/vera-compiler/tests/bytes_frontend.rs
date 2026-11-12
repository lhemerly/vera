use num_bigint::BigInt;
use vera_compiler::{
    ir::CheckedProgram,
    runtime::{eval_function, FailureKind},
    types::{ExprType, ValueType},
    value::{decode_value, encode_value, value_from_json, value_to_json, Value},
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
#[test]
fn capacities_are_explicit_types_and_legacy_ast_omits_new_metadata() {
    let p=checked("effect E(a:bytes<0>,b:bytes<1048576>); function empty()->bytes<0> body {yield hex<0>(\"\");}");
    assert_eq!(
        p.effects()[0].params[0].ty,
        ValueType::Bytes { capacity: 0 }
    );
    assert_eq!(
        p.value_wire_width(&p.effects()[0].params[1].ty).unwrap(),
        1048584
    );
    assert_eq!(
        eval_function(&p, p.function_id("empty").unwrap(), &[]).unwrap(),
        Value::Bytes(vec![])
    );
    let old = vera_compiler::parse_source("effect E(x:u8);")
        .unwrap()
        .canonical_json();
    assert!(old["decls"][0]["params"][0]["type"]
        .get("capacity")
        .is_none());
    let new = p.syntax().canonical_json();
    assert!(new["decls"][0]["params"][0]["type"]
        .get("capacity")
        .is_some());
}
#[test]
fn hex_literals_preserve_leading_zeroes_and_check_capacity() {
    let p = checked("function value()->bytes<4> body {yield hex<4>(\"0000aAFF\");}");
    assert_eq!(
        eval_function(&p, p.function_id("value").unwrap(), &[]).unwrap(),
        Value::Bytes(vec![0, 0, 170, 255])
    );
    for hex in ["0", "gg", "éé", "0000000000"] {
        reject(
            &format!("function f()->bytes<4> body {{yield hex<4>(\"{hex}\");}}"),
            "D_LITERAL_RANGE",
        );
    }
    reject(
        "function f()->bytes<1> body {yield hex<2>(\"00\");}",
        "D_TYPE_MISMATCH",
    );
}
#[test]
fn byte_intrinsics_have_precise_argument_and_result_types() {
    let p=checked("function take(x:bytes<8>)->bytes<2> body {yield bytes_slice<2>(x,1,2);} function get(x:bytes<8>,i:u64)->u8 body {yield bytes_get(x,i);} function length(x:bytes<8>)->u64 body {yield bytes_len(x);}");
    let arg = Value::Bytes(vec![0, 1, 2, 3]);
    assert_eq!(
        eval_function(
            &p,
            p.function_id("take").unwrap(),
            std::slice::from_ref(&arg)
        )
        .unwrap(),
        Value::Bytes(vec![1, 2])
    );
    assert_eq!(
        eval_function(&p, p.function_id("get").unwrap(), &[arg.clone(), int(3)]).unwrap(),
        int(3)
    );
    assert_eq!(
        eval_function(&p, p.function_id("length").unwrap(), &[arg]).unwrap(),
        int(4)
    );
    reject(
        "function f()->u64 body {yield bytes_len(1);}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "function f(x:bytes<2>)->u8 body {yield bytes_get(x,true);}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "function f(x:bytes<2>)->u8 body {yield bytes_get(x);}",
        "D_ARITY",
    );
}
#[test]
fn bounds_and_capacity_failures_are_traps_and_zero_length_slice_is_valid() {
    let p=checked("function slice(x:bytes<4>,s:u64,n:u64)->bytes<2> body {yield bytes_slice<2>(x,s,n);} function shrink(x:bytes<4>)->bytes<2> body {yield bytes_resize<2>(x);} function join(x:bytes<4>,y:bytes<4>)->bytes<2> body {yield bytes_concat<2>(x,y);}");
    let f = p.function_id("slice").unwrap();
    assert_eq!(
        eval_function(&p, f, &[Value::Bytes(vec![1, 2]), int(2), int(0)]).unwrap(),
        Value::Bytes(vec![])
    );
    for (start, count) in [(3, 0), (1, 2), (u64::MAX, 1)] {
        let e =
            eval_function(&p, f, &[Value::Bytes(vec![1, 2]), int(start), int(count)]).unwrap_err();
        assert_eq!(e.kind, FailureKind::Trap);
        assert_eq!(e.code, "ERR_BYTES_BOUNDS");
    }
    for (name, args) in [
        ("shrink", vec![Value::Bytes(vec![1, 2, 3])]),
        (
            "join",
            vec![Value::Bytes(vec![1, 2]), Value::Bytes(vec![3])],
        ),
    ] {
        assert_eq!(
            eval_function(&p, p.function_id(name).unwrap(), &args)
                .unwrap_err()
                .code,
            "ERR_BYTES_CAPACITY"
        );
    }
}
#[test]
fn canonical_byte_envelope_rejects_lengths_padding_and_extra_bytes() {
    let p = checked("");
    let ty = ValueType::Bytes { capacity: 4 };
    let v = Value::Bytes(vec![0, 255]);
    let b = encode_value(&p, &v, &ty).unwrap();
    assert_eq!(b, vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0]);
    assert_eq!(decode_value(&p, &b, &ty).unwrap(), v);
    let mut bad = b.clone();
    bad[0] = 5;
    assert!(decode_value(&p, &bad, &ty).is_err());
    bad = b.clone();
    bad[11] = 1;
    assert!(decode_value(&p, &bad, &ty).is_err());
    bad = b.clone();
    bad.push(0);
    assert!(decode_value(&p, &bad, &ty).is_err());
    assert!(decode_value(&p, &b[..11], &ty).is_err());
    let empty = ValueType::Bytes { capacity: 0 };
    assert_eq!(
        encode_value(&p, &Value::Bytes(vec![]), &empty).unwrap(),
        vec![0; 8]
    );
}
#[test]
fn json_is_exact_byte_hex_not_utf8_text() {
    let p = checked("");
    let ty = ValueType::Bytes { capacity: 4 };
    let v = value_from_json(&p, &serde_json::json!("0x00FF"), &ty).unwrap();
    assert_eq!(v, Value::Bytes(vec![0, 255]));
    assert_eq!(value_to_json(&p, &v, &ty), serde_json::json!("0x00ff"));
    for text in ["hello", "00ff", "0x0", "0xgg", "0x0001020304"] {
        assert!(value_from_json(&p, &serde_json::json!(text), &ty).is_err());
    }
}
#[test]
fn codecs_reuse_typed_wire_forms_and_nested_byte_types() {
    let p=checked("record Packet{kind:u16;data:bytes<4>;} function round(x:Packet)->Packet body {yield decode<Packet>(encode<Packet>(x));} function raw(x:bytes<4>)->bytes<12> body {yield encode<bytes<4>>(x);} function restore(x:bytes<12>)->bytes<4> body {yield decode<bytes<4>>(x);}");
    let ty = ValueType::Named(p.type_id("Packet").unwrap());
    let value = value_from_json(&p, &serde_json::json!({"kind":513,"data":"0x00ff"}), &ty).unwrap();
    assert_eq!(
        eval_function(
            &p,
            p.function_id("round").unwrap(),
            std::slice::from_ref(&value)
        )
        .unwrap(),
        value
    );
    let arg = Value::Bytes(vec![170]);
    let encoded = eval_function(
        &p,
        p.function_id("raw").unwrap(),
        std::slice::from_ref(&arg),
    )
    .unwrap();
    assert_eq!(
        encoded,
        Value::Bytes(vec![1, 0, 0, 0, 0, 0, 0, 0, 170, 0, 0, 0])
    );
    assert_eq!(
        eval_function(&p, p.function_id("restore").unwrap(), &[encoded]).unwrap(),
        arg
    );
    assert!(matches!(
        p.expr(p.function(p.function_id("raw").unwrap()).body)
            .domain,
        ExprType::Bytes { capacity: 12 }
    ));
}
#[test]
fn malformed_decode_is_partial_but_can_decode_and_lazy_guard_are_total() {
    let p=checked("function valid(x:bytes<2>)->bool body {yield can_decode<bool>(x);} function read(x:bytes<2>)->bool body {yield decode<bool>(x);} function safe(x:bytes<2>)->bool body {yield select(can_decode<bool>(x),decode<bool>(x),false);}");
    for data in [vec![], vec![2], vec![0, 0]] {
        let arg = Value::Bytes(data);
        assert_eq!(
            eval_function(
                &p,
                p.function_id("valid").unwrap(),
                std::slice::from_ref(&arg)
            )
            .unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_function(
                &p,
                p.function_id("read").unwrap(),
                std::slice::from_ref(&arg)
            )
            .unwrap_err()
            .code,
            "ERR_DECODE"
        );
        assert_eq!(
            eval_function(&p, p.function_id("safe").unwrap(), &[arg]).unwrap(),
            Value::Bool(false)
        );
    }
}
#[test]
fn capacities_are_not_policy_limited_and_physical_overflow_is_explicit() {
    checked("effect E(x:bytes<4294967296>);");
    reject(
        "effect E(x:bytes<18446744073709551615>);",
        "D_TYPE_REPRESENTATION",
    );
    reject(
        "effect E(x:bytes<18446744073709551616>);",
        "D_BYTES_CAPACITY",
    );
    reject(
        "table T capacity 1 {key id:id64<T>;col b:bytes<4>[tracked_sum];}",
        "D_BAD_OPERAND",
    );
}
