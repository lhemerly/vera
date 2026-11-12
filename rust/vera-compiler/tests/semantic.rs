//! Native frontend admission and resolved-semantics regressions.

use vera_compiler::parser::parse_source;
use vera_compiler::semantic::analyze;
use vera_compiler::{
    ir::{BindingOrigin, ExprKind, Snapshot, StmtKind},
    types::ScalarType,
};

fn accept(source: &str) {
    let module = parse_source(source).unwrap();
    if let Err(errors) = analyze(module) {
        panic!("valid source was rejected: {errors:?}\n{source}");
    }
}

fn reject(source: &str, code: &str) {
    let module = parse_source(source).unwrap();
    match analyze(module) {
        Ok(_) => panic!("invalid source was admitted: {source}"),
        Err(errors) => {
            assert!(
                errors.iter().any(|e| e.code == code),
                "missing {code}: {errors:?}\n{source}"
            );
            assert!(errors.iter().all(|e| e.span.end >= e.span.start));
        }
    }
}

const HEADER: &str = "table T capacity 2 {key id:id64<T>;col v:u8;}";

#[test]
fn all_shipped_programs_have_native_semantic_admission() {
    for source in [
        include_str!("../../../examples/bank.vera"),
        include_str!("../../../examples/bank_buggy.vera"),
        include_str!("../../../examples/counter.vera"),
    ] {
        // A deliberately broken invariant preservation proof is still a valid
        // typed source program; verification is a separate operation.
        accept(source);
    }
}

#[test]
fn forward_and_self_referencing_handles_resolve_before_columns() {
    accept("table A capacity 1 {key id:id64<A>;col own:id64<A>;col other:id64<B>;}table B capacity 2 {key id:id64<B>;col value:u64;}");
    reject(
        "table A capacity 1 {key id:id64<A>;col other:id64<Missing>;}",
        "D_UNKNOWN_TABLE",
    );
    reject(
        "table A capacity 1 {key id:id64<B>;}table B capacity 1 {key id:id64<B>;}",
        "D_BAD_KEY",
    );
}

#[test]
fn capacity_validation_never_allocates_table_contents() {
    accept("table T capacity 4294967296 {key id:id64<T>;col v:dec128 [tracked_sum];col code:bytes32 [unique];}");
    for n in ["0", "3", "4294967297", "18446744073709551616"] {
        reject(
            &format!("table T capacity {n} {{key id:id64<T>;}}"),
            "D_CAPACITY",
        );
    }
}

#[test]
fn duplicate_declarations_bindings_parameters_and_enum_tags_are_rejected() {
    for source in [
        "table T capacity 1 {key id:id64<T>;}table T capacity 1 {key id:id64<T>;}",
        "table T capacity 1 {key id:id64<T>;col v:u8;col v:u8;}",
        "effect E(x:u64,x:u64);",
        "transition go(x:u64,x:u64) reads [] writes [] body {commit;}",
        "transition go() reads [] writes [] body {let %x:u64=1;let %x:u64=2;commit;}",
    ] {
        reject(source, "D_DUPLICATE");
    }
    reject(
        "table T capacity 1 {key id:id64<T>;col tag:enum{A,A};}",
        "D_ENUM_TAG",
    );
}

#[test]
fn scalar_literals_are_contextual_exact_and_range_checked() {
    accept("transition go() reads [] writes [] body {let %a:u8=255;let %b:dec128=-1.000000000000000001;let %c:bytes32=0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff;let %d:enum{A,B}=B;let %e:sym32=\"verified\";commit;}");
    for expression in ["256", "-1", "340282366920938463463374607431768211456"] {
        reject(
            &format!("transition go() reads [] writes [] body {{let %x:u8={expression};commit;}}"),
            "D_LITERAL_RANGE",
        );
    }
    reject(
        "transition go() reads [] writes [] body {let %x:dec64=0.000000001;commit;}",
        "D_LITERAL_RANGE",
    );
    reject(
        "transition go() reads [] writes [] body {let %x:bool=1;commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go() reads [] writes [] body {let %x:u64=true;commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go() reads [] writes [] body {let %x:u64=1.5;commit;}",
        "D_TYPE_MISMATCH",
    );
}

#[test]
fn symbol_collisions_are_rejected_instead_of_aliasing_literals() {
    reject("transition go() reads [] writes [] body {let %a:sym32=\"vera-symbol-41416\";let %b:sym32=\"vera-symbol-91667\";commit;}", "D_LITERAL_RANGE");
}

