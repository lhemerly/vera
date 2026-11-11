//! Complete native CLI workflows, with no executable available on child PATH.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use num_bigint::BigInt;
use serde_json::{json, Value as Json};
use vera_compiler::{fuzz::Xoshiro256, value::Value};

const WORKFLOW: &str = r#"
module Workflows;
table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum];}
effect Changed(k:id64<T>,value:u8);
transition create(value:u8) reads [] writes [T[*].v]
  body {let %k:id64<T>=table_alloc(T,{v:value});commit;}
transition bump(k:id64<T>,delta:u8) reads [T[k].v] writes [T[k].v]
  require live:table_exists(T,k);
  require room:le(delta,sub(255,T[k].v));
  ensure changed:eq(T[k].v,add(old(T[k].v),delta));
  body {let %next:u8=add(table_get(T,k,v),delta);table_set(T,k,v,%next);emit Changed(k,%next);commit;}
transition set(k:id64<T>,value:u8,accept:bool) reads [] writes [T[k].v]
  require live:table_exists(T,k);
  body {table_set(T,k,v,value);emit Changed(k,value);guard(accept,ERR_REFUSED);commit;}
transition noop() reads [] writes [] body {commit;}
projection View(k:id64<T>) {
  allow_if:table_exists(T,k);
  expose:[T[k].v];
  enable_intents:[bump when (lt(T[k].v,255))];
}
"#;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vera-cli-workflows-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.file(name);
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn native<I, S>(args: I, source: &str) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args(args)
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Err(error) = child.stdin.take().unwrap().write_all(source.as_bytes()) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().unwrap()
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn error(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        text(output),
        error(output)
    );
    assert!(output.stderr.is_empty(), "{}", error(output));
}

fn decoded(output: &Output) -> Json {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}\n{}\n{}",
            text(output),
            self::error(output)
        )
    })
}

fn as_os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn with_paths(args: &[&str], paths: &[(&str, &Path)]) -> Vec<OsString> {
    let mut args = as_os(args);
    for (flag, path) in paths {
        args.push(OsString::from(flag));
        args.push(path.as_os_str().to_owned());
    }
    args
}

#[test]
fn check_verify_and_linked_solver_work_with_empty_path() {
    let checked = native(["check", "-", "--json"], WORKFLOW);
    success(&checked);
    let check = decoded(&checked);
    assert_eq!(check["ok"], true);
    assert_eq!(check["tables"][0]["capacity"], 2);
    assert_eq!(check["transitions"].as_array().unwrap().len(), 4);
    let directory = TestDir::new();
    let prefix = directory.file("proof");
    let verified = native(
        with_paths(
            &["verify", "-", "--transition", "bump", "--json"],
            &[("--emit-smt", &prefix)],
        ),
        WORKFLOW,
    );
    success(&verified);
    assert_eq!(decoded(&verified)["ok"], true);
    for kind in ["safety", "panic", "liveness"] {
        let script =
            fs::read_to_string(directory.file(&format!("proof.bump.{kind}.smt2"))).unwrap();
        assert!(script.contains("check-sat"), "{script}");
    }
    let run = native(
        [
            "run",
            "-",
            "--transition",
            "create",
            "--param",
            "value=7",
            "--json",
        ],
        WORKFLOW,
    );
    success(&run);
    assert_eq!(
        decoded(&run)["state"]["tables"]["T"][0],
        json!({"id":2,"v":7})
    );
}

