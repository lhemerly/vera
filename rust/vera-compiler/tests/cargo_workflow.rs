//! Exercise the public Cargo workflow, not just successful compiler output.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn cargo(root: &Path, args: &[&str]) -> Output {
    Command::new("cargo")
        .args(args)
        .arg("--locked")
        .current_dir(root)
        .env("VERA", env!("CARGO_BIN_EXE_vera"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap()
}
fn passed(out: Output) {
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn cargo_build_proves_core_and_host_tests_detect_disconnection_and_mismapping() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let source = root.join("examples/parcel-quote");
    let temp =
        Scratch(std::env::temp_dir().join(format!("vera-cargo-workflow-{}", std::process::id())));
    fs::create_dir_all(temp.0.join("src")).unwrap();
    fs::create_dir_all(temp.0.join("tests")).unwrap();
    for name in [
        "build.rs",
        "Cargo.lock",
        "policy.vera",
        "src/main.rs",
        "tests/cli.rs",
    ] {
        fs::copy(source.join(name), temp.0.join(name)).unwrap();
    }
    let manifest = fs::read_to_string(source.join("Cargo.toml"))
        .unwrap()
        .replace(
            "\"../../rust/vera-build\"",
            &format!("{:?}", root.join("rust/vera-build").to_str().unwrap()),
        );
    fs::write(temp.0.join("Cargo.toml"), manifest).unwrap();
    passed(cargo(&temp.0, &["test", "--offline"]));
    let host_path = temp.0.join("src/main.rs");
    let host = fs::read_to_string(&host_path).unwrap();
    let call = "call_shipping_fee(weight, express, subtotal)";
    for mutant in [
        "call_shipping_fee(weight, express, subtotal).map(|_ignored| 500)",
        "call_shipping_fee(subtotal, express, weight)",
    ] {
        let changed = host.replace(call, mutant);
        assert_ne!(host, changed);
        fs::write(&host_path, changed).unwrap();
        // Both mutants must compile. A compilation error is not behavioral detection.
        passed(cargo(&temp.0, &["test", "--offline", "--no-run"]));
        let failed = cargo(&temp.0, &["test", "--offline", "--test", "cli"]);
        assert!(!failed.status.success(), "mutant survived: {mutant}");
        assert!(String::from_utf8_lossy(&failed.stdout)
            .contains("verified_price_is_the_observable_result ... FAILED"));
    }
    fs::write(&host_path, host).unwrap();
    passed(cargo(&temp.0, &["test", "--offline"]));
    // Changing only policy source must invalidate Cargo's previous successful build.
    let policy_path = temp.0.join("policy.vera");
    let policy = fs::read_to_string(&policy_path).unwrap();
    fs::write(
        &policy_path,
        policy.replace(
            "ensure bounded_fee : le(result, 1500)",
            "ensure bounded_fee : le(result, 1)",
        ),
    )
    .unwrap();
    let failed = cargo(&temp.0, &["build", "--offline"]);
    assert!(
        !failed.status.success(),
        "unsound policy reused stale generated core"
    );
    // Repair and rebuild work without deleting the target directory manually.
    fs::write(&policy_path, policy).unwrap();
    passed(cargo(&temp.0, &["test", "--offline"]));
}
