use std::{
    io::Write,
    process::{Command, Stdio},
};
fn cli(source: &str, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    c.wait_with_output().unwrap()
}
#[test]
fn function_only_program_cannot_vacuously_pass_cli_verify_or_codegen() {
    let source = "function wrong(x:u8)->u8 ensure bad:eq(result,1);body {yield 0;}";
    let output = cli(source, &["verify", "-", "--json"]);
    assert!(!output.status.success());
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["results"][0]["target_kind"], "function");
    assert_eq!(data["ok"], false);
    let output = cli(source, &["codegen", "rust", "-", "--no-main"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("function"));
}
#[test]
fn cli_verifies_and_reports_each_function_once_and_supports_selected_helper() {
    let source="function good(x:u8)->u8 ensure exact:eq(result,x);body {yield x;}function bad(x:u8)->u8 ensure wrong:eq(result,1);body {yield 0;}transition go(x:u8) reads [] writes [] ensure same:eq(good(x),x);body {commit;}";
    let output = cli(source, &["verify", "-", "--function", "good", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["results"].as_array().unwrap().len(), 1);
    assert_eq!(data["results"][0]["target_kind"], "function");
    let output = cli(source, &["verify", "-", "--json"]);
    assert!(!output.status.success());
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["results"].as_array().unwrap().len(), 3);
    let output = cli(
        source,
        &["verify", "-", "--function", "good", "--transition", "go"],
    );
    assert!(!output.status.success());
}
