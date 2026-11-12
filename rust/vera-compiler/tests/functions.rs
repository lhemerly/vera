use num_bigint::BigInt;
use vera_compiler::{
    ir::CheckedProgram,
    runtime::{eval_function, FailureKind, Machine},
    value::Value,
};
fn checked(source: &str) -> CheckedProgram {
    vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap()
}
fn int(x: u64) -> Value {
    Value::Int(BigInt::from(x))
}
fn reject(source: &str, code: &str) {
    let errors =
        vera_compiler::semantic::analyze(vera_compiler::parse_source(source).unwrap()).unwrap_err();
    assert!(errors.iter().any(|d| d.code == code), "{errors:?}");
}
#[test]
fn forward_dependencies_and_isolated_bindings_execute_real_bodies() {
    let p=checked("function outer(x:u64)->u64 body {yield add(inner(x),x);} function inner(x:u64)->u64 ensure misleading:eq(result,99); body {yield add(x,1);}");
    assert_eq!(p.functions()[0].name, "inner");
    assert_eq!(
        eval_function(&p, p.function_id("outer").unwrap(), &[int(3)]).unwrap(),
        int(7)
    );
    assert!(p.functions()[1].wcet_bound > p.functions()[0].wcet_bound);
}
#[test]
fn huge_unrelated_tables_are_not_allocated_for_function_replay() {
    let p=checked("table T capacity 4294967296 {key id:id64<T>;col v:u64;} function identity(x:u64)->u64 body {yield x;}");
    assert_eq!(
        eval_function(&p, p.function_id("identity").unwrap(), &[int(42)]).unwrap(),
        int(42)
    );
}
#[test]
fn call_contract_failures_are_ordered_traps_and_roll_back_effects() {
    let p = checked(include_str!("fixtures/functions.vera"));
    let mut m = Machine::new(&p).unwrap();
    let before = m.state.clone();
    let e = m.call("bad_pre", &Default::default()).unwrap_err();
    assert_eq!(e.kind, FailureKind::Trap);
    assert_eq!(e.code, "ERR_FUNCTION_PRECONDITION");
    assert_eq!(m.state, before);
    m.call("lazy", &Default::default()).unwrap();
    assert_eq!(m.state.outbox[0].args, vec![int(7)]);
    let f = p.function_id("ordered").unwrap();
    let e = eval_function(&p, f, &[int(255)]).unwrap_err();
    assert_ne!(e.code, "ERR_FUNCTION_PRECONDITION");
    let e = eval_function(&p, f, &[int(0)]).unwrap_err();
    assert_eq!(e.code, "ERR_FUNCTION_PRECONDITION");
}
#[test]
fn arguments_execute_once_in_source_order() {
    let p = checked(include_str!("fixtures/functions.vera"));
    let mut m = Machine::new(&p).unwrap();
    m.call("argument_order", &Default::default()).unwrap();
    assert_eq!(
        m.state
            .outbox
            .iter()
            .map(|r| r.args[0].clone())
            .collect::<Vec<_>>(),
        vec![int(1), int(2), int(11)]
    );
}
#[test]
fn invalid_unused_functions_are_rejected() {
    for source in [
        "function f(x:u64)->u64 body {yield f(x);}",
        "function f(x:u64)->u64 body {yield g(x);} function g(x:u64)->u64 body {yield f(x);}",
    ] {
        reject(source, "D_FUNCTION_CYCLE");
    }
    reject(
        "function f(x:u64)->u64 body {yield missing(x);}",
        "D_UNKNOWN_FUNCTION",
    );
    reject(
        "function f(x:u64)->u64 require p:eq(f(x),x); body {yield x;}",
        "D_FUNCTION_CONTRACT",
    );
    reject("table T capacity 1 {key id:id64<T>;col v:u64;} function f(x:id64<T>)->u64 body {yield table_get(T,x,v);}","D_FUNCTION_PURITY");
    reject(
        "function f(result:u64)->u64 body {yield result;}",
        "D_DUPLICATE",
    );
    reject(
        "function f(x:u64)->u64 require a:true; require a:true; body {yield x;}",
        "D_DUPLICATE",
    );
    reject(
        "function f(x:u64)->u64 body {yield x;} function g()->u64 body {yield f();}",
        "D_ARITY",
    );
    reject(
        "function f(x:u64)->u64 body {yield result;}",
        "D_UNKNOWN_PARAM",
    );
}
#[test]
fn functions_have_no_fixed_count_or_dependency_depth_limit() {
    let mut source = "function f0(x:u64)->u64 body {yield x;}".to_string();
    for i in 1..80 {
        source.push_str(&format!(
            "function f{i}(x:u64)->u64 body {{yield f{}(x);}}",
            i - 1
        ));
    }
    let p = checked(&source);
    assert_eq!(
        eval_function(&p, p.function_id("f79").unwrap(), &[int(8)]).unwrap(),
        int(8)
    );
}

