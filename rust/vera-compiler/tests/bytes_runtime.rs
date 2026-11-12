//! Bounded logical byte sequences and strict source-level canonical codecs.
use num_bigint::BigInt;
use serde_json::json;
use std::collections::BTreeMap;
use vera_compiler::{
    ir::CheckedProgram,
    parse_source,
    persistence::{decode_state, encode_state, schema_fingerprint, Intent},
    runtime::{eval_function, recompute_aggregates, FailureKind, Machine, State},
    semantic::analyze,
    types::ValueType,
    value::{decode_value, encode_value, value_from_json, value_to_json, Value},
};
fn checked(s: &str) -> CheckedProgram {
    analyze(parse_source(s).unwrap()).unwrap()
}
fn call(
    p: &CheckedProgram,
    name: &str,
    args: &[Value],
) -> Result<Value, vera_compiler::runtime::RuntimeError> {
    eval_function(p, p.function_id(name).unwrap(), args)
}
fn bytes(v: &[u8]) -> Value {
    Value::Bytes(v.to_vec())
}
fn int(v: u64) -> Value {
    Value::Int(BigInt::from(v))
}

#[test]
fn byte_operations_preserve_logical_length_and_do_not_truncate() {
    let p=checked("function len(v:bytes<8>)->u64 body {yield bytes_len(v);} function get(v:bytes<8>,i:u64)->u8 body {yield bytes_get(v,i);} function take(v:bytes<8>)->bytes<2> body {yield bytes_slice<2>(v,1,2);} function join(a:bytes<4>,b:bytes<4>)->bytes<8> body {yield bytes_concat<8>(a,b);} function shrink(v:bytes<8>)->bytes<2> body {yield bytes_resize<2>(v);}");
    assert_eq!(call(&p, "len", &[bytes(&[1, 0, 2])]).unwrap(), int(3));
    assert_eq!(
        call(&p, "get", &[bytes(&[1, 0, 2]), int(1)]).unwrap(),
        int(0)
    );
    assert_eq!(
        call(&p, "take", &[bytes(&[1, 0, 2, 9])]).unwrap(),
        bytes(&[0, 2])
    );
    assert_eq!(
        call(&p, "join", &[bytes(&[1, 0]), bytes(&[2, 3])]).unwrap(),
        bytes(&[1, 0, 2, 3])
    );
    assert_eq!(
        call(&p, "shrink", &[bytes(&[1, 0])]).unwrap(),
        bytes(&[1, 0])
    );
    assert_eq!(
        call(&p, "shrink", &[bytes(&[1, 0, 2])]).unwrap_err().code,
        "ERR_BYTES_CAPACITY"
    );
    assert_eq!(
        call(&p, "get", &[bytes(&[1]), int(u64::MAX)])
            .unwrap_err()
            .code,
        "ERR_BYTES_BOUNDS"
    );
    assert_eq!(
        call(&p, "take", &[bytes(&[1, 2])]).unwrap_err().code,
        "ERR_BYTES_BOUNDS"
    );
}

#[test]
fn zero_capacity_empty_slices_and_canonical_empty_encoding_are_valid() {
    let p=checked("function empty()->bytes<0> body {yield hex<0>(\"\");} function tail(v:bytes<2>)->bytes<0> body {yield bytes_slice<0>(v,bytes_len(v),0);} function wire()->bytes<8> body {yield encode<bytes<0>>(hex<0>(\"\"));}");
    assert_eq!(call(&p, "empty", &[]).unwrap(), bytes(&[]));
    assert_eq!(call(&p, "tail", &[bytes(&[0, 1])]).unwrap(), bytes(&[]));
    assert_eq!(call(&p, "wire", &[]).unwrap(), bytes(&[0; 8]));
}

