//! Fixed language and execution vectors exercise the native toolchain without
//! depending on another compiler or scripting runtime at test time.

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use vera_compiler::parse_source;

fn fixture(text: &str) -> Value {
    let value: Value = serde_json::from_str(text).expect("valid conformance vectors");
    assert_eq!(value["version"], 1, "unknown fixture format");
    value
}

fn array(value: &Value) -> &[Value] {
    value.as_array().expect("fixture array")
}

fn text(value: &Value) -> &str {
    value.as_str().expect("fixture string")
}

fn index(value: &Value) -> usize {
    usize::try_from(value.as_u64().expect("fixture index")).expect("host fixture index")
}

fn execute(command: &mut Command, input: Option<&str>) -> Output {
    let mut child = command
        .env("PATH", "")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch native CLI with empty PATH");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("source input pipe")
            .write_all(input.as_bytes())
            .expect("write source");
    }
    child.wait_with_output().expect("complete native CLI")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn complete_source_corpus_preserves_exact_syntax_and_semantic_contracts() {
    let fixture = fixture(include_str!("fixtures/conformance/sources.json"));
    let cases = array(&fixture["cases"]);
    let metadata = array(&fixture["metadata"]);
    assert_eq!(cases.len(), 148);
    assert_eq!(metadata.len(), 118);
    let mut outcomes = [0; 3];
    let mut unique = BTreeSet::new();
    for (case_index, case) in cases.iter().enumerate() {
        let case = array(case);
        assert_eq!(case.len(), 4);
        let source = text(&case[0]);
        assert!(unique.insert(source), "duplicate source {case_index}");
        let outcome = index(&case[3]);
        assert!(outcome < outcomes.len(), "unknown source outcome");
        outcomes[outcome] += 1;
        let parsed = parse_source(source);
        if outcome == 2 {
            let error = parsed.expect_err("invalid syntax must fail");
            assert_eq!(error.code, "D_PARSE", "source {case_index}");
            assert!(error.span.line > 0 && error.span.column > 0);
            assert!(case[1].is_null() && case[2].is_null());
        } else {
            let module = parsed
                .unwrap_or_else(|error| panic!("source {case_index} rejected: {error}\n{source}"));
            let canonical = serde_json::to_vec(&module.canonical_json()).unwrap();
            let hash = format!("{:x}", Sha256::digest(canonical));
            assert_eq!(hash, text(&case[1]), "source {case_index}: canonical AST");
        }
        let result = execute(
            Command::new(env!("CARGO_BIN_EXE_vera")).args(["check", "-", "--json"]),
            Some(source),
        );
        if outcome == 0 {
            assert!(
                result.status.success(),
                "source {case_index}: {}",
                stderr(&result)
            );
            assert!(
                result.stderr.is_empty(),
                "source {case_index}: {}",
                stderr(&result)
            );
            let actual: Value = serde_json::from_slice(&result.stdout).expect("check JSON");
            assert_eq!(
                actual,
                metadata[index(&case[2])],
                "source {case_index}: checked metadata"
            );
        } else {
            assert!(!result.status.success(), "source {case_index} admitted");
            assert!(
                result.stdout.is_empty(),
                "source {case_index}: partial JSON"
            );
            assert!(
                stderr(&result).contains("D_"),
                "source {case_index}: missing diagnostic"
            );
        }
    }
    assert_eq!(outcomes, [129, 15, 4]);
}

struct Temporary(PathBuf);

impl Temporary {
    fn new() -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "vera-conformance-{}-{timestamp}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create conformance directory");
        Self(path)
    }

    fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).expect("write conformance input");
        path
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove conformance directory");
    }
}

fn command(source: &Path, transition: &str, projection: bool, params: &Value) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vera"));
    command
        .arg(if projection { "project" } else { "run" })
        .arg(source)
        .arg("--json");
    if projection {
        command.arg("--projection");
    } else {
        command.args(["--unverified", "--transition"]);
    }
    command.arg(transition);
    for param in array(params) {
        command.arg("--param").arg(text(param));
    }
    command
}

#[test]
fn scalar_boundaries_transactions_and_projections_match_fixed_execution_vectors() {
    let fixture = fixture(include_str!("fixtures/conformance/execution.json"));
    let programs = array(&fixture["programs"]);
    let transitions = array(&fixture["transitions"]);
    let inputs = array(&fixture["inputs"]);
    let setups = array(&fixture["setups"]);
    let outputs = array(&fixture["outputs"]);
    let cases = array(&fixture["cases"]);
    assert_eq!(programs.len(), 12);
    assert_eq!(cases.len(), 1968);
    let folder = Temporary::new();
    let paths: Vec<_> = programs
        .iter()
        .enumerate()
        .map(|(i, program)| {
            folder.write(
                &format!("program-{i}.vera"),
                text(&program["source"]).as_bytes(),
            )
        })
        .collect();
    let setups: Vec<_> = setups
        .iter()
        .enumerate()
        .map(|(i, setup)| {
            if setup.is_null() {
                None
            } else {
                Some(folder.write(
                    &format!("setup-{i}.json"),
                    &serde_json::to_vec(setup).unwrap(),
                ))
            }
        })
        .collect();
    let mut outcomes = [0; 3];
    for (case_index, case) in cases.iter().enumerate() {
        let case = array(case);
        assert_eq!(case.len(), 6);
        let program = index(&case[0]);
        let transition = text(&transitions[index(&case[1])]);
        let params = &inputs[index(&case[2])];
        let expected = &outputs[index(&case[4])];
        let projection = match index(&case[5]) {
            0 => false,
            1 => true,
            _ => panic!("unknown execution outcome"),
        };
        let mut command = command(&paths[program], transition, projection, params);
        if let Some(setup) = &setups[index(&case[3])] {
            command.arg("--setup").arg(setup);
        }
        let result = execute(&mut command, None);
        let label = format!(
            "case {case_index}: {}/{transition} {params}",
            programs[program]["name"]
        );
        assert!(result.stderr.is_empty(), "{label}: {}", stderr(&result));
        let native: Value = serde_json::from_slice(&result.stdout).unwrap_or_else(|error| {
            panic!(
                "{label}: invalid output: {error}: {}",
                String::from_utf8_lossy(&result.stdout)
            )
        });
        let (actual, expected_status) = if projection {
            outcomes[2] += 1;
            (native, 0)
        } else {
            let ok = expected["ok"]
                .as_bool()
                .expect("committing or reverting vector");
            outcomes[usize::from(!ok)] += 1;
            let error = if native["error"].is_null() {
                Value::Null
            } else {
                json!({"code":native["error"]["code"],"kind":text(&native["error"]["kind"]).to_ascii_lowercase()})
            };
            (
                json!({"ok":native["ok"],"error":error,"state":native["state"]}),
                i32::from(!ok),
            )
        };
        assert_eq!(
            result.status.code(),
            Some(expected_status),
            "{label}: process status"
        );
        assert_eq!(&actual, expected, "{label}: execution result");
    }
    assert_eq!(outcomes, [1764, 196, 8]);
}