#[test]
fn setup_projection_and_snapshot_restore_keep_exact_state() {
    let directory = TestDir::new();
    let setup = directory.write("setup.json", r#"{"tables":{"T":[{"id":2,"v":7}]}}"#);
    let snapshot = directory.file("state.snapshot");
    let committed = native(
        with_paths(
            &[
                "run",
                "-",
                "--transition",
                "bump",
                "--param",
                "k=2",
                "--param",
                "delta=3",
                "--json",
            ],
            &[("--setup", &setup), ("--snapshot-out", &snapshot)],
        ),
        WORKFLOW,
    );
    success(&committed);
    assert_eq!(decoded(&committed)["state"]["tables"]["T"][0]["v"], 10);
    let projected = native(
        with_paths(
            &[
                "project",
                "-",
                "--projection",
                "View",
                "--param",
                "k=2",
                "--json",
            ],
            &[("--snapshot-in", &snapshot)],
        ),
        WORKFLOW,
    );
    success(&projected);
    let frame = decoded(&projected);
    assert_eq!(frame["allowed"], true);
    assert_eq!(frame["state_slice"]["T.v[2]"]["value"], 10);
    assert_eq!(frame["affordances"][0]["enabled"], true);
    let denied = native(
        with_paths(
            &[
                "project",
                "-",
                "--projection",
                "View",
                "--param",
                "k=3",
                "--json",
            ],
            &[("--snapshot-in", &snapshot)],
        ),
        WORKFLOW,
    );
    success(&denied);
    assert_eq!(decoded(&denied)["allowed"], false);
    assert_eq!(decoded(&denied)["state_slice"], json!({}));
    let restored = native(
        with_paths(
            &[
                "run",
                "-",
                "--transition",
                "bump",
                "--param",
                "k=2",
                "--param",
                "delta=1",
                "--json",
            ],
            &[("--snapshot-in", &snapshot), ("--snapshot-out", &snapshot)],
        ),
        WORKFLOW,
    );
    success(&restored);
    assert_eq!(decoded(&restored)["state"]["tables"]["T"][0]["v"], 11);
}

#[test]
fn durable_restart_replays_baseline_once_then_appends_in_order() {
    let directory = TestDir::new();
    let setup = directory.write("setup.json", r#"{"tables":{"T":[{"id":2,"v":7}]}}"#);
    let wal = directory.file("intents.wal");
    for (delta, expected) in [(3, 10), (5, 15)] {
        let delta = format!("delta={delta}");
        let output = native(
            with_paths(
                &[
                    "run",
                    "-",
                    "--transition",
                    "bump",
                    "--param",
                    "k=2",
                    "--param",
                    &delta,
                    "--json",
                ],
                &[("--setup", &setup), ("--wal", &wal)],
            ),
            WORKFLOW,
        );
        success(&output);
        let state = decoded(&output);
        assert_eq!(state["state"]["tables"]["T"][0]["v"], expected);
        assert!(!state["trace"].as_array().unwrap().is_empty());
    }
    let before = fs::read(&wal).unwrap();
    let replayed = native(
        with_paths(
            &["replay", "-", "--json"],
            &[("--setup", &setup), ("--wal", &wal)],
        ),
        WORKFLOW,
    );
    success(&replayed);
    let state = decoded(&replayed);
    assert_eq!(state["state"]["tables"]["T"][0]["v"], 15);
    assert_eq!(state["state"]["outbox"].as_array().unwrap().len(), 2);
    assert_eq!(fs::read(&wal).unwrap(), before);
    let records = vera_compiler::persistence::Wal::open(wal.to_str().unwrap()).unwrap();
    let records = records.replay().unwrap();
    assert_eq!(records.iter().map(|r| r.tx_seq).collect::<Vec<_>>(), [1, 2]);
}

#[test]
fn cli_replay_recovers_only_torn_tails_and_rejects_foreign_programs() {
    let directory = TestDir::new();
    let wal = directory.file("recovery.wal");
    let output = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified", "--json"],
            &[("--wal", &wal)],
        ),
        WORKFLOW,
    );
    success(&output);
    let valid = fs::read(&wal).unwrap();
    let mut next = {
        let log = vera_compiler::persistence::Wal::open(wal.to_str().unwrap()).unwrap();
        log.replay().unwrap().remove(0)
    };
    next.intent_id = 2;
    next.tx_seq = 2;
    let next = next.encode().unwrap();
    let mut torn = valid.clone();
    torn.extend_from_slice(&next[..next.len() / 2]);
    fs::write(&wal, torn).unwrap();
    let output = native(
        with_paths(&["replay", "-", "--json"], &[("--wal", &wal)]),
        WORKFLOW,
    );
    success(&output);
    assert_eq!(fs::read(&wal).unwrap(), valid);
    let foreign = WORKFLOW.replace("module Workflows;", "module DifferentProgram;");
    let output = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified", "--json"],
            &[("--wal", &wal)],
        ),
        &foreign,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(error(&output).contains("fingerprint"), "{}", error(&output));
    assert_eq!(fs::read(&wal).unwrap(), valid);
    let mut corrupt = valid;
    corrupt[32] ^= 1;
    fs::write(&wal, &corrupt).unwrap();
    let output = native(
        with_paths(
            &["replay", "-", "--unverified", "--json"],
            &[("--wal", &wal)],
        ),
        WORKFLOW,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(&wal).unwrap(), corrupt);
}

