//! Black-box native CLI checks: no Python executable is available to children.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], source: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args(args)
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch native vera with an empty PATH");
    if let Err(error) = child.stdin.take().unwrap().write_all(source.as_bytes()) {
        // Argument errors can close stdin before the parent writes its input.
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().expect("wait for native vera")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

#[test]
fn parses_stdin_and_emits_only_canonical_json_without_python() {
    let output = run(
        &["parse", "-", "--json"],
        "module Native;table T capacity 2 {key id:id64<T>;col v:u64;}\n\
         transition noop() reads [] writes [] body {commit;}",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    let text: String = stdout(&output)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(text.starts_with('{') && text.ends_with('}'), "{text}");
    assert!(text.contains("\"node\":\"Module\""), "{text}");
    assert!(text.contains("\"name\":\"Native\""), "{text}");
    assert!(text.contains("\"node\":\"TableDecl\""), "{text}");
    assert!(text.contains("\"node\":\"TransitionDecl\""), "{text}");
    assert!(!text.contains("\"line\":"), "{text}");
    assert!(!text.contains("\"col\":"), "{text}");
}

#[test]
fn retains_full_width_integer_and_decimal_source_values() {
    let output = run(
        &["parse", "-", "--json"],
        "module Exact;table T capacity 2 {key id:id64<T>;col v:dec128;col code:bytes32;}\n\
         transition create() reads [] writes [T[*].v,T[*].code] body {\n\
         let %v:dec128=1.000000000000000001;\n\
         let %k:id64<T>=table_alloc(T,{v:%v,code:0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff});commit;}",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text: String = stdout(&output)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        text.contains("\"integer\":\"115792089237316195423570985008687907853269984665640564039457584007913129639935\""),
        "{text}"
    );
    assert!(
        text.contains("\"digits\":\"1000000000000000001\""),
        "{text}"
    );
    assert!(text.contains("\"scale\":18"), "{text}");
}

#[test]
fn human_summary_identifies_module_and_declarations() {
    let output = run(
        &["parse", "-"],
        "module Native;table T capacity 2 {key id:id64<T>;col v:u64;}\n\
         transition noop() reads [] writes [] body {commit;}",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Native"), "{text}");
    assert!(text.to_lowercase().contains("table"), "{text}");
    assert!(text.to_lowercase().contains("transition"), "{text}");
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[test]
fn invalid_syntax_reports_a_location_without_partial_json() {
    let output = run(&["parse", "-", "--json"], "module Bad;\n@\n");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{}", stdout(&output));
    let error = stderr(&output);
    assert!(error.contains("D_PARSE:2:1"), "{error}");
    assert!(!error.contains("panicked"), "{error}");
}

#[test]
fn unsupported_compiler_commands_fail_explicitly_without_python() {
    for command in ["check", "verify", "run", "codegen"] {
        let output = run(&[command, "-"], "module Native;");
        assert!(!output.status.success(), "{command} unexpectedly succeeded");
        assert!(output.stdout.is_empty(), "{}", stdout(&output));
        let error = stderr(&output).to_lowercase();
        assert!(
            error.contains("unsupported")
                || error.contains("not implemented")
                || error.contains("not yet")
                || (error.contains("only") && error.contains("parse")),
            "{command}: {error}"
        );
        assert!(!error.contains("panicked"), "{command}: {error}");
    }
}

#[test]
fn help_and_version_work_without_python() {
    let help = run(&["--help"], "");
    assert!(help.status.success(), "{}", stderr(&help));
    assert!(stdout(&help).contains("parse"));
    assert!(stdout(&help).contains("--json"));
    let version = run(&["--version"], "");
    assert!(version.status.success(), "{}", stderr(&version));
    assert!(stdout(&version).to_lowercase().contains("vera"));
}

#[test]
fn missing_files_and_unrecognized_options_fail_without_panics() {
    for args in [
        vec!["parse", "/__vera_missing_input__/missing.vera"],
        vec!["parse", "-", "--unrecognized"],
        vec!["parse"],
    ] {
        let output = run(&args, "module Native;");
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(output.stdout.is_empty(), "{}", stdout(&output));
        assert!(!output.stderr.is_empty());
        assert!(!stderr(&output).contains("panicked"));
    }
}

#[cfg(unix)]
#[test]
fn parses_utf8_source_from_a_non_utf8_path_without_python() {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, OpenOptions};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct SourceFile(PathBuf);
    impl Drop for SourceFile {
        fn drop(&mut self) {
            fs::remove_file(&self.0).expect("remove native CLI test input");
        }
    }

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut name = OsString::from(format!("vera-native-{}-{unique}-", std::process::id()));
    name.push(OsStr::from_bytes(b"\xff.vera"));
    let source = SourceFile(std::env::temp_dir().join(name));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source.0)
        .unwrap();
    file.write_all(b"module Filename;table T capacity 2 {key id:id64<T>;col v:u64;}")
        .unwrap();
    drop(file);

    let output = Command::new(env!("CARGO_BIN_EXE_vera"))
        .arg("parse")
        .arg(&source.0)
        .arg("--json")
        .env("PATH", "")
        .output()
        .expect("launch native compiler for non-UTF-8 path");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    let module: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(module["node"], "Module");
    assert_eq!(module["name"], "Filename");
}
