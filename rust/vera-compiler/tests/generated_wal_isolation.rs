//! Embedded runtime tests must not share process-ID-only temporary paths.
use std::{fs, path::PathBuf, process::Command};
use vera_compiler::{
    codegen_rust::{generate_rust, RustOptions},
    parse_source,
    semantic::analyze,
};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn embedded_cores_run_wal_tests_in_parallel_without_shared_files() {
    let mut sequence = 0u64;
    let scratch = loop {
        let path = std::env::temp_dir().join(format!(
            "vera-embedded-wal-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => break Scratch(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => sequence += 1,
            Err(error) => panic!("cannot reserve generated WAL test directory: {error}"),
        }
    };
    let program =
        analyze(parse_source("function identity(x:u64)->u64 body{yield x;}").unwrap()).unwrap();
    let generated = generate_rust(
        &program,
        RustOptions {
            use_rt: false,
            include_main: false,
        },
    )
    .unwrap();
    fs::write(scratch.0.join("generated.rs"), generated).unwrap();
    let modules = (0..16)
        .map(|i| format!("#[path=\"generated.rs\"] mod core_{i};\n"))
        .collect::<String>();
    fs::write(scratch.0.join("host.rs"), modules).unwrap();
    let binary = scratch.0.join("tests");
    let built = Command::new("rustc")
        .args(["--edition=2021", "--test"])
        .arg(scratch.0.join("host.rs"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    for _ in 0..5 {
        let tested = Command::new(&binary)
            .arg("--test-threads=64")
            .output()
            .unwrap();
        assert!(
            tested.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&tested.stdout),
            String::from_utf8_lossy(&tested.stderr)
        );
        assert!(String::from_utf8_lossy(&tested.stdout).contains("64 passed; 0 failed"));
    }
}