#[test]
fn logged_and_direct_success_and_revert_have_identical_traces() {
    let directory = TestDir::new();
    let setup = directory.write("setup.json", r#"{"tables":{"T":[{"id":2,"v":7}]}}"#);
    for accept in [true, false] {
        let argument = format!("accept={accept}");
        let base = [
            "run",
            "-",
            "--transition",
            "set",
            "--param",
            "k=2",
            "--param",
            "value=9",
            "--param",
            &argument,
            "--json",
        ];
        let direct = native(with_paths(&base, &[("--setup", &setup)]), WORKFLOW);
        let wal = directory.file(&format!("{accept}.wal"));
        let logged = native(
            with_paths(&base, &[("--setup", &setup), ("--wal", &wal)]),
            WORKFLOW,
        );
        assert_eq!(direct.status.success(), accept, "{}", error(&direct));
        assert_eq!(logged.status.success(), accept, "{}", error(&logged));
        let direct = decoded(&direct);
        let logged = decoded(&logged);
        assert!(!direct["trace"].as_array().unwrap().is_empty());
        assert_eq!(logged["trace"], direct["trace"]);
        assert_eq!(logged["state"], direct["state"]);
        assert_eq!(logged["ok"], accept);
        if !accept {
            assert_eq!(logged["state"]["tables"]["T"][0]["v"], 7);
            assert_eq!(logged["state"]["outbox"], json!([]));
        }
    }
}

#[test]
fn snapshots_without_log_cursor_cannot_replay_an_existing_wal() {
    let directory = TestDir::new();
    let snapshot = directory.file("state.snapshot");
    let saved = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--json"],
            &[("--snapshot-out", &snapshot)],
        ),
        WORKFLOW,
    );
    success(&saved);
    let wal = directory.file("must-not-be-created.wal");
    let setup = directory.write("setup.json", r#"{"tables":{}}"#);
    for extra in [("--wal", wal.as_path()), ("--setup", setup.as_path())] {
        let output = native(
            with_paths(
                &["run", "-", "--transition", "noop", "--json"],
                &[("--snapshot-in", &snapshot), extra],
            ),
            WORKFLOW,
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            error(&output).contains("no log cursor"),
            "{}",
            error(&output)
        );
    }
    assert!(!wal.exists());
}

