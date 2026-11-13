use vera_compiler::contract::{
    compare_source, eligible_bodies, freeze_source, BodyTarget, FrozenContract,
};

const SOURCE: &str = r#"
module Frozen;
table T capacity 2 {key id:id64<T>;col v:u8;}
invariant bound {forall k in T:le(T[k].v,10);}
effect Changed(v:u8);
transition go(k:id64<T>,v:u8) reads [T[k].v] writes [T[k].v]
require live:table_exists(T,k);
require bound:le(v,10);
ensure exact:eq(table_get(T,k,v),v);
ensure count:eq(effect_count(),1);
ensure kind:effect_is(Changed,0);
ensure arg:eq(effect_arg(Changed,0,v),v);
body {table_set(T,k,v,v);emit Changed(v);commit;}
"#;
fn transition() -> Vec<BodyTarget> {
    vec![BodyTarget::Transition { name: "go".into() }]
}

#[test]
fn roundtrip_is_deterministic_and_whitespace_is_not_contract() {
    let frozen = freeze_source(SOURCE, &transition()).unwrap();
    let serialized = serde_json::to_string(&frozen).unwrap();
    let restored: FrozenContract = serde_json::from_str(&serialized).unwrap();
    restored.validate().unwrap();
    assert_eq!(frozen, restored);
    assert!(
        compare_source(&frozen, &SOURCE.replace(";", ";\n"))
            .unwrap()
            .matches
    );
    assert_eq!(
        frozen.canonical,
        freeze_source(SOURCE, &transition()).unwrap().canonical
    );
}

#[test]
fn only_explicitly_authorized_bodies_are_mutable() {
    let changed = SOURCE.replace(
        "table_set(T,k,v,v);",
        "guard(true,ERR_OK);table_set(T,k,v,v);",
    );
    assert!(
        compare_source(&freeze_source(SOURCE, &transition()).unwrap(), &changed)
            .unwrap()
            .matches
    );
    assert!(
        !compare_source(&freeze_source(SOURCE, &[]).unwrap(), &changed)
            .unwrap()
            .matches
    );
    // Contract matching is not verification: this valid but wrong body is permitted.
    let wrong = SOURCE.replace("table_set(T,k,v,v);emit Changed(v);", "");
    assert!(
        compare_source(&freeze_source(SOURCE, &transition()).unwrap(), &wrong)
            .unwrap()
            .matches
    );
}

#[test]
fn attempted_contract_weakening_is_reported_not_admitted() {
    let frozen = freeze_source(SOURCE, &transition()).unwrap();
    for changed in [
        SOURCE.replace("capacity 2", "capacity 4"),
        SOURCE
            .replace("col v:u8", "col v:u16")
            .replace("v:u8", "v:u16"),
        SOURCE.replace("require bound:le(v,10);", "require bound:true;"),
        SOURCE.replace("ensure exact:eq(table_get(T,k,v),v);", "ensure exact:true;"),
        SOURCE.replace("le(T[k].v,10)", "true"),
        SOURCE.replace("reads [T[k].v]", "reads []"),
        SOURCE.replace("ensure count:eq(effect_count(),1);", ""),
        SOURCE.replace("module Frozen;", "module Other;"),
    ] {
        let result = compare_source(&frozen, &changed).unwrap();
        assert!(!result.matches, "{changed}");
        assert!(!result.differences.is_empty());
        assert!(result.differences.iter().all(|d| d.path.starts_with('/')));
    }
}