#[test]
fn fixed_width_variables_never_implicitly_convert() {
    reject(
        "transition go(a:u8,b:u64) reads [] writes [] body {let %x:u64=add(a,b);commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go(a:u8) reads [] writes [] body {let %x:u64=a;commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go(a:u8) reads [] writes [] body {let %x:u8=neg(a);commit;}",
        "D_BAD_OPERAND",
    );
    reject(
        "transition go(a:bytes32,b:bytes32) reads [] writes [] body {let %x:bool=lt(a,b);commit;}",
        "D_BAD_OPERAND",
    );
    accept("transition go(a:u8,b:u8) reads [] writes [] body {let %x:u8=add(a,b);let %same:bool=eq(1,a);commit;}");
}

#[test]
fn handles_are_strongly_typed_and_require_explicit_integer_construction() {
    reject(&format!("{HEADER}transition go() reads [T[*].v] writes [] body {{let %x:u8=table_get(T,0,v);commit;}}"), "D_TYPE_MISMATCH");
    reject("table A capacity 1 {key id:id64<A>;col v:u64;}table B capacity 1 {key id:id64<B>;}transition go(k:id64<B>) reads [A[*].v] writes [] body {let %x:u64=table_get(A,k,v);commit;}", "D_TYPE_MISMATCH");
    accept(&format!("{HEADER}transition go() reads [T[*].v] writes [] body {{let %x:u8=table_get(T,id64<T>(18446744073709551615),v);commit;}}"));
    reject(&format!("{HEADER}transition go() reads [] writes [] body {{let %x:id64<T>=id64<T>(true);commit;}}"), "D_TYPE_MISMATCH");
}

#[test]
fn table_columns_aggregate_markers_and_unique_immutability_are_checked() {
    reject(
        &format!(
            "{HEADER}transition go(k:id64<T>) reads [T[k].missing] writes [] body {{commit;}}"
        ),
        "D_UNKNOWN_COLUMN",
    );
    reject(
        &format!(
            "{HEADER}transition go() reads [T[*].v] writes [] body {{let %x:u8=sum(T.v);commit;}}"
        ),
        "D_BAD_OPERAND",
    );
    reject(
        "table T capacity 1 {key id:id64<T>;col code:bytes32 [tracked_sum];}",
        "D_BAD_OPERAND",
    );
    reject(&format!("{HEADER}transition go() reads [T[*].v] writes [] body {{let %x:id64<T>=table_lookup(T,v,0);commit;}}"), "D_BAD_OPERAND");
    reject("table T capacity 1 {key id:id64<T>;col code:u64 [unique];}transition go(k:id64<T>) reads [] writes [T[k].code] body {table_set(T,k,code,1);commit;}", "D_BAD_OPERAND");
}