#[test]
fn malformed_codecs_are_total_for_can_decode_and_trap_for_decode() {
    let p=checked("function valid(d:bytes<2>)->bool body {yield can_decode<bool>(d);} function read(d:bytes<2>)->bool body {yield decode<bool>(d);} function safe(d:bytes<2>)->bool body {yield select(can_decode<bool>(d),decode<bool>(d),false);}");
    for bad in [&[][..], &[2][..], &[1, 0][..]] {
        assert_eq!(
            call(&p, "valid", &[bytes(bad)]).unwrap(),
            Value::Bool(false)
        );
        let e = call(&p, "read", &[bytes(bad)]).unwrap_err();
        assert_eq!(e.code, "ERR_DECODE");
        assert_eq!(e.kind, FailureKind::Trap);
        assert_eq!(call(&p, "safe", &[bytes(bad)]).unwrap(), Value::Bool(false));
    }
    assert_eq!(call(&p, "read", &[bytes(&[1])]).unwrap(), Value::Bool(true));
}

#[test]
fn source_codecs_round_trip_records_with_bytes_and_reject_noncanonical_padding() {
    let p=checked("record Packet {data:bytes<4>;flag:bool;} function wire(p:Packet)->bytes<13> body {yield encode<Packet>(p);} function read(d:bytes<13>)->Packet body {yield decode<Packet>(d);} function valid(d:bytes<13>)->bool body {yield can_decode<Packet>(d);}");
    let ty = ValueType::Named(p.type_id("Packet").unwrap());
    let value = value_from_json(&p, &json!({"data":"0xab","flag":true}), &ty).unwrap();
    let wire = call(&p, "wire", std::slice::from_ref(&value)).unwrap();
    let expected = vec![1, 0, 0, 0, 0, 0, 0, 0, 0xab, 0, 0, 0, 1];
    assert_eq!(wire, Value::Bytes(expected.clone()));
    assert_eq!(
        call(&p, "read", std::slice::from_ref(&wire)).unwrap(),
        value
    );
    for (offset, replacement) in [(0, 5), (9, 1), (12, 2)] {
        let mut bad = expected.clone();
        bad[offset] = replacement;
        assert_eq!(
            call(&p, "valid", &[Value::Bytes(bad.clone())]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            call(&p, "read", &[Value::Bytes(bad)]).unwrap_err().code,
            "ERR_DECODE"
        );
    }
}

#[test]
fn byte_json_and_wire_admission_validate_capacity_length_and_zero_padding() {
    let p = checked("");
    let ty = ValueType::Bytes { capacity: 4 };
    let value = value_from_json(&p, &json!("0x00aB"), &ty).unwrap();
    assert_eq!(value, bytes(&[0, 0xab]));
    assert_eq!(value_to_json(&p, &value, &ty), json!("0x00ab"));
    for bad in [
        json!("0x0"),
        json!("0xzz"),
        json!("0x0001020304"),
        json!([0, 1]),
    ] {
        assert!(value_from_json(&p, &bad, &ty).is_err());
    }
    let wire = encode_value(&p, &value, &ty).unwrap();
    assert_eq!(wire, vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 0xab, 0, 0]);
    assert_eq!(decode_value(&p, &wire, &ty).unwrap(), value);
    let mut bad = wire.clone();
    bad[11] = 9;
    assert!(decode_value(&p, &bad, &ty).is_err());
    bad = wire.clone();
    bad[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(decode_value(&p, &bad, &ty).is_err());
    assert!(decode_value(&p, &wire[..11], &ty).is_err());
}

#[test]
fn wide_aggregate_indices_are_rejected_without_u64_truncation() {
    let p=checked("table T capacity 2 {key id:id64<T>;col n:u64 [tracked_sum];} transition go() reads [T[*].n] writes [] body {let %x:u8=bytes_get(hex<1>(\"01\"),add(sum(T.n),2));commit;}");
    let mut state = State::empty(&p).unwrap();
    state.tables[0].occupied.fill(true);
    state.tables[0].generations.fill(1);
    state.tables[0].columns[0].fill(int(u64::MAX));
    recompute_aggregates(&p, &mut state).unwrap();
    let before = state.clone();
    let mut m = Machine::with_state(&p, state).unwrap();
    assert_eq!(
        m.call("go", &BTreeMap::new()).unwrap_err().code,
        "ERR_BYTES_BOUNDS"
    );
    assert_eq!(m.state, before);
}

#[test]
fn byte_capacity_failure_rolls_back_effects_and_concat_evaluates_both_arguments() {
    let p=checked("effect Mark(n:u8);transition go() reads [] writes [] body {let %v:bytes<1>=bytes_concat<1>(fold_bounded<1>(a:bytes<2>=hex<2>(\"\"),i in [0..0]) {emit Mark(1);yield hex<2>(\"0011\");},fold_bounded<1>(a:bytes<1>=hex<1>(\"\"),i in [0..0]) {emit Mark(2);yield hex<1>(\"22\");});commit;}");
    let mut m = Machine::new(&p).unwrap();
    let before = m.state.clone();
    let e = m.execute("go", &BTreeMap::new(), true).unwrap_err();
    assert_eq!(e.code, "ERR_BYTES_CAPACITY");
    assert_eq!(m.state, before);
    let effects = m
        .last_trace
        .iter()
        .filter(|s| s.kind == "effect")
        .collect::<Vec<_>>();
    assert_eq!(effects.len(), 2);
    assert!(effects[0].text.contains('1'));
    assert!(effects[1].text.contains('2'));
}

#[test]
fn bytes_in_unique_columns_effects_intents_and_snapshots_keep_exact_data() {
    let p=checked("table T capacity 2 {key id:id64<T>;col data:bytes<4> [unique];} effect Seen(d:bytes<4>);transition create(d:bytes<4>) reads [] writes [T[*].data] body {let %k:id64<T>=table_alloc(T,{data:d});emit Seen(d);commit;}");
    let args = BTreeMap::from([("d".into(), bytes(&[1, 0]))]);
    let intent = Intent::from_parameters(&p, "create", &args).unwrap();
    let (tag, wire) = intent.encode(&p).unwrap();
    assert_eq!(Intent::decode(&p, tag, &wire).unwrap(), intent);
    let mut m = Machine::new(&p).unwrap();
    m.call("create", &args).unwrap();
    let before = m.state.clone();
    assert_eq!(
        m.call("create", &args).unwrap_err().code,
        "ERR_DUPLICATE_KEY"
    );
    assert_eq!(m.state, before);
    assert_eq!(
        decode_state(&p, &encode_state(&p, &m.state).unwrap()).unwrap(),
        m.state
    );
    let q=checked("table T capacity 2 {key id:id64<T>;col data:bytes<5> [unique];} effect Seen(d:bytes<5>);transition create(d:bytes<5>) reads [] writes [T[*].data] body {let %k:id64<T>=table_alloc(T,{data:d});emit Seen(d);commit;}");
    assert_ne!(schema_fingerprint(&p), schema_fingerprint(&q));
}

#[test]
fn cli_accepts_logical_hex_and_fuzzer_can_sample_full_capacity_literals() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let source="effect Seen(d:bytes<4>);transition go(d:bytes<4>) reads [] writes [] body {emit Seen(d);commit;}";
    let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args([
            "run",
            "-",
            "--unverified",
            "--json",
            "--transition",
            "go",
            "--param",
            "d=0x00aB",
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
    assert!(String::from_utf8_lossy(&result.stdout).contains("0x00ab"));
    let source=format!("transition go(d:bytes<64>) reads [] writes [] ensure short:le(bytes_len(d),2);body {{let %example:bytes<64>=hex<64>(\"{}\");commit;}}","aa".repeat(64));
    let p = checked(&source);
    let result = vera_compiler::fuzz::fuzz_module(
        &p,
        &vera_compiler::fuzz::FuzzConfig {
            steps: 64,
            threads: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!result.ok);
    assert!(result.violations.iter().any(|v| v
        .trace
        .iter()
        .any(|t| matches!(&t.params["d"],Value::Bytes(b) if b.len()==64))));
}