#[test]
fn invalid_candidate_policy_or_frozen_document_fails_closed() {
    let frozen = freeze_source(SOURCE, &transition()).unwrap();
    assert!(compare_source(&frozen, "invalid vera").is_err());
    assert!(freeze_source(
        SOURCE,
        &[BodyTarget::Function {
            name: "missing".into()
        }]
    )
    .is_err());
    let mut duplicate = transition();
    duplicate.extend(transition());
    assert!(freeze_source(SOURCE, &duplicate).is_err());
    let mut invalid = frozen.clone();
    invalid.schema_version = 2;
    assert!(compare_source(&invalid, SOURCE).is_err());
    invalid = frozen.clone();
    invalid.canonical["decls"] = serde_json::json!([]);
    assert!(compare_source(&invalid, SOURCE).is_err());
    let mut json = serde_json::to_value(frozen).unwrap();
    json["unrecognized"] = true.into();
    assert!(serde_json::from_value::<FrozenContract>(json).is_err());
}

#[test]
fn components_and_function_contracts_are_frozen() {
    let source = include_str!("../../../examples/components.vera");
    let policy = eligible_bodies(source).unwrap();
    let frozen = freeze_source(source, &policy).unwrap();
    let candidate = source.replace("emit Tick(add(%v,1));", "emit Tick(%v);");
    assert!(compare_source(&frozen, &candidate).unwrap().matches);
    let candidate = source.replace("ensure count:eq(effect_count(),1);", "ensure count:true;");
    assert!(!compare_source(&frozen, &candidate).unwrap().matches);
    let source = include_str!("../../../examples/functions.vera");
    let frozen = freeze_source(source, &eligible_bodies(source).unwrap()).unwrap();
    assert!(
        !compare_source(&frozen, &source.replace("lt(x, 1000)", "lt(x, 1001)"))
            .unwrap()
            .matches
    );
}

#[test]
fn helper_bodies_cannot_indirectly_redefine_frozen_contracts() {
    let source = r#"
function inner(x:u8)->bool body {yield le(x,10);}
function outer(x:u8)->bool body {yield inner(x);}
function worker(x:u8)->u8 ensure identity:eq(result,x); body {yield x;}
transition go(x:u8) reads [] writes [] require allowed:outer(x); body {commit;}
"#;
    let policy = eligible_bodies(source).unwrap();
    assert_eq!(policy.len(), 2);
    assert!(policy.contains(&BodyTarget::Function {
        name: "worker".into()
    }));
    let frozen = freeze_source(source, &policy).unwrap();
    assert!(
        !compare_source(&frozen, &source.replace("le(x,10)", "true"))
            .unwrap()
            .matches
    );
    assert!(freeze_source(
        source,
        &[BodyTarget::Function {
            name: "outer".into()
        }]
    )
    .is_err());
    assert!(freeze_source(
        source,
        &[BodyTarget::Function {
            name: "inner".into()
        }]
    )
    .is_err());
}

#[test]
fn cli_freezes_compares_and_rejects_policy_override() {
    use std::{
        fs,
        io::Write,
        process::{Command, Output, Stdio},
    };
    fn run(args: &[&str], source: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
            .args(args)
            .env("PATH", "")
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
        child.wait_with_output().unwrap()
    }
    let result = run(&["contract", "-", "--json"], SOURCE);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let frozen: FrozenContract = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(frozen.editable_bodies, transition());
    let path = std::env::temp_dir().join(format!("vera-contract-test-{}.json", std::process::id()));
    fs::write(&path, &result.stdout).unwrap();
    let filename = path.to_str().unwrap();
    let same = run(&["contract", "-", "--frozen", filename], SOURCE);
    assert!(
        same.status.success(),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&same.stdout).unwrap()["matches"],
        true
    );
    let weakened = run(
        &["contract", "-", "--frozen", filename],
        &SOURCE.replace("require bound:le(v,10);", "require bound:true;"),
    );
    assert!(!weakened.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&weakened.stdout).unwrap()["matches"],
        false
    );
    let override_policy = run(
        &[
            "contract",
            "-",
            "--frozen",
            filename,
            "--editable",
            filename,
        ],
        SOURCE,
    );
    assert!(!override_policy.status.success());
    fs::write(&path, "{invalid").unwrap();
    assert!(!run(&["contract", "-", "--frozen", filename], SOURCE)
        .status
        .success());
    fs::remove_file(&path).unwrap();
}