#[test]
fn snapshot_output_aliases_never_append_or_replace_wal_bytes() {
    let directory = TestDir::new();
    let fresh = directory.file("must-not-be-created");
    let rejected = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified", "--json"],
            &[("--wal", &fresh), ("--snapshot-out", &fresh)],
        ),
        WORKFLOW,
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(!fresh.exists());
    let wal = directory.file("intents.wal");
    let created = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified", "--json"],
            &[("--wal", &wal)],
        ),
        WORKFLOW,
    );
    success(&created);
    let before = fs::read(&wal).unwrap();
    let hardlink = directory.file("snapshot-hardlink");
    fs::hard_link(&wal, &hardlink).unwrap();
    let mut aliases = vec![wal.clone(), hardlink];
    #[cfg(unix)]
    {
        let link = directory.file("snapshot-symlink");
        std::os::unix::fs::symlink(&wal, &link).unwrap();
        aliases.push(link);
    }
    for alias in aliases {
        let output = native(
            with_paths(
                &["run", "-", "--transition", "noop", "--unverified", "--json"],
                &[("--wal", &wal), ("--snapshot-out", &alias)],
            ),
            WORKFLOW,
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}\n{}",
            text(&output),
            error(&output)
        );
        assert!(output.stdout.is_empty());
        assert_eq!(fs::read(&wal).unwrap(), before);
        assert_eq!(fs::read(&alias).unwrap(), before);
    }
}

#[test]
fn all_emitter_targets_run_with_empty_path_and_checked_proofs() {
    let directory = TestDir::new();
    let source = "transition noop() reads [] writes [] body {commit;}";
    for target in ["rust", "glsl", "wgsl", "cuda"] {
        let path = directory.file(&format!("engine.{target}"));
        let output = native(
            with_paths(&["codegen", target, "-"], &[("--out", &path)]),
            source,
        );
        success(&output);
        assert!(output.stdout.is_empty());
        let emitted = fs::read_to_string(&path).unwrap();
        assert!(!emitted.is_empty());
        assert!(!emitted.contains("import vera"));
        if target == "rust" {
            assert!(emitted.contains("PROGRAM_FINGERPRINT"));
            assert!(emitted.contains("fn main"));
        }
    }
    let no_main = native(
        ["codegen", "rust", "-", "--no-main", "--runtime-crate"],
        source,
    );
    success(&no_main);
    assert!(!text(&no_main).contains("fn main"));
    assert!(text(&no_main).contains("vera_rt"));
}

#[test]
fn proof_gate_rejects_buggy_execution_and_codegen_before_creating_artifacts() {
    let directory = TestDir::new();
    let source = "transition bug(x:u8) reads [] writes [] ensure impossible:false;body {commit;}";
    let wal = directory.file("must-not-exist.wal");
    let output = native(
        with_paths(
            &[
                "run",
                "-",
                "--transition",
                "bug",
                "--param",
                "x=1",
                "--json",
            ],
            &[("--wal", &wal)],
        ),
        source,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        error(&output).contains("not verified"),
        "{}",
        error(&output)
    );
    assert!(!wal.exists());
    let artifact = directory.file("must-not-exist.rs");
    let output = native(
        with_paths(&["codegen", "rust", "-"], &[("--out", &artifact)]),
        source,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!artifact.exists());
    let development = native(
        [
            "run",
            "-",
            "--transition",
            "bug",
            "--param",
            "x=1",
            "--unverified",
            "--json",
        ],
        source,
    );
    success(&development);
    assert_eq!(decoded(&development)["ok"], true);
}

#[test]
fn typed_cli_parameters_preserve_decimals_symbols_enums_and_bytes() {
    let source = "effect Seen(flag:bool,tag:enum{A,B},symbol:sym32,wide:dec128,blob:bytes32);transition inspect(flag:bool,tag:enum{A,B},symbol:sym32,wide:dec128,blob:bytes32) reads [] writes [] body {emit Seen(flag,tag,symbol,wide,blob);commit;}";
    let bytes = format!(
        "blob=0x{}",
        (0u8..32).map(|b| format!("{b:02x}")).collect::<String>()
    );
    let output = native(
        [
            "run",
            "-",
            "--transition",
            "inspect",
            "--param",
            "flag=yes",
            "--param",
            "tag=B",
            "--param",
            "symbol=Alice",
            "--param",
            "wide=-1.000000000000000001",
            "--param",
            &bytes,
            "--json",
        ],
        source,
    );
    success(&output);
    let output = decoded(&output);
    let args = &output["state"]["outbox"][0]["args"];
    assert_eq!(args["flag"], true);
    assert_eq!(args["tag"], 1);
    assert_eq!(args["symbol"], vera_compiler::value::symbol_id("Alice"));
    assert_eq!(args["wide"].to_string(), "-1000000000000000001");
    assert_eq!(args["blob"], bytes.strip_prefix("blob=").unwrap());
}

