#![cfg(unix)]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use vera_build::Build;
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    source: PathBuf,
    compiler: PathBuf,
    out: PathBuf,
}
impl Fixture {
    fn new(extra: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "vera-build-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("policy.vera");
        let compiler = root.join("fake-compiler");
        let out = root.join("out");
        fs::write(&source, "module Policy;").unwrap();
        fs::write(&compiler, format!(r##"#!/bin/sh
set -eu
[ "$1" = codegen ] && [ "$2" = rust ] && [ "$4" = --no-main ] && [ "$5" = --out ] && [ "$7" = --api-out ]
[ "$#" = 8 ]
{extra}
printf '#![allow(dead_code)]\npub fn value()->u64{{7}}\n' > "$6"
printf '{{"proof_admitted":true,"proof_scope":"checked_program_not_host_integration","api":{{"schema_version":1,"target":"rust","functions":[],"program_fingerprint":"fake-only-unit-test"}}}}' > "$8"
"##)).unwrap();
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            source,
            compiler,
            out,
        }
    }
    fn builder(&self) -> Build {
        Build::new(&self.source)
            .compiler(&self.compiler)
            .out_dir(&self.out)
            .module_name("policy")
    }
    fn assert_empty(&self) {
        assert_eq!(fs::read_dir(&self.out).unwrap().count(), 0);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn emits_exact_bound_artifacts_and_usable_module_binder() {
    let f = Fixture::new("");
    let b = f.builder().compile().unwrap();
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(&b.receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["host_result_use"], "not_checked");
    assert_eq!(receipt["program_fingerprint"], "fake-only-unit-test");
    assert_eq!(receipt["host_verification"], "not_performed");
    use sha2::{Digest, Sha256};
    for (key, path) in [
        ("source_sha256", &f.source),
        ("compiler_sha256", &f.compiler),
        ("generated_rust_sha256", &b.rust_path),
        ("api_manifest_sha256", &b.api_path),
        ("module_binder_sha256", &b.module_path),
    ] {
        let expected: String = Sha256::digest(fs::read(path).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(receipt[key], expected);
    }
    let main = f.root.join("main.rs");
    fs::write(
        &main,
        format!(
            "include!({:?}); fn main() {{assert_eq!(policy::value(),7);}}",
            b.module_path.to_str().unwrap()
        ),
    )
    .unwrap();
    let exe = f.root.join("demo");
    assert!(std::process::Command::new("rustc")
        .arg(&main)
        .arg("-o")
        .arg(&exe)
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new(exe).status().unwrap().success());
}
#[test]
fn failed_proof_clears_old_outputs_and_receipt() {
    let f = Fixture::new("");
    f.builder().compile().unwrap();
    fs::write(
        &f.compiler,
        "#!/bin/sh\necho 'unknown is not proof' >&2\nexit 1\n",
    )
    .unwrap();
    let error = f.builder().compile().unwrap_err().to_string();
    assert!(error.contains("unknown is not proof"));
    f.assert_empty();
}
#[test]
fn missing_compiler_clears_old_outputs() {
    let f = Fixture::new("");
    f.builder().compile().unwrap();
    fs::remove_file(&f.compiler).unwrap();
    assert!(f.builder().compile().is_err());
    f.assert_empty();
}
#[test]
fn missing_source_clears_old_outputs() {
    let f = Fixture::new("");
    f.builder().compile().unwrap();
    fs::remove_file(&f.source).unwrap();
    assert!(f.builder().compile().is_err());
    f.assert_empty();
}
#[test]
fn changed_source_during_generation_is_rejected() {
    let f = Fixture::new("");
    let script = fs::read_to_string(&f.compiler).unwrap();
    let script = script.replace(
        "set -eu",
        &format!(
            "set -eu\nprintf 'module Changed;' > '{}'",
            f.source.display()
        ),
    );
    fs::write(&f.compiler, script).unwrap();
    assert!(f
        .builder()
        .compile()
        .unwrap_err()
        .to_string()
        .contains("changed during generation"));
    f.assert_empty();
}
#[test]
fn malformed_manifest_is_rejected() {
    let f = Fixture::new("printf 'garbage' > \"$8\"; printf '// code' > \"$6\"; exit 0");
    assert!(f.builder().compile().is_err());
    f.assert_empty();
}
#[test]
fn output_alias_does_not_delete_source() {
    let f = Fixture::new("");
    assert!(Build::new(&f.source)
        .compiler(&f.compiler)
        .out_dir(&f.root)
        .module_name("policy")
        .compile()
        .is_ok());
    let source = f.root.join("policy.rs");
    let before = fs::read(&source).unwrap();
    assert!(Build::new(&source)
        .compiler(&f.compiler)
        .out_dir(&f.root)
        .module_name("policy")
        .compile()
        .is_err());
    assert_eq!(fs::read(source).unwrap(), before);
}
#[test]
fn invalid_module_names_rejected() {
    let f = Fixture::new("");
    for name in ["", "../bad", "a-b", "type", "9abc", "_"] {
        assert!(f.builder().module_name(name).compile().is_err());
    }
}
#[test]
fn compiler_alias_does_not_delete_executable() {
    let f = Fixture::new("");
    fs::create_dir(&f.out).unwrap();
    let alias = f.out.join("policy.rs");
    fs::copy(&f.compiler, &alias).unwrap();
    let before = fs::read(&alias).unwrap();
    assert!(f
        .builder()
        .compiler(&alias)
        .compile()
        .unwrap_err()
        .to_string()
        .contains("compiler aliases"));
    assert_eq!(fs::read(&alias).unwrap(), before);
}
#[test]
fn missing_source_cannot_delete_aliased_compiler() {
    let f = Fixture::new("");
    fs::create_dir(&f.out).unwrap();
    let compiler = f.out.join("policy.rs");
    fs::copy(&f.compiler, &compiler).unwrap();
    fs::remove_file(&f.source).unwrap();
    let bytes = fs::read(&compiler).unwrap();
    assert!(f.builder().compiler(&compiler).compile().is_err());
    assert_eq!(fs::read(compiler).unwrap(), bytes);
}
#[test]
fn missing_compiler_cannot_delete_aliased_source() {
    let f = Fixture::new("");
    fs::create_dir(&f.out).unwrap();
    let source = f.out.join("policy.rs");
    fs::copy(&f.source, &source).unwrap();
    fs::remove_file(&f.compiler).unwrap();
    let bytes = fs::read(&source).unwrap();
    assert!(Build::new(&source)
        .compiler(&f.compiler)
        .out_dir(&f.out)
        .module_name("policy")
        .compile()
        .is_err());
    assert_eq!(fs::read(source).unwrap(), bytes);
}

#[test]
fn absent_false_or_wrong_scope_admission_is_rejected_and_clears_outputs() {
    for replacement in [
        r#""proof_admitted":false"#,
        r#""other_field":true"#,
        r#""proof_admitted":null"#,
        r#""proof_admitted":"true""#,
    ] {
        let f = Fixture::new("");
        f.builder().compile().unwrap();
        let script = fs::read_to_string(&f.compiler).unwrap();
        fs::write(
            &f.compiler,
            script.replace(r#""proof_admitted":true"#, replacement),
        )
        .unwrap();
        assert!(f
            .builder()
            .compile()
            .unwrap_err()
            .to_string()
            .contains("did not admit"));
        f.assert_empty();
    }
    let f = Fixture::new("");
    f.builder().compile().unwrap();
    let script = fs::read_to_string(&f.compiler).unwrap();
    fs::write(
        &f.compiler,
        script.replace("checked_program_not_host_integration", "host_verified"),
    )
    .unwrap();
    assert!(f
        .builder()
        .compile()
        .unwrap_err()
        .to_string()
        .contains("did not admit"));
    f.assert_empty();
}