#[test]
fn declarations_and_contracts_participate_in_canonical_identity() {
    let base = "function f(x:u64)->u64 ensure exact:eq(result,x); body {yield x;}";
    let ast = vera_compiler::parse_source(base).unwrap().canonical_json();
    for changed in [
        base.replace("yield x", "yield add(x,1)"),
        base.replace("eq(result,x)", "ge(result,x)"),
        base.replace("x:u64", "x:u32"),
    ] {
        assert_ne!(
            ast,
            vera_compiler::parse_source(&changed)
                .unwrap()
                .canonical_json()
        );
    }
    assert_eq!(
        ast,
        vera_compiler::parse_source(&format!("// spacing\n{base}\n"))
            .unwrap()
            .canonical_json()
    );
}

#[test]
fn function_arguments_and_returns_are_narrowed_and_typed() {
    let p = checked(
        "function f(x:u8)->u8 body {yield x;} function overflow(x:u8)->u8 body {yield add(x,1);}",
    );
    assert!(eval_function(&p, p.function_id("f").unwrap(), &[int(256)]).is_err());
    assert!(eval_function(&p, p.function_id("f").unwrap(), &[]).is_err());
    assert!(eval_function(&p, p.function_id("overflow").unwrap(), &[int(255)]).is_err());
    reject(
        "function f(x:u8)->u8 body {yield x;} function g(x:u64)->u8 body {yield f(x);}",
        "D_TYPE_MISMATCH",
    );
}

#[test]
fn purity_checks_cover_effectful_folds_and_unused_contracts() {
    reject("effect E(); function f()->u64 body {yield fold_bounded<1>(acc:u64=0,i in [0..0]){emit E();yield acc;};}","D_FUNCTION_PURITY");
    reject(
        "function f()->u64 ensure count:eq(effect_count(),0); body {yield 0;}",
        "D_FUNCTION_PURITY",
    );
    reject("table T capacity 1 {key id:id64<T>;col v:u64;} function f(x:id64<T>)->u64 ensure same:eq(result,old(T[x].v)); body {yield 0;}","D_FUNCTION_PURITY");
}

#[test]
fn dependency_graph_construction_does_not_expand_repeated_calls() {
    let mut source = "function f0(x:bool)->bool body {yield x;}".to_string();
    for i in 1..20 {
        source.push_str(&format!(
            "function f{i}(x:bool)->bool body {{yield and(f{}(x),f{}(x));}}",
            i - 1,
            i - 1
        ));
    }
    let p = checked(&source);
    assert_eq!(p.functions().len(), 20);
    assert_eq!(p.functions()[19].dependencies.len(), 1);
    assert!(p.functions()[19].wcet_bound > 1_000_000);
}

#[test]
fn traced_calls_identify_arguments_and_failed_named_requirements() {
    let p = checked(include_str!("fixtures/functions.vera"));
    let mut m = Machine::new(&p).unwrap();
    m.execute("bad_pre", &Default::default(), true).unwrap_err();
    assert!(m
        .last_trace
        .iter()
        .any(|step| step.kind == "function_argument"
            && step.text == "quotient.y"
            && step.value == Some(int(0))));
    assert!(m
        .last_trace
        .iter()
        .any(|step| step.kind == "function_require"
            && step.text == "quotient.nonzero"
            && step.value == Some(Value::Bool(false))));
    assert!(m
        .last_trace
        .iter()
        .any(|step| step.kind == "function_return"
            && step.text == "quotient"
            && step.value.is_none()));
}

#[test]
fn independent_function_count_has_no_fixed_policy_limit() {
    let source = (0..1100)
        .map(|i| format!("function f{i}()->bool body {{yield true;}}\n"))
        .collect::<String>();
    assert_eq!(checked(&source).functions().len(), 1100);
}

#[test]
fn projections_execute_pure_calls_and_fail_closed_on_call_preconditions() {
    use std::collections::BTreeMap;
    use vera_compiler::projection::build_affordance_frame;
    let p=checked("function positive(x:u64)->bool require nonzero:gt(x,0); body {yield true;} projection V(x:u64) {allow_if:positive(x);expose:[];enable_intents:[];}");
    let m = Machine::new(&p).unwrap();
    assert!(
        build_affordance_frame(&p, &m.state, "V", &BTreeMap::from([("x".into(), int(1))]))
            .unwrap()
            .allowed
    );
    assert!(
        !build_affordance_frame(&p, &m.state, "V", &BTreeMap::from([("x".into(), int(0))]))
            .unwrap()
            .allowed
    );
}
