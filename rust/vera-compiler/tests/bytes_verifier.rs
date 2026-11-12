//! Sequence-based bounded bytes and definitional canonical codecs.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    value::Value,
    verifier::{verify_all, verify_transition, VerifierConfig},
};
fn program(s: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(s).unwrap()).unwrap_or_else(|e| panic!("{e:?}\n{s}"))
}
fn verify(s: &str) -> vera_compiler::verifier::VerificationResult {
    verify_transition(
        &program(s),
        "go",
        &VerifierConfig {
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    )
}
fn accepted(s: &str) {
    let r = verify(s);
    assert!(r.ok(), "{}\n{}", r.summary(), r.smt_safety);
    assert!(r.witness.is_some());
}
#[test]
fn sequence_inputs_preserve_logical_equality_lengths_and_model_bytes() {
    accepted("transition go(x:bytes<8>) reads [] writes [] ensure length:le(bytes_len(x),8);ensure resize:eq(bytes_resize<8>(x),x);ensure slice:eq(bytes_slice<8>(x,0,bytes_len(x)),x);body {commit;}");
    let r=verify("transition go(x:bytes<3>) reads [] writes [] require len:eq(bytes_len(x),2);require first:eq(bytes_get(x,0),255);ensure wrong:eq(bytes_get(x,1),0);body {commit;}");
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
    let Value::Bytes(bytes) = &r.counterexample.unwrap().params["x"] else {
        panic!("missing sequence")
    };
    assert_eq!(bytes.len(), 2);
    assert_eq!(bytes[0], 255);
    assert_ne!(bytes[1], 0);
}
#[test]
fn partial_bytes_access_and_capacity_failures_replay() {
    for (body, error) in [
        ("let %b:u8=bytes_get(hex<0>(\"\"),0);", "ERR_BYTES_BOUNDS"),
        (
            "let %b:bytes<1>=bytes_concat<1>(hex<1>(\"ff\"),hex<1>(\"00\"));",
            "ERR_BYTES_CAPACITY",
        ),
        (
            "let %b:bytes<0>=bytes_resize<0>(hex<1>(\"ff\"));",
            "ERR_BYTES_CAPACITY",
        ),
        (
            "let %b:bytes<1>=bytes_slice<1>(hex<1>(\"ff\"),1,1);",
            "ERR_BYTES_BOUNDS",
        ),
    ] {
        let r = verify(&format!(
            "transition go() reads [] writes [] body {{{body}commit;}}"
        ));
        assert_eq!(r.panic.status, "sat", "{}", r.summary());
        assert_eq!(r.counterexample.unwrap().failure.unwrap().code, error);
    }
}
#[test]
fn scalar_record_union_and_bytes_roundtrips_are_proved_from_codec_definitions() {
    for (defs, ty) in [
        ("", "u16"),
        ("", "bool"),
        ("", "enum {A,B,C}"),
        ("", "bytes<3>"),
        ("record R {n:u16;flag:bool;data:bytes<2>;}", "R"),
        ("record R {n:u8;flag:bool;} union U {Data:R;Other:u8;}", "U"),
        (
            "record Packet {kind:u8;content:bytes<4>;} union Message {Data:Packet;Empty:bool;}",
            "Message",
        ),
    ] {
        accepted(&format!("{defs} transition go(x:{ty}) reads [] writes [] ensure valid:can_decode<{ty}>(encode<{ty}>(x));ensure roundtrip:eq(decode<{ty}>(encode<{ty}>(x)),x);body {{commit;}}"));
    }
}
#[test]
fn malformed_codecs_are_total_false_and_decode_traps() {
    for (defs, ty, hex) in [
        ("", "bool", "02"),
        ("", "u16", "01"),
        ("", "enum {A,B}", "02"),
        ("", "bytes<2>", "03000000000000000000"),
        ("", "bytes<2>", "0100000000000000ff01"),
        ("union U {A:u8;B:u16;}", "U", "020000000000"),
        ("union U {A:u8;B:u16;}", "U", "00000000ff01"),
    ] {
        let n = hex.len() / 2;
        accepted(&format!("{defs} transition go() reads [] writes [] ensure malformed:not(can_decode<{ty}>(hex<{n}>(\"{hex}\")));body {{commit;}}"));
        let r=verify(&format!("{defs} transition go() reads [] writes [] body {{let %bad:{ty}=decode<{ty}>(hex<{n}>(\"{hex}\"));commit;}}"));
        assert_eq!(r.panic.status, "sat", "{}", r.summary());
        assert_eq!(
            r.counterexample.unwrap().failure.unwrap().code,
            "ERR_DECODE"
        );
    }
}
#[test]
fn codec_canonical_vectors_and_empty_bytes_are_exact() {
    accepted("transition go(x:bytes<0>) reads [] writes [] ensure empty:eq(x,hex<0>(\"\"));ensure enc:eq(encode<bytes<0>>(x),hex<8>(\"0000000000000000\"));ensure scalar:eq(encode<u16>(513),hex<2>(\"0102\"));ensure decode:eq(decode<u16>(hex<2>(\"0102\")),513);body {commit;}");
    accepted("record R {n:u16;flag:bool;} union U {Small:bool;Big:R;} transition go() reads [] writes [] ensure exact:eq(encode<U>(variant<U,Small>(true)),hex<7>(\"00000000010000\"));body {commit;}");
}
#[test]
fn reusable_byte_function_contract_and_effect_payloads_use_sequence_sorts() {
    let p=program("function same(x:bytes<4>)->bytes<4> ensure exact:eq(result,x);body {yield x;} effect E(data:bytes<4>); transition go(x:bytes<4>) reads [] writes [] ensure exact:eq(effect_arg(E,0,data),x);body {emit E(same(x));commit;}");
    for r in verify_all(
        &p,
        &VerifierConfig {
            timeout_ms: Some(10_000),
            ..Default::default()
        },
    ) {
        assert!(r.ok(), "{}", r.summary());
    }
}
#[test]
fn full_aggregate_indices_and_encode_narrowing_do_not_wrap() {
    let base="table T capacity 2 {key id:id64<T>;col n:u64 [tracked_sum];} transition go() reads [T[*].n] writes [] require big:gt(sum(T.n),18446744073709551615);";
    for (body, error) in [
        (
            "let %b:u8=bytes_get(hex<1>(\"ff\"),sum(T.n));",
            "ERR_BYTES_BOUNDS",
        ),
        (
            "let %b:bytes<1>=bytes_slice<1>(hex<1>(\"ff\"),0,sum(T.n));",
            "ERR_BYTES_BOUNDS",
        ),
    ] {
        let r = verify(&format!("{base} body {{{body}commit;}}"));
        assert_eq!(r.panic.status, "sat", "{}", r.summary());
        assert_eq!(r.counterexample.unwrap().failure.unwrap().code, error);
    }
    let r = verify(&format!(
        "{base} body {{let %b:bytes<8>=encode<u64>(sum(T.n));commit;}}"
    ));
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    assert!(r.counterexample.unwrap().failure.is_some());
    accepted("table T capacity 1 {key id:id64<T>;col n:u64 [tracked_sum];} transition go(b:bytes<2>) reads [T[*].n] writes [] require n:gt(bytes_len(b),0);ensure length:eq(add(sum(T.n),bytes_len(b)),add(sum(T.n),bytes_len(b)));body {commit;}");
}
#[test]
fn byte_table_values_and_structured_counterexamples_preserve_canonical_json() {
    accepted("table T capacity 2 {key id:id64<T>;col b:bytes<4>;} transition go(k:id64<T>,b:bytes<4>) reads [T[k].b] writes [T[k].b] require live:table_exists(T,k);ensure saved:eq(T[k].b,b);body {table_set(T,k,b,b);commit;}");
    let r=verify("record R {b:bytes<2>;} transition go(r:R) reads [] writes [] require len:eq(bytes_len(field(r,b)),1);ensure wrong:eq(bytes_get(field(r,b),0),0);body {commit;}");
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
    let Value::Record { fields, .. } = &r.counterexample.unwrap().params["r"] else {
        panic!("record")
    };
    let Value::Bytes(b) = &fields[0] else {
        panic!("bytes")
    };
    assert_eq!(b.len(), 1);
    assert_ne!(b[0], 0);
}
#[test]
fn shared_codec_schema_and_padding_relations_do_not_expand_with_capacity() {
    let mut prev = 0;
    for capacity in [4, 256, 65536] {
        // Impossible entry avoids constructing a giant runtime output witness.
        let r=verify(&format!("record R {{a:bytes<{capacity}>;b:bytes<{capacity}>;}} transition go(r:R) reads [] writes [] require none:false;ensure schema:can_decode<R>(encode<R>(r));body {{commit;}}"));
        assert_eq!(r.liveness.status, "unsat", "{}", r.summary());
        assert_eq!(r.smt_safety.matches("(define-fun encode_").count(), 2);
        assert_eq!(r.smt_safety.matches("(define-fun decode_valid_").count(), 2);
        assert_eq!(r.smt_safety.matches("(declare-fun zero_pad ").count(), 1);
        assert_eq!(
            r.smt_safety.matches("(define-fun padding_parts_").count(),
            2
        );
        let size = r.smt_safety.len();
        if prev > 0 {
            assert!(
                size < prev * 2,
                "padding expanded with capacity: {size} vs {prev}"
            );
        }
        prev = size;
    }
}
#[test]
fn malformed_symbolic_input_is_not_assumed_valid_and_canonical_decode_reencodes() {
    let r=verify("transition go(b:bytes<1>) reads [] writes [] require exact:eq(b,hex<1>(\"02\"));ensure wrong:can_decode<bool>(b);body {commit;}");
    assert_eq!(r.safety.status, "sat", "{}", r.summary());
    assert_eq!(r.counterexample.unwrap().params["b"], Value::Bytes(vec![2]));
    accepted("union U {A:bool;B:u16;} transition go(b:bytes<6>) reads [] writes [] require valid:can_decode<U>(b);ensure canonical:eq(encode<U>(decode<U>(b)),b);body {commit;}");
    accepted("transition go(b:bytes<11>) reads [] writes [] require valid:can_decode<bytes<3>>(b);ensure canonical:eq(encode<bytes<3>>(decode<bytes<3>>(b)),b);body {commit;}");
    accepted("table T capacity 1 {key id:id64<T>;col n:u8 [tracked_sum];} transition go(b:bytes<2>) reads [T[*].n] writes [] require n:gt(bytes_len(b),0);ensure wide:eq(add(sum(T.n),bytes_get(b,0)),add(sum(T.n),bytes_get(b,0)));body {commit;}");
}
#[test]
fn unused_huge_capacity_is_not_enumerated_or_silently_reduced() {
    accepted("transition go(b:bytes<1000000000000>) reads [] writes [] ensure bound:le(bytes_len(b),1000000000000);body {commit;}");
}
#[test]
fn decoder_only_billion_byte_capacity_does_not_materialize_encoder_padding() {
    let p=program("record R {data:bytes<1000000000>;} union U {Data:R;Empty:bool;} transition go() reads [] writes [] ensure short:not(can_decode<U>(hex<0>(\"\")));body {commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
    assert!(!r.smt_safety.contains("zero_pad"));
    assert!(!r.smt_safety.contains("(define-fun encode_"));
    assert!(r.smt_safety.contains("seq.in.re"));
    assert!(r.smt_safety.len() < 20_000);
}
