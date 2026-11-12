use vera_compiler::{analyze, ir::StmtKind, parse_source};

const TABLES: &str = "table A capacity 2 {key id:id64<A>;col v:u8;}table B capacity 2 {key id:id64<B>;col v:u8;}effect E(v:u8);";
fn checked(source: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(source).unwrap_or_else(|e| panic!("{e:?}\n{source}")))
        .unwrap_or_else(|e| panic!("{e:?}\n{source}"))
}
fn rejected(source: &str, code: &str) {
    let error = analyze(parse_source(source).unwrap()).unwrap_err();
    assert_eq!(error[0].code, code, "{error:?}\n{source}");
}
#[test]
fn components_retain_owners_scoped_invariants_and_declared_frames() {
    let p = checked(&format!("{TABLES}component C owns [A] {{invariant local {{forall k in A:le(A[k].v,10);}}operation same(k:id64<A>) reads [A[*].v] writes [A[k].v] effects [E] require live:table_exists(A,k);ensure unchanged:eq(table_get(A,k,v),old(A[k].v));body {{emit E(table_get(A,k,v));commit;}}}}"));
    assert_eq!(p.components().len(), 1);
    assert_eq!(p.operations().len(), 1);
    let o = &p.operations()[0];
    assert_eq!(o.transition.name, "C.same");
    assert_eq!(p.invariants()[0].owner, Some(o.component));
    assert_eq!(p.invariants()[0].name, "C.local");
    assert_eq!(o.writes.len(), 1); // Retain the promise even when body writes nothing.
    assert!(o.reads[0].key.is_none());
    assert!(o.transition.writes.is_empty());
    assert_eq!(p.table_owner(p.table_id("A").unwrap()), Some(o.component));
    assert_eq!(p.table_owner(p.table_id("B").unwrap()), None);
}
#[test]
fn tables_have_one_exclusive_mutation_owner() {
    rejected(
        &format!("{TABLES}component C owns [A] {{}} component D owns [A] {{}}"),
        "D_COMPONENT_OWNERSHIP",
    );
    rejected(
        &format!("{TABLES}component C owns [A,A] {{}}"),
        "D_COMPONENT_OWNERSHIP",
    );
    rejected(&format!("{TABLES}component C owns [A] {{}}transition bad(k:id64<A>) reads [] writes [A[k].v]body {{table_set(A,k,v,0);commit;}}"),"D_COMPONENT_OWNERSHIP");
}
#[test]
fn foreign_state_is_rejected_in_owner_invariants_contracts_and_dead_expressions() {
    for part in [
        "invariant bad {forall k in B:eq(B[k].v,0);}",
        "operation bad(k:id64<B>) reads [] writes [] effects [] require foreign:table_exists(B,k);body {commit;}",
        "operation bad(k:id64<B>) reads [] writes [] effects [] ensure foreign:table_exists(B,k);body {commit;}",
        "operation bad(k:id64<B>) reads [] writes [] effects [] body {let %x:bool=select(false,table_exists(B,k),true);commit;}",
    ] { rejected(&format!("{TABLES}component C owns [A] {{{part}}}"),"D_COMPONENT_OWNERSHIP"); }
}
#[test]
fn nested_fold_owned_writes_and_undeclared_effects_cannot_escape_checks() {
    rejected(&format!("{TABLES}component C owns [A] {{}}transition bad(k:id64<A>) reads [] writes [A[k].v]body {{let %n:u8=fold_bounded<1>(a:u8=0,i in [0..0]) {{table_set(A,k,v,0);yield a;}};commit;}}"),"D_COMPONENT_OWNERSHIP");
    rejected(&format!("{TABLES}component C owns [A] {{operation bad() reads [] writes [] effects []body {{let %n:u8=fold_bounded<1>(a:u8=0,i in [0..0]) {{emit E(a);yield a;}};commit;}}}}"),"D_OPERATION_EFFECT");
}
#[test]
fn operation_frames_are_input_stable_and_allocation_is_not_silently_framed() {
    rejected(&format!("{TABLES}component C owns [A] {{operation bad(k:id64<A>) reads [A[*].v] writes [A[select(eq(table_get(A,k,v),0),k,k)].v] effects []body {{commit;}}}}"),"D_OPERATION_FRAME");
    rejected(&format!("{TABLES}component C owns [A] {{operation bad() reads [] writes [A[*].v] effects []body {{let %k:id64<A>=table_alloc(A,{{v:0}});commit;}}}}"),"D_OPERATION_ALLOCATION");
}
#[test]
fn leaf_and_unknown_calls_have_explicit_diagnostics() {
    rejected(&format!("{TABLES}component C owns [A] {{operation bad() reads [] writes [] effects []body {{call C.bad();commit;}}}}"),"D_OPERATION_NESTING");
    rejected(
        &format!("{TABLES}transition bad() reads [] writes []body {{call C.missing();commit;}}"),
        "D_UNKNOWN_OPERATION",
    );
}
#[test]
fn forward_calls_and_parameterized_frames_resolve_without_wildcard_overreach() {
    let p=checked(&format!("{TABLES}transition go(k:id64<A>,v:u8) reads [A[k].v] writes [A[k].v]body {{call C.set(k,v);commit;}}component C owns [A] {{operation set(row:id64<A>,value:u8) reads [A[row].v] writes [A[row].v] effects [] require live:table_exists(A,row);body {{table_set(A,row,v,value);commit;}}}}"));
    let StmtKind::OperationCall { reads, writes, .. } = &p.transitions()[0].body[0].kind else {
        panic!("expected call")
    };
    assert!(reads[0].key.is_some());
    assert!(writes[0].key.is_some());
    assert!(p.transitions()[0].reads[0].key.is_some());
}
#[test]
fn malformed_or_duplicate_component_declarations_are_not_accepted() {
    rejected(
        &format!("{TABLES}component C owns [A] {{}}component C owns [B] {{}}"),
        "D_DUPLICATE",
    );
    rejected(&format!("{TABLES}component C owns [A] {{operation f() reads [] writes [] effects []body {{commit;}}operation f() reads [] writes [] effects []body {{commit;}}}}"),"D_DUPLICATE");
    rejected(&format!("{TABLES}component C owns [A] {{operation f() reads [] writes [] effects [E,E]body {{commit;}}}}"),"D_DUPLICATE");
}