#[test]
fn invalid_source_parameters_and_setups_fail_before_wal_mutation() {
    let directory = TestDir::new();
    let wal = directory.file("must-not-exist.wal");
    for parameters in [
        vec![],
        vec!["--param", "value=-1"],
        vec!["--param", "value=256"],
        vec!["--param", "value=1", "--param", "value=2"],
        vec!["--param", "unknown=1"],
        vec!["--param", "missing-equals"],
    ] {
        let mut args = as_os(&[
            "run",
            "-",
            "--transition",
            "create",
            "--unverified",
            "--json",
        ]);
        args.extend(as_os(&parameters));
        args.extend([OsString::from("--wal"), wal.as_os_str().to_owned()]);
        let output = native(args, WORKFLOW);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}\n{}",
            text(&output),
            error(&output)
        );
        assert!(output.stdout.is_empty());
        assert!(!wal.exists());
    }
    for bad in [
        r#"{"tables":{},"unknown":1}"#,
        r#"{"tables":{"Missing":[]}}"#,
        r#"{"tables":{"T":{}}}"#,
        r#"{"tables":{"T":[{"id":-1,"v":0}]}}"#,
        r#"{"tables":{"T":[{"id":2,"v":256}]}}"#,
        r#"{"tables":{"T":[{"id":2,"unknown":1}]}}"#,
        r#"{"tables":{"T":[{"id":2,"v":1},{"id":4,"v":2}]}}"#,
        r#"{"tables":{"T":[{"id":1099511627776,"v":1}]}}"#,
    ] {
        let setup = directory.write("invalid.json", bad);
        let output = native(
            with_paths(
                &["run", "-", "--transition", "noop", "--unverified", "--json"],
                &[("--setup", &setup), ("--wal", &wal)],
            ),
            WORKFLOW,
        );
        assert!(!output.status.success(), "accepted setup {bad}");
        assert!(output.stdout.is_empty());
        assert!(!wal.exists());
    }
    let output = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified"],
            &[("--wal", &wal)],
        ),
        "transition broken(",
    );
    assert!(!output.status.success());
    assert!(!wal.exists());
}

#[test]
fn setup_and_restore_admit_valid_nonempty_state_when_empty_state_is_invalid() {
    let directory = TestDir::new();
    let source = "table T capacity 1 {key id:id64<T>;col v:u64 [tracked_sum];}invariant populated {eq(sum(T.v),1);}transition noop() reads [] writes [] body {commit;}";
    let setup = directory.write("populated.json", r#"{"tables":{"T":[{"id":1,"v":1}]}}"#);
    let snapshot = directory.file("populated.snapshot");
    let output = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified", "--json"],
            &[("--setup", &setup), ("--snapshot-out", &snapshot)],
        ),
        source,
    );
    success(&output);
    let output = native(
        with_paths(
            &["run", "-", "--transition", "noop", "--unverified", "--json"],
            &[("--snapshot-in", &snapshot)],
        ),
        source,
    );
    success(&output);
    assert_eq!(decoded(&output)["state"]["tables"]["T"][0]["v"], 1);
}

