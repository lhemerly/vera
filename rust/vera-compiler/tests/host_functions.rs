//! Public, stable host calls must work from a separate Rust module, without
//! patching generated source or knowing the compiler's private function IDs.
use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use vera_compiler::{
    codegen_rust::{generate_rust, rust_api_manifest, RustOptions},
    parse_source,
    semantic::analyze,
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
fn program(source: &str) -> vera_compiler::ir::CheckedProgram {
    analyze(parse_source(source).unwrap()).unwrap_or_else(|errors| panic!("{errors:?}"))
}
fn compile_host(source: &str, body: &str, deny_unused: bool) -> std::process::Output {
    let dir = std::env::temp_dir().join(format!(
        "vera-host-functions-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("generated.rs"),
        generate_rust(
            &program(source),
            RustOptions {
                use_rt: false,
                include_main: false,
            },
        )
        .unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("host.rs"),
        format!("mod generated; fn main(){{{body}}}"),
    )
    .unwrap();
    let mut command = Command::new("rustc");
    command
        .current_dir(&dir)
        .args(["--edition=2021", "host.rs", "-o", "host"]);
    if deny_unused {
        command.arg("-Dunused-must-use");
    }
    let result = command.output().unwrap();
    if result.status.success() {
        let run = Command::new(dir.join("host")).output().unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
    fs::remove_dir_all(&dir).unwrap();
    result
}
fn passes(source: &str, body: &str) {
    let result = compile_host(source, body, true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
#[test]
fn pure_host_api_is_public_named_and_handles_preconditions() {
    passes("function increment(x:u64)->u64 require room:lt(x,18446744073709551615); ensure exact:eq(result,add(x,1)); body {yield add(x,1);}",
      "assert_eq!(generated::functions::call_increment(41),Ok(42));assert_eq!(generated::functions::call_increment(u64::MAX),Err(generated::ErrorCode::FunctionPrecondition));");
}
#[test]
fn names_are_stable_across_reordering_and_rust_keywords() {
    let a="function type(x:u8)->u8 body{yield x;}function self(x:u8)->u8 body{yield x;}function vera_self(x:u8)->u8 body{yield x;}function functions(x:u8)->u8 body{yield x;} function VERA_HOST_API_JSON()->u8 body{yield 5;}function call_type()->u8 body{yield 6;}transition functions() reads [] writes [] body{commit;}transition VERA_HOST_API_JSON() reads [] writes [] body{commit;}transition call_type() reads [] writes [] body{commit;}";
    passes(a,"assert_eq!(generated::functions::call_type(1),Ok(1));assert_eq!(generated::functions::call_self(2),Ok(2));assert_eq!(generated::functions::call_vera_self(3),Ok(3));assert_eq!(generated::functions::call_functions(4),Ok(4));");
    let a = rust_api_manifest(&program(
        "function a()->u8 body{yield 1;}function b()->u8 body{yield 2;}",
    ));
    let b = rust_api_manifest(&program(
        "function b()->u8 body{yield 2;}function a()->u8 body{yield 1;}",
    ));
    for name in ["a", "b"] {
        let find = |v: &serde_json::Value| {
            v["functions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == name)
                .unwrap()["rust_path"]
                .clone()
        };
        assert_eq!(find(&a), find(&b));
    }
}
#[test]
fn invalid_host_bytes_are_rejected_before_body_access() {
    passes("function first(x:bytes<2>)->u8 require nonempty:gt(bytes_len(x),0);body{yield bytes_get(x,0);}",
      "use generated::{functions::call_first,VeraBytes,ErrorCode}; assert_eq!(call_first(VeraBytes{len:3,data:[1,2]}),Err(ErrorCode::InvalidEncoding));assert_eq!(call_first(VeraBytes{len:1,data:[1,2]}),Err(ErrorCode::InvalidEncoding));assert_eq!(call_first(VeraBytes::from_slice(&[42]).unwrap()),Ok(42));");
}
#[test]
fn nominal_values_are_public_and_recursively_validated() {
    passes("record Packet{data:bytes<2>;} union Choice{Packet:Packet;Number:u64;} function same(x:Choice)->Choice body{yield x;}",
      "use generated::{functions::call_same,VeraValue_Packet,VeraValue_Choice,VeraBytes,ErrorCode};let good=VeraValue_Choice::Number(42);assert_eq!(call_same(good),Ok(good));let bad=VeraValue_Choice::Packet(VeraValue_Packet{data:VeraBytes{len:9,data:[0,0]}});assert_eq!(call_same(bad),Err(ErrorCode::InvalidEncoding));");
}
#[test]
fn unused_result_is_reported_but_explicit_discard_is_not_claimed_prevented() {
    let source = "function value()->u8 body{yield 42;}";
    let rejected = compile_host(source, "generated::functions::call_value();", true);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unused"));
    passes(source, "let _ = generated::functions::call_value();");
}
#[test]
fn metadata_describes_exact_public_signatures_without_a_proof_claim() {
    let api = rust_api_manifest(&program(
        "function amount(x:dec64,y:bytes<4>)->dec64 body{yield x;}",
    ));
    assert_eq!(api["schema_version"], 1);
    assert_eq!(api["verification"], "not_asserted_by_interface_metadata");
    assert_eq!(api["functions"][0]["rust_path"], "functions::call_amount");
    assert_eq!(api["functions"][0]["parameters"][0]["name"], "x");
    assert_eq!(api["functions"][0]["parameters"][0]["rust_type"], "i64");
    assert_eq!(
        api["functions"][0]["parameters"][1]["rust_type"],
        "VeraBytes<4>"
    );
}

#[test]
fn enum_host_tags_and_metadata_are_explicit() {
    let source = "function same(x:enum {Ready,Done})->enum {Ready,Done} body{yield x;}";
    passes(source,"assert_eq!(generated::functions::call_same(1),Ok(1));assert_eq!(generated::functions::call_same(2),Err(generated::ErrorCode::InvalidEncoding));");
    let api = rust_api_manifest(&program(source));
    assert_eq!(
        api["functions"][0]["parameters"][0]["representation"]["tags"][1],
        serde_json::json!({"name":"Done","value":1})
    );
}
#[test]
fn metadata_includes_nominal_layouts_scales_and_program_binding() {
    let program=program("record Packet{cost:dec64;}union Choice{Packet:Packet;Number:u64;}function same(x:Choice)->Choice body{yield x;}");
    let api = rust_api_manifest(&program);
    assert_eq!(
        api["named_types"][0]["fields"][0]["representation"]["scale_factor"],
        100000000u64
    );
    assert_eq!(api["named_types"][1]["variants"][0]["tag"], 0);
    assert_eq!(api["named_types"][1]["variants"][0]["rust_name"], "Packet");
    let fingerprint = vera_compiler::persistence::program_fingerprint(&program)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(api["program_fingerprint"], fingerprint);
}
#[test]
fn cli_sidecar_truthfully_reports_proof_and_protects_outputs() {
    let dir = std::env::temp_dir().join(format!("vera-host-cli-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("input.vera");
    let output = dir.join("generated.rs");
    let api = dir.join("api.json");
    let good = "function same(x:u8)->u8 ensure exact:eq(result,x);body{yield x;}";
    fs::write(&source, good).unwrap();
    let run = |out: &std::path::Path, api: &std::path::Path, unverified: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vera"));
        cmd.args(["codegen", "rust"])
            .arg(&source)
            .arg("--no-main")
            .arg("--out")
            .arg(out)
            .arg("--api-out")
            .arg(api);
        if unverified {
            cmd.arg("--unverified");
        }
        cmd.output().unwrap()
    };
    assert!(run(&output, &api, false).status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(&api).unwrap()).unwrap();
    assert_eq!(metadata["proof_admitted"], true);
    assert_eq!(
        metadata["proof_scope"],
        "checked_program_not_host_integration"
    );
    assert!(!run(&output, &source, false).status.success());
    assert_eq!(fs::read_to_string(&source).unwrap(), good);
    assert!(!run(&output, &output, false).status.success());
    #[cfg(unix)]
    {
        let alias = dir.join("alias.vera");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        assert!(!run(&output, &alias, false).status.success());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&source, &alias).unwrap();
        assert!(!run(&output, &alias, false).status.success());
    }
    fs::write(
        &source,
        "function wrong()->u8 ensure bad:eq(result,1);body{yield 0;}",
    )
    .unwrap();
    fs::remove_file(&api).unwrap();
    assert!(!run(&output, &api, false).status.success());
    assert!(!api.exists());
    assert!(run(&output, &api, true).status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(&api).unwrap()).unwrap();
    assert_eq!(metadata["proof_admitted"], false);
    fs::remove_file(&api).unwrap();
    assert!(!run(&dir.join("missing/out.rs"), &api, true)
        .status
        .success());
    assert!(!api.exists());
    fs::remove_dir_all(dir).unwrap();
}
