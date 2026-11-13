//! Exercise the real host executable, including an intentionally disconnected consumer.
use std::{fs, path::PathBuf, process::Command};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn compile(folder: &Scratch, name: &str) -> PathBuf {
    let binary = folder.0.join(name);
    let output = Command::new("rustc")
        .args(["--edition", "2021"])
        .arg(folder.0.join("main.rs"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

fn cases_match(binary: &PathBuf) -> bool {
    [
        (["1000", "false", "4000"], "{\"shipping_cents\":500}\n"),
        (["1000", "false", "5000"], "{\"shipping_cents\":0}\n"),
        (["1000", "true", "5000"], "{\"shipping_cents\":1500}\n"),
        (["20000", "false", "0"], "{\"shipping_cents\":500}\n"),
    ]
    .iter()
    .all(|(args, expected)| {
        let out = Command::new(binary).args(args).output().unwrap();
        out.status.success() && out.stdout == expected.as_bytes() && out.stderr.is_empty()
    })
}

#[test]
fn shipping_example_uses_verified_value_and_detects_discarded_value() {
    let folder =
        Scratch(std::env::temp_dir().join(format!("vera-host-example-{}", std::process::id())));
    fs::create_dir_all(&folder.0).unwrap();
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/shipping");
    let generated = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args(["codegen", "rust"])
        .arg(example.join("policy.vera"))
        .args(["--no-main", "--out"])
        .arg(folder.0.join("verified.rs"))
        .arg("--api-out")
        .arg(folder.0.join("api.json"))
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let host = fs::read_to_string(example.join("main.rs")).unwrap();
    fs::write(folder.0.join("main.rs"), &host).unwrap();
    let binary = compile(&folder, "shipping");
    assert!(cases_match(&binary));
    for args in [
        ["0", "false", "4000"],
        ["20001", "true", "0"],
        ["bad", "false", "100"],
    ] {
        let out = Command::new(&binary).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
    // Still invokes generated code, but discards its value: calling it is not adoption.
    let mutant = host.replace(
        "call_shipping_fee(weight, express, subtotal)",
        "call_shipping_fee(weight, express, subtotal).map(|_ignored| 500)",
    );
    assert_ne!(mutant, host);
    fs::write(folder.0.join("main.rs"), mutant).unwrap();
    assert!(!cases_match(&compile(&folder, "constant-consumer")));
}
