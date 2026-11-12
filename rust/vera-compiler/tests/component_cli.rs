use std::{
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
        .ok();
    child.wait_with_output().unwrap()
}
fn source() -> &'static str {
    include_str!("fixtures/components.vera")
        .split("transition twice")
        .next()
        .unwrap()
}
#[test]
fn selected_and_full_operation_only_proofs_are_not_vacuous() {
    for args in [
        vec!["verify", "-", "--json"],
        vec!["verify", "-", "--json", "--operation", "Counter.inc"],
    ] {
        let out = run(&args, source());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["target_kind"], "operation");
        assert_eq!(result["results"][0]["transition"], "Counter.inc");
    }
}
#[test]
fn invalid_uncalled_operations_block_every_codegen_target() {
    let source = source().replace(
        "ensure count:eq(effect_count(),1);",
        "ensure count:eq(effect_count(),2);",
    );
    for target in ["rust", "glsl", "wgsl", "cuda"] {
        let out = run(&["codegen", target, "-"], &source);
        assert!(!out.status.success(), "{target}");
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("not verified"));
    }
}
#[test]
fn operation_selection_is_exclusive_and_check_reports_ownership() {
    let out = run(
        &[
            "verify",
            "-",
            "--operation",
            "Counter.inc",
            "--transition",
            "nope",
        ],
        source(),
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("select one"));
    let out = run(&["check", "-", "--json"], source());
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["components"][0]["name"], "Counter");
    assert_eq!(json["components"][0]["owns"][0], "T");
    assert_eq!(json["operations"][0]["effects"][0], "Tick");
}
