use std::{
    io::Write,
    process::{Command, Stdio},
};
fn run(source: &str, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args(["run", "-"])
        .args(args)
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
fn succeeds(source: &str, args: &[&str]) -> serde_json::Value {
    let out = run(source, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn pure_function_proves_dependencies_and_returns_typed_result() {
    let source="function inc(x:u64)->u64 require room:lt(x,18446744073709551615);ensure exact:eq(result,add(x,1));body{yield add(x,1);}function twice(x:u64)->u64 require room:lt(x,18446744073709551614);ensure exact:eq(result,add(x,2));body{yield inc(inc(x));}";
    let result = succeeds(
        source,
        &["--function", "twice", "--param", "x=40", "--json"],
    );
    assert_eq!(result["ok"], true);
    assert_eq!(result["proof_admitted"], true);
    assert_eq!(result["function"], "twice");
    assert_eq!(result["result_type"], "u64");
    assert_eq!(result["result"], 42);
    assert_eq!(result["error"], serde_json::Value::Null);
}
#[test]
fn false_callee_contract_is_never_assumed_by_execution_gate() {
    let source="function bad()->u8 ensure wrong:eq(result,1);body{yield 0;}function caller()->u8 ensure inherited:eq(result,1);body{yield bad();}";
    let out = run(source, &["--function", "caller", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not verified"));
    let result = succeeds(source, &["--function", "caller", "--json", "--unverified"]);
    assert_eq!(result["proof_admitted"], false);
    assert_eq!(result["result"], 0);
}
#[test]
fn structured_records_unions_and_bytes_roundtrip_without_state() {
    let source="record Packet{data:bytes<4>;amount:u64;}union Choice{Packet:Packet;Number:u64;}function identity(x:Choice)->Choice body{yield x;}";
    let input = r#"{"tag":"Packet","payload":{"data":"0x0102","amount":42}}"#;
    let argument = format!("x={input}");
    let result = succeeds(
        source,
        &["--function", "identity", "--param", &argument, "--json"],
    );
    assert_eq!(
        result["result"],
        serde_json::from_str::<serde_json::Value>(input).unwrap()
    );
    let bytes = succeeds(
        "function identity(x:bytes<4>)->bytes<4> body{yield x;}",
        &["--function", "identity", "--param", "x=0x0102", "--json"],
    );
    assert_eq!(bytes["result"], "0x0102");
}
#[test]
fn scalar_formats_preserve_existing_parameter_semantics() {
    let result = succeeds(
        "function identity(x:dec64)->dec64 body{yield x;}",
        &["--function", "identity", "--param", "x=1.25", "--json"],
    );
    assert_eq!(result["result"], 125000000);
    let result = succeeds(
        "function identity(x:enum {Ready,Done})->enum {Ready,Done} body{yield x;}",
        &["--function", "identity", "--param", "x=Done", "--json"],
    );
    assert_eq!(result["result"], 1);
}
#[test]
fn failed_runtime_precondition_is_not_reported_as_proof_failure() {
    let out = run(
        "function positive(x:u8)->u8 require positive:gt(x,0);body{yield x;}",
        &["--function", "positive", "--param", "x=0", "--json"],
    );
    assert_eq!(out.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["proof_admitted"], true);
    assert_eq!(result["result"], serde_json::Value::Null);
    assert!(result["error"]
        .to_string()
        .contains("ERR_FUNCTION_PRECONDITION"));
}
#[test]
fn invalid_arguments_and_stateful_option_combinations_are_rejected() {
    let source = "function identity(x:u8)->u8 body{yield x;}";
    for extra in [
        vec!["--transition", "anything"],
        vec!["--setup", "not-opened"],
        vec!["--snapshot-in", "not-opened"],
        vec!["--snapshot-out", "not-written"],
        vec!["--wal", "not-written"],
    ] {
        let mut args = vec!["--function", "identity", "--param", "x=1"];
        args.extend(extra);
        let out = run(source, &args);
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be combined"));
    }
    for args in [
        vec!["--function", "missing"],
        vec!["--function", "identity"],
        vec!["--function", "identity", "--param", "x=256"],
        vec!["--function", "identity", "--param", "y=1"],
        vec!["--function", "identity", "--param", "x=1", "--param", "x=2"],
    ] {
        assert_eq!(run(source, &args).status.code(), Some(2));
    }
}
#[test]
fn transition_execution_is_unchanged() {
    let result = succeeds(
        "transition noop() reads [] writes [] body{commit;}",
        &["--transition", "noop", "--json"],
    );
    assert_eq!(result["ok"], true);
    assert!(result.get("state").is_some());
}

#[test]
fn proof_scope_excludes_unrelated_broken_function() {
    let source="function unrelated()->u8 ensure wrong:eq(result,1);body{yield 0;}function identity(x:u8)->u8 ensure exact:eq(result,x);body{yield x;}";
    let result = succeeds(
        source,
        &["--function", "identity", "--param", "x=42", "--json"],
    );
    assert_eq!(result["proof_admitted"], true);
    assert_eq!(result["result"], 42);
}

#[test]
fn plain_output_is_the_value_without_status_wrapper() {
    let result = succeeds(
        "function answer()->u8 body{yield 42;}",
        &["--function", "answer"],
    );
    assert_eq!(result, 42);
}

#[test]
fn pure_execution_ignores_unrelated_invalid_initial_state() {
    let source="table T capacity 1 {key id:id64<T>;col v:u64 [tracked_sum];}invariant populated {eq(sum(T.v),1);}function answer()->u8 ensure correct:eq(result,42);body{yield 42;}transition noop() reads [] writes [] body{commit;}";
    let result = succeeds(source, &["--function", "answer", "--json"]);
    assert_eq!(result["proof_admitted"], true);
    assert_eq!(result["result"], 42);
    // The empty table actually violates its invariant; stateful execution must reject it.
    let transition = run(source, &["--transition", "noop", "--json", "--unverified"]);
    assert!(!transition.status.success());
}