#[test]
fn allocation_initializes_each_column_once_and_effects_are_typed() {
    reject(&format!("{HEADER}transition go() reads [] writes [T[*].v] body {{let %k:id64<T>=table_alloc(T,{{}});commit;}}"), "D_UNINITIALIZED");
    reject(&format!("{HEADER}transition go() reads [] writes [T[*].v] body {{let %k:id64<T>=table_alloc(T,{{v:1,v:2}});commit;}}"), "D_DUPLICATE");
    reject(&format!("{HEADER}transition go() reads [] writes [T[*].v] body {{let %k:id64<T>=table_alloc(T,{{missing:1}});commit;}}"), "D_UNKNOWN_COLUMN");
    reject(
        "effect E(x:bool);transition go() reads [] writes [] body {emit E(1);commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "effect E(x:u64);transition go() reads [] writes [] body {emit E();commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go() reads [] writes [] body {emit Missing();commit;}",
        "D_UNKNOWN_EFFECT",
    );
}

#[test]
fn requires_guards_and_specification_results_are_boolean() {
    reject(&format!("{HEADER}invariant bad {{1;}}"), "D_TYPE_MISMATCH");
    reject(
        "transition go() reads [] writes [] require bad:1;body {commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go() reads [] writes [] ensure bad:1;body {commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go() reads [] writes [] body {guard(1,ERR_REJECT);commit;}",
        "D_TYPE_MISMATCH",
    );
    reject(
        "transition go() reads [] writes [] body {let %x:u8=select(1,2,3);commit;}",
        "D_TYPE_MISMATCH",
    );
}

#[test]
fn actual_reads_and_writes_must_match_the_declared_footprints() {
    reject(&format!("{HEADER}transition go(k:id64<T>) reads [] writes [] body {{let %x:u8=table_get(T,k,v);commit;}}"), "D_ACCESS_VIOLATION");
    reject(&format!("{HEADER}transition go(k:id64<T>) reads [] writes [] body {{table_set(T,k,v,1);commit;}}"), "D_ACCESS_VIOLATION");
    reject(&format!("{HEADER}transition go(a:id64<T>,b:id64<T>) reads [T[a].v] writes [] body {{let %x:u8=table_get(T,b,v);commit;}}"), "D_ACCESS_VIOLATION");
    accept(&format!("{HEADER}transition go(a:id64<T>,b:id64<T>) reads [T[*].v] writes [T[*].v] body {{let %x:u8=table_get(T,b,v);table_set(T,a,v,%x);commit;}}"));
}

#[test]
fn fold_bindings_are_scoped_and_bounds_have_matching_integer_types() {
    accept("transition go() reads [] writes [] body {let %one:u64=fold_bounded<2>(acc:u64=0,i in [0..1]) {let %local:u64=1;yield add(acc,%local);};let %two:u64=fold_bounded<2>(acc:u64=0,i in [0..1]) {yield add(acc,1);};commit;}");
    reject("transition go() reads [] writes [] body {let %one:u64=fold_bounded<1>(acc:u64=0,i in [0..0]) {let %local:u64=1;yield %local;};let %out:u64=%local;commit;}", "D_UNKNOWN_PARAM");
    reject("transition go() reads [] writes [] body {let %one:u64=fold_bounded<1>(acc:u64=0,acc in [0..0]) {yield acc;};commit;}", "D_DUPLICATE");
    reject("transition go() reads [] writes [] body {let %one:u64=fold_bounded<1>(acc:u64=0,i in [0.0..1.0]) {yield acc;};commit;}", "D_BAD_OPERAND");
}

#[test]
fn fold_representation_bounds_do_not_impose_validation_policy_limits() {
    for bound in ["0", "18446744073709551616"] {
        reject(&format!("transition go() reads [] writes [] body {{let %x:u64=fold_bounded<{bound}>(acc:u64=0,i in [0..1]) {{yield acc;}};commit;}}"), "D_FOLD_BOUND");
    }
    accept("transition go() reads [] writes [] body {let %x:u64=fold_bounded<32>(a:u64=0,i in [0..31]) {let %b:u64=fold_bounded<33>(b:u64=0,j in [0..32]) {yield b;};yield a;};commit;}");
    accept("transition go() reads [] writes [] body {let %x:u64=fold_bounded<1>(a:u64=0,i in [0..0]) {let %b:u64=fold_bounded<1>(b:u64=0,j in [0..0]) {let %c:u64=fold_bounded<1>(c:u64=0,k in [0..0]) {yield c;};yield b;};yield a;};commit;}");
}

#[test]
fn specification_projection_and_footprint_folds_cannot_mutate_state() {
    let mutating = "fold_bounded<1>(acc:id64<T>=id64<T>(0),i in [0..0]) {let %new:id64<T>=table_alloc(T,{v:0});yield %new;}";
    reject(
        &format!(
            "{HEADER}projection V() {{allow_if:true;expose:[T[{mutating}].v];enable_intents:[];}}"
        ),
        "D_BAD_OPERAND",
    );
    reject(
        &format!("{HEADER}transition go() reads [T[{mutating}].v] writes [] body {{commit;}}"),
        "D_BAD_OPERAND",
    );
    reject(
        &format!("{HEADER}invariant bad {{eq({mutating},id64<T>(0));}}"),
        "D_BAD_OPERAND",
    );
    reject("invariant bad {eq(fold_bounded<1>(acc:u64=0,i in [0..0]) {guard(true,ERR_REJECT);yield acc;},0);}", "D_BAD_OPERAND");
}

#[test]
fn old_state_keys_allow_pure_guarded_folds_and_reject_writes() {
    accept(&format!("{HEADER}transition go(k:id64<T>) reads [T[*].v] writes [] body {{let %x:u8=old(T[fold_bounded<1>(acc:id64<T>=k,i in [0..0]) {{guard(true,ERR_REJECT);yield acc;}}].v);commit;}}"));
    reject(&format!("{HEADER}transition go(k:id64<T>) reads [T[*].v] writes [T[k].v] body {{let %x:u8=old(T[fold_bounded<1>(acc:id64<T>=k,i in [0..0]) {{table_set(T,k,v,1);yield acc;}}].v);commit;}}"), "D_BAD_OPERAND");
}

#[test]
fn projections_resolve_transition_names_and_typed_row_keys() {
    reject(&format!("{HEADER}projection V() {{allow_if:true;expose:[];enable_intents:[missing when (true)];}}"), "D_UNKNOWN_TRANSITION");
    reject(
        &format!("{HEADER}projection V() {{allow_if:true;expose:[T[0].v];enable_intents:[];}}"),
        "D_TYPE_MISMATCH",
    );
    reject(
        &format!("{HEADER}projection V() {{allow_if:1;expose:[];enable_intents:[];}}"),
        "D_TYPE_MISMATCH",
    );
    accept(&format!("{HEADER}projection V(k:id64<T>) {{allow_if:table_exists(T,k);expose:[T[k].v,T.v];enable_intents:[go when (true)];}}transition go() reads [] writes [] body {{commit;}}"));
}

#[test]
fn aggregate_domains_and_scalar_narrowing_are_resolved_before_backends() {
    let program = analyze(parse_source("table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum];}transition go() reads [T[*].v] writes [] ensure total:le(add(sum(T.v),1),1000);body {let %narrow:u8=sum(T.v);let %again:u8=%narrow;commit;}").unwrap()).unwrap();
    let transition = &program.transitions()[0];
    let condition = program.expr(transition.ensures[0].value);
    assert_eq!(
        condition.domain.scalar_domain().unwrap().scalar,
        ScalarType::Bool
    );
    assert_eq!(condition.domain.scalar_domain().unwrap().width, 8);
    let ExprKind::Binary { left, right, .. } = condition.kind else {
        panic!("comparison missing")
    };
    assert_eq!(
        program.expr(left).domain.scalar_domain().unwrap().width,
        128
    );
    assert_eq!(
        program.expr(right).domain.scalar_domain().unwrap().width,
        128
    );
    let ExprKind::Constant(ref value) = program.expr(right).kind else {
        panic!("contextual literal missing")
    };
    assert_eq!(value.int().unwrap().to_string(), "1000");
    let StmtKind::Let { binding, value } = transition.body[0].kind else {
        panic!("scalar boundary missing")
    };
    assert_eq!(program.binding(binding).ty, ScalarType::U8);
    assert_eq!(
        program.expr(value).domain.scalar_domain().unwrap().width,
        128
    );
    let StmtKind::Let { value, .. } = transition.body[1].kind else {
        panic!("later binding missing")
    };
    assert_eq!(program.expr(value).domain.scalar_domain().unwrap().width, 8);
}

#[test]
fn sibling_fold_scopes_have_distinct_stable_bindings_and_exact_costs() {
    let program = analyze(parse_source("transition go() reads [] writes [] body {let %a:u64=fold_bounded<2>(acc:u64=0,i in [0..1]) {yield add(acc,1);};let %b:u64=fold_bounded<3>(acc:u64=0,i in [0..2]) {yield add(acc,1);};commit;}").unwrap()).unwrap();
    let accumulators = program
        .bindings()
        .iter()
        .filter(|binding| binding.origin == BindingOrigin::FoldAccumulator)
        .collect::<Vec<_>>();
    assert_eq!(accumulators.len(), 2);
    assert_ne!(accumulators[0].id, accumulators[1].id);
    assert_eq!(accumulators[0].name, accumulators[1].name);
    assert_eq!(program.transitions()[0].wcet_bound, 7);
    assert_eq!(program.transitions()[0].fold_depth, 1);
    assert_eq!(program.transitions()[0].max_fold_product, 3);
}

#[test]
fn old_state_access_lowers_its_complete_key_subtree_in_the_pre_state() {
    let program = analyze(parse_source("table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:u8;}transition go(k:id64<T>) reads [T[*].owner,T[*].v] writes [T[k].owner] body {table_set(T,k,owner,id64<T>(3));let %old:u8=old(T[table_get(T,k,owner)].v);commit;}").unwrap()).unwrap();
    let StmtKind::Let { value, .. } = program.transitions()[0].body[1].kind else {
        panic!("old value missing")
    };
    let ExprKind::SlotRead { key, state, .. } = program.expr(value).kind else {
        panic!("old total access missing")
    };
    assert_eq!(state, Snapshot::Pre);
    let ExprKind::TableGet { state, .. } = program.expr(key).kind else {
        panic!("old key read missing")
    };
    assert_eq!(state, Snapshot::Pre);
}

#[test]
fn state_dependent_select_conditions_cannot_be_misclassified_as_tier_one() {
    let program = analyze(parse_source(&format!("{HEADER}transition go(k:id64<T>) reads [T[*].v] writes [] body {{let %selected:id64<T>=select(eq(table_get(T,k,v),0),k,id64<T>(3));let %value:u8=table_get(T,%selected,v);commit;}}" )).unwrap()).unwrap();
    let selected = program
        .bindings()
        .iter()
        .find(|binding| binding.name == "%selected")
        .unwrap();
    assert!(selected.state_dependent);
    assert_eq!(program.transitions()[0].tier, 2);
    assert!(program.transitions()[0]
        .reads
        .iter()
        .any(|access| access.tier == 2));
}

#[test]
fn footprint_fold_equivalence_is_structural_and_alpha_renames_local_bindings() {
    let declared = "fold_bounded<1>(outer:id64<T>=k,j in [0..0]) {yield outer;}";
    let actual = "fold_bounded<1>(acc:id64<T>=k,i in [0..0]) {yield acc;}";
    accept(&format!("{HEADER}transition go(k:id64<T>,other:id64<T>) reads [T[{declared}].v] writes [] body {{let %value:u8=table_get(T,{actual},v);commit;}}"));
    let changed = "fold_bounded<1>(acc:id64<T>=other,i in [0..0]) {yield acc;}";
    reject(&format!("{HEADER}transition go(k:id64<T>,other:id64<T>) reads [T[{declared}].v] writes [] body {{let %value:u8=table_get(T,{changed},v);commit;}}"), "D_ACCESS_VIOLATION");
}

#[test]
fn guarded_old_key_folds_do_not_bypass_outer_specification_purity() {
    reject(&format!("{HEADER}invariant bad {{eq(old(T[fold_bounded<1>(acc:id64<T>=id64<T>(0),i in [0..0]) {{guard(true,ERR_REJECT);yield acc;}}].v),0);}}"), "D_BAD_OPERAND");
}

#[test]
fn programmatic_malformed_literal_returns_a_diagnostic_without_panicking() {
    use vera_compiler::{ast, literal::DecimalLiteral};

    let mut module =
        parse_source("transition go() reads [] writes [] body {let %value:dec64=1.0;commit;}")
            .unwrap();
    let ast::Decl::Transition(transition) = &mut module.decls[0] else {
        panic!("transition missing")
    };
    let ast::Stmt::Let(binding) = &mut transition.body[0] else {
        panic!("binding missing")
    };
    let ast::Expr::Literal(literal) = &mut binding.value else {
        panic!("literal missing")
    };
    literal.value = ast::LiteralValue::Decimal(DecimalLiteral {
        negative: false,
        digits: "🔥".into(),
        scale: 10,
    });
    let errors = analyze(module).unwrap_err();
    assert_eq!(errors[0].code, "D_LITERAL_RANGE");
}

#[test]
fn footprint_selectors_distinguish_current_and_explicit_old_state() {
    let header = "table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:u8;}";
    let current = format!("{header}transition go(k:id64<T>) reads [T[k].owner,T[table_get(T,k,owner)].v] writes [T[k].owner] body {{table_set(T,k,owner,id64<T>(3));let %value:u8=table_get(T,table_get(T,k,owner),v);commit;}}");
    accept(&current);
    let wrong_snapshot = format!("{header}transition go(k:id64<T>) reads [T[k].owner,T[table_get(T,k,owner)].v] writes [T[k].owner] body {{table_set(T,k,owner,id64<T>(3));let %value:u8=old(T[table_get(T,k,owner)].v);commit;}}");
    reject(&wrong_snapshot, "D_ACCESS_VIOLATION");
    let wildcard = wrong_snapshot.replace("T[table_get(T,k,owner)].v] writes", "T[*].v] writes");
    accept(&wildcard);
    let requirement = format!("{header}transition go(k:id64<T>) reads [T[k].owner,T[table_get(T,k,owner)].v] writes [] require same:ge(T[table_get(T,k,owner)].v,0);body {{commit;}}");
    accept(&requirement);
}
