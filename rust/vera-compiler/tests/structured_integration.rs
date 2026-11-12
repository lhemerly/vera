//! Cross-layer proof regressions for general product/sum values, not packed scalars.
use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_all, verify_transition, VerifierConfig},
};
fn checked(source: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(source).unwrap_or_else(|e| panic!("parse {e:?}\n{source}")))
        .unwrap_or_else(|e| panic!("types {e:?}\n{source}"))
}
fn accepted(source: &str) {
    let p = checked(source);
    for r in verify_all(&p, &VerifierConfig::default()) {
        assert!(r.ok(), "{}", r.summary());
    }
}
const TYPES:&str="record Pair {amount:u8;flag:bool;}record Empty {}union Message {Data:Pair;Number:u64;None:Empty;}";
#[test]
fn structured_contracts_compose_through_functions_and_effects() {
    accepted(&format!("{TYPES}function echo(x:Message)->Message ensure exact:eq(result,x);body {{yield x;}}effect E(message:Message);transition go(x:Message) reads [] writes [] ensure one:eq(effect_count(),1);ensure kind:effect_is(E,0);ensure same:eq(effect_arg(E,0,message),x);body {{emit E(echo(x));commit;}}"));
}
#[test]
fn unsafe_union_payload_is_a_real_trap_and_lazy_type_guard_proves_safe() {
    let p=checked(&format!("{TYPES}transition go(x:Message) reads [] writes [] body {{let %p:Pair=payload<Message,Data>(x);commit;}}"));
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert!(!r.ok());
    assert_eq!(r.panic.status, "sat", "{}", r.summary());
    assert_eq!(
        r.counterexample.unwrap().failure.unwrap().code,
        "ERR_VARIANT_ACCESS"
    );
    accepted(&format!("{TYPES}transition go(x:Message) reads [] writes [] ensure safe:select(is_variant<Message,Data>(x),le(field(payload<Message,Data>(x),amount),255),true);body {{commit;}}"));
}
#[test]
fn structured_state_keeps_old_values_and_field_invariants() {
    accepted(&format!("{TYPES}table T capacity 2 {{key id:id64<T>;col item:Pair;}}invariant bounded {{forall k in T:le(field(T[k].item,amount),10);}}transition go(k:id64<T>,next:Pair) reads [] writes [T[k].item] require live:table_exists(T,k);require bound:le(field(next,amount),10);ensure updated:eq(T[k].item,next);ensure old_bound:le(field(old(T[k].item),amount),10);body {{table_set(T,k,item,next);commit;}}"));
}
#[test]
fn nested_scalar_domains_are_constrained_in_structured_parameters() {
    accepted("record R {tag:enum {A,B};flag:bool;}function valid(x:R)->bool ensure yes:eq(result,true);body {yield and(or(eq(field(x,tag),A),eq(field(x,tag),B)),or(field(x,flag),not(field(x,flag))));}");
}
#[test]
fn structured_values_larger_than_scalar_gpu_carrier_are_not_numeric_packed_types() {
    accepted("record Large {first:bytes32;second:bytes32;}function same(x:Large)->Large ensure exact:eq(result,x);body {yield x;}effect E(value:Large);transition go(x:Large) reads [] writes [] ensure exact:eq(effect_arg(E,0,value),x);body {emit E(same(x));commit;}");
}
#[test]
fn nominally_distinct_equal_shapes_do_not_implicitly_convert() {
    let source = "record A {n:u8;}record B {n:u8;}function wrong(x:A)->B body {yield x;}";
    assert!(analyze(parse_source(source).unwrap()).is_err());
}
#[test]
fn unions_have_no_eight_bit_tag_policy_or_variant_cartesian_expansion() {
    let mut source = String::from("union U {");
    for i in 0..300 {
        source.push_str(&format!("V{i}:u8;"));
    }
    source.push_str("}record Many {");
    for i in 0..8 {
        source.push_str(&format!("f{i}:U;"));
    }
    source.push_str("}function id(x:Many)->Many ensure exact:eq(result,x);body {yield x;}effect E(value:Many);transition go(x:Many) reads [] writes [] ensure exact:eq(effect_arg(E,0,value),x);body {emit E(id(x));commit;}");
    let p = checked(&source);
    let r = verify_transition(&p, "go", &VerifierConfig::default());
    assert!(r.ok(), "{}", r.summary());
    assert!(
        r.smt_safety.len() < 500_000,
        "interface variants must not be Cartesian expanded: {}",
        r.smt_safety.len()
    );
}