#[test]
fn id_named_column_keeps_handle_and_roundtrips_nested_setup_and_snapshot() {
    let directory = TestDir::new();
    let source = "table T capacity 2 {key row:id64<T>;col id:u8;col columns:u8;}transition create() reads [] writes [T[*].id,T[*].columns] body {let %k:id64<T>=table_alloc(T,{id:9,columns:11});commit;}transition noop() reads [] writes [] body {commit;}";
    let snapshot = directory.file("collision.snapshot");
    let created = native(
        with_paths(
            &["run", "-", "--transition", "create", "--json"],
            &[("--snapshot-out", &snapshot)],
        ),
        source,
    );
    success(&created);
    let state = decoded(&created)["state"].clone();
    assert_eq!(
        state["tables"]["T"][0],
        json!({"id":2,"columns":{"id":9,"columns":11}})
    );
    let setup = directory.write(
        "collision.json",
        &json!({"tables":state["tables"]}).to_string(),
    );
    for initial in [
        ("--setup", setup.as_path()),
        ("--snapshot-in", snapshot.as_path()),
    ] {
        let output = native(
            with_paths(&["run", "-", "--transition", "noop", "--json"], &[initial]),
            source,
        );
        success(&output);
        assert_eq!(decoded(&output)["state"], state);
    }
}

#[test]
fn end_of_options_preserves_help_named_source_file() {
    let directory = TestDir::new();
    directory.write("--help", "module Sentinel;");
    let output = Command::new(env!("CARGO_BIN_EXE_vera"))
        .args(["parse", "--json", "--", "--help"])
        .current_dir(&directory.0)
        .env("PATH", "")
        .output()
        .unwrap();
    success(&output);
    assert_eq!(decoded(&output)["name"], "Sentinel");
}

#[test]
fn deterministic_fuzzing_covers_signed_values_and_minimizes_to_zero() {
    let signed =
        "transition go(x:i64) reads [] writes [] ensure nonnegative:ge(x,0);body {commit;}";
    let output = native(
        [
            "fuzz",
            "-",
            "--steps",
            "64",
            "--threads",
            "1",
            "--seed",
            "1",
            "--json",
        ],
        signed,
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}\n{}",
        text(&output),
        error(&output)
    );
    let violation = decoded(&output);
    assert_eq!(violation["ok"], false);
    assert_eq!(violation["violations"][0]["law"], "ensure nonnegative");
    let parameter: Value =
        serde_json::from_value(violation["violations"][0]["trace"][0]["params"]["x"].clone())
            .unwrap();
    assert_eq!(parameter, Value::Int(BigInt::from(-1)));
    let nonmonotonic = "transition wait() reads [] writes [] body {commit;}transition go(x:u8) reads [] writes [] ensure narrow:and(ne(x,0),lt(x,100));body {commit;}";
    let args = [
        "fuzz",
        "-",
        "--steps",
        "64",
        "--threads",
        "1",
        "--seed",
        "1",
        "--json",
    ];
    let first = native(args, nonmonotonic);
    let second = native(args, nonmonotonic);
    assert_eq!(first.status.code(), Some(1));
    assert_eq!(first.stdout, second.stdout);
    let result = decoded(&first);
    let trace = result["violations"][0]["trace"].as_array().unwrap();
    assert_eq!(trace.len(), 1);
    let parameter: Value = serde_json::from_value(trace[0]["params"]["x"].clone()).unwrap();
    assert_eq!(parameter, Value::Int(BigInt::from(0)));
    let clean = native(
        [
            "fuzz",
            "-",
            "--steps",
            "4",
            "--threads",
            "2",
            "--capacity",
            "0",
            "--json",
        ],
        "transition noop() reads [] writes [] body {commit;}",
    );
    success(&clean);
    assert_eq!(decoded(&clean)["executed"], 8);
    assert_eq!(decoded(&clean)["committed"], 8);
}

#[test]
fn xoshiro_stream_matches_independent_reference_vector() {
    let mut rng = Xoshiro256::new(1);
    assert_eq!(
        (0..5).map(|_| rng.next_u64()).collect::<Vec<_>>(),
        [
            12966619160104079557,
            9600361134598540522,
            10590380919521690900,
            7218738570589545383,
            12860671823995680371
        ]
    );
}
