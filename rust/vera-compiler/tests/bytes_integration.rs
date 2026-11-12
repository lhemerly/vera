//! Bounded-byte contracts compose without enumerating contents or capacities.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_all, verify_transition, VerifierConfig},
};
fn checked(s: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(s).unwrap_or_else(|e| panic!("{e:?}\n{s}")))
        .unwrap_or_else(|e| panic!("{e:?}\n{s}"))
}
fn accepted(s: &str) {
    let p = checked(s);
    for r in verify_all(&p, &VerifierConfig::default()) {
        assert!(r.ok(), "{}", r.summary());
    }
}
#[test]
fn byte_contracts_compose_through_helpers_and_effects() {
    accepted("function same(x:bytes<4>)->bytes<4> ensure exact:eq(result,x);body {yield x;}effect E(value:bytes<4>);transition go(x:bytes<4>) reads [] writes [] ensure exact:eq(effect_arg(E,0,value),x);body {emit E(same(x));commit;}");
}
#[test]
fn scalar_record_union_and_byte_round_trips_follow_actual_codec_definitions() {
    for (d, t) in [
        ("", "u64"),
        ("record R {n:u8;flag:bool;}", "R"),
        ("union U {Small:u8;Large:u64;}", "U"),
        ("", "bytes<4>"),
    ] {
        accepted(&format!("{d}function roundtrip(x:{t})->{t} ensure exact:eq(result,x);body {{yield decode<{t}>(encode<{t}>(x));}}"));
    }
}
#[test]
fn malformed_inputs_are_not_assumed_away() {
    accepted("transition go() reads [] writes [] ensure b:eq(can_decode<bool>(hex<1>(\"02\")),false);ensure n:eq(can_decode<u8>(hex<2>(\"0000\")),false);body {commit;}");
    let p=checked("transition go() reads [] writes [] body {let %x:bool=decode<bool>(hex<1>(\"02\"));commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    assert_eq!(
        r.counterexample.unwrap().failure.unwrap().code,
        "ERR_DECODE"
    );
}
#[test]
fn byte_bounds_are_caller_obligations_with_real_replay_errors() {
    accepted("function at(x:bytes<8>,i:u64)->u8 require inside:lt(i,bytes_len(x));body {yield bytes_get(x,i);}");
    let p=checked("transition go(x:bytes<8>,i:u64) reads [] writes [] body {let %b:u8=bytes_get(x,i);commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    assert_eq!(
        r.counterexample.unwrap().failure.unwrap().code,
        "ERR_BYTES_BOUNDS"
    );
}
#[test]
fn zero_capacity_and_lazy_decode_preserve_semantics() {
    accepted("function empty()->bytes<0> ensure zero:eq(bytes_len(result),0);body {yield bytes_concat<0>(hex<0>(\"\"),bytes_slice<0>(hex<1>(\"01\"),1,0));}transition go() reads [] writes [] ensure lazy:select(can_decode<bool>(hex<1>(\"02\")),decode<bool>(hex<1>(\"02\")),true);body {let %x:bytes<0>=empty();let %w:bytes<8>=bytes_resize<8>(%x);commit;}");
}
#[test]
fn capacity_is_static_and_resize_never_truncates() {
    let p=checked("transition go() reads [] writes [] body {let %x:bytes<1>=bytes_resize<1>(hex<2>(\"0001\"));commit;}");
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    assert_eq!(
        r.counterexample.unwrap().failure.unwrap().code,
        "ERR_BYTES_CAPACITY"
    );
    assert!(
        analyze(parse_source("function wrong(x:bytes<2>)->bytes<4> body {yield x;}").unwrap())
            .is_err()
    );
}
#[test]
fn huge_capacity_codec_formulas_do_not_expand_per_byte() {
    let mut sizes = Vec::new();
    for capacity in [8, 1024, 1_000_000_000u64] {
        let s=format!("transition go() reads [] writes [] ensure short:eq(can_decode<bytes<{capacity}>>(hex<0>(\"\")),false);body {{commit;}}");
        let p = checked(&s);
        let r = verify_transition(&p, "go", &VerifierConfig::default());
        assert!(r.ok(), "{}", r.summary());
        assert!(r.smt_safety.len() < 150_000, "{}", r.smt_safety.len());
        sizes.push(r.smt_safety.len());
    }
    assert!(sizes[2] < sizes[1] * 4, "{sizes:?}");
}
