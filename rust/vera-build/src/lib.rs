//! Proof-gated generation for a conventional Rust application's build script.
//!
//! ```no_run
//! let bundle = vera_build::Build::new("policy.vera")
//!     .module_name("policy")
//!     .compile()
//!     .expect("VERA policy must verify");
//! # let _ = bundle;
//! ```
//!
//! Include the generated module with
//! `include!(concat!(env!("OUT_DIR"), "/policy.module.rs"))`; the binder declares
//! a `policy` module and preserves generated module-level attributes.
//! The trusted VERA compiler proves the core before generation. Rust compilation,
//! tests and result-use tests remain separate obligations for the host. The
//! receipt is provenance, not a signed attestation or proof of arbitrary Rust.

use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Files emitted only after successful proof-gated compiler execution.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub rust_path: PathBuf,
    /// Include this binder, not the generated Rust directly.
    pub module_path: PathBuf,
    pub api_path: PathBuf,
    pub receipt_path: PathBuf,
}

/// Build configuration. There is intentionally no unverified mode or arbitrary
/// extra compiler-argument escape hatch.
#[derive(Debug, Clone)]
pub struct Build {
    source: PathBuf,
    module_name: String,
    compiler: Option<PathBuf>,
    out_dir: Option<PathBuf>,
}

impl Build {
    pub fn new(source: impl AsRef<Path>) -> Self {
        Self {
            source: source.as_ref().to_owned(),
            module_name: "verified".into(),
            compiler: None,
            out_dir: None,
        }
    }
    pub fn module_name(mut self, name: impl Into<String>) -> Self {
        self.module_name = name.into();
        self
    }
    /// Select a trusted VERA executable. Defaults to VERA, then `vera` on PATH.
    pub fn compiler(mut self, path: impl AsRef<Path>) -> Self {
        self.compiler = Some(path.as_ref().to_owned());
        self
    }
    /// Override Cargo's OUT_DIR, for tools that use the same build workflow.
    pub fn out_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.out_dir = Some(path.as_ref().to_owned());
        self
    }
    pub fn compile(self) -> io::Result<Bundle> {
        if self.module_name.is_empty()
            || !self
                .module_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || self.module_name.as_bytes()[0].is_ascii_digit()
            || is_keyword(&self.module_name)
        {
            return Err(invalid(
                "module name must be an ASCII Rust-style identifier",
            ));
        }
        let out = self
            .out_dir
            .or_else(|| env::var_os("OUT_DIR").map(PathBuf::from))
            .ok_or_else(|| invalid("OUT_DIR is unavailable; use Build::out_dir outside Cargo"))?;
        fs::create_dir_all(&out)?;
        let out = out.canonicalize()?;
        let bundle = Bundle {
            rust_path: out.join(format!("{}.rs", self.module_name)),
            module_path: out.join(format!("{}.module.rs", self.module_name)),
            api_path: out.join(format!("{}.api.json", self.module_name)),
            receipt_path: out.join(format!("{}.receipt.json", self.module_name)),
        };
        // Resolve both protected inputs before any cleanup, even if the other
        // input is missing. A mistaken output alias must never delete either.
        let source_result = self.source.canonicalize();
        let compiler_name = self
            .compiler
            .map(PathBuf::into_os_string)
            .or_else(|| env::var_os("VERA"))
            .unwrap_or_else(|| OsString::from("vera"));
        let compiler_result = (|| {
            let spelling = resolve_executable(Path::new(&compiler_name))?;
            let canonical = spelling.canonicalize()?;
            Ok::<_, io::Error>((spelling, canonical))
        })();
        for path in paths(&bundle) {
            let existing = path.canonicalize().ok();
            if let Ok(source) = &source_result {
                if path == source || existing.as_ref() == Some(source) {
                    return Err(invalid("source aliases a generated output"));
                }
            }
            if let Ok((_, compiler)) = &compiler_result {
                if path == compiler || existing.as_ref() == Some(compiler) {
                    return Err(invalid("compiler aliases a generated output"));
                }
            }
        }
        clear(&bundle)?;
        let source = source_result?;
        let (compiler_spelling, compiler) = compiler_result?;
        let result = (|| {
            // Cargo values are line based. Refuse path injection into its protocol.
            for path in [&self.source, &source, &compiler_spelling, &compiler] {
                if path.to_string_lossy().contains(['\n', '\r']) {
                    return Err(invalid("source/compiler paths cannot contain line breaks"));
                }
                println!("cargo:rerun-if-changed={}", path.display());
            }
            println!("cargo:rerun-if-env-changed=VERA");
            println!("cargo:rerun-if-env-changed=PATH");
            let source_bytes = fs::read(&source)?;
            let compiler_bytes = fs::read(&compiler)?;
            let temp = Temp::new(&out)?;
            let snapshot = temp.0.join("core.vera");
            let rust = temp.0.join("core.rs");
            let api = temp.0.join("api.json");
            fs::write(&snapshot, &source_bytes)?;
            let output = Command::new(&compiler)
                .args(["codegen", "rust"])
                .arg(&snapshot)
                .arg("--no-main")
                .arg("--out")
                .arg(&rust)
                .arg("--api-out")
                .arg(&api)
                .output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "VERA proof-gated code generation failed ({}):\n{}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            if self.source.canonicalize()? != source
                || compiler_spelling.canonicalize()? != compiler
                || fs::read(&source)? != source_bytes
                || fs::read(&compiler)? != compiler_bytes
            {
                return Err(invalid(
                    "source or compiler changed during generation; rebuild with stable inputs",
                ));
            }
            let rust_bytes = fs::read(&rust)?;
            let api_bytes = fs::read(&api)?;
            if rust_bytes.is_empty() {
                return Err(invalid("compiler produced empty Rust"));
            }
            let metadata: serde_json::Value =
                serde_json::from_slice(&api_bytes).map_err(io::Error::other)?;
            if metadata["proof_admitted"] != true
                || metadata["proof_scope"] != "checked_program_not_host_integration"
            {
                return Err(invalid(
                    "compiler did not admit a proof for the checked program",
                ));
            }
            if metadata["api"]["schema_version"] != 1
                || metadata["api"]["target"] != "rust"
                || !metadata["api"]["functions"].is_array()
            {
                return Err(invalid("unsupported or malformed VERA Rust API manifest"));
            }
            let binder = format!(
                "#[path = {:?}] pub mod {};\n",
                bundle
                    .rust_path
                    .to_str()
                    .ok_or_else(|| invalid("OUT_DIR must be UTF-8 for Rust module generation"))?,
                self.module_name
            );
            let receipt = serde_json::json!({
                "schema_version": 1,
                "kind": "vera_rust_build_provenance",
                "source_sha256": digest(&source_bytes),
                "compiler_sha256": digest(&compiler_bytes),
                "generated_rust_sha256": digest(&rust_bytes),
                "module_binder_sha256": digest(binder.as_bytes()),
                "api_manifest_sha256": digest(&api_bytes),
                "program_fingerprint": metadata["api"]["program_fingerprint"],
                "compiler_arguments": ["codegen", "rust", "<source-snapshot>", "--no-main", "--out", "<generated-rust>", "--api-out", "<api-manifest>"],
                "core_proof": "trusted_compiler_proof_gated_codegen_succeeded",
                "host_verification": "not_performed",
                "host_result_use": "not_checked",
                "limitations": ["Provenance is not a signed attestation.", "Trust includes the compiler and its runtime dependencies, the Rust toolchain and build host.", "The receipt binds generated files, not the final host executable.", "Host tests must establish the verified result controls required observable behavior."]
            });
            fs::write(&bundle.rust_path, rust_bytes)?;
            fs::write(&bundle.api_path, api_bytes)?;
            fs::write(&bundle.module_path, binder)?;
            // Commit marker written last; failed writes invalidate every output.
            fs::write(
                &bundle.receipt_path,
                serde_json::to_vec_pretty(&receipt).map_err(io::Error::other)?,
            )?;
            Ok(bundle.clone())
        })();
        if result.is_err() {
            clear(&bundle)?;
        }
        result
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn paths(bundle: &Bundle) -> [&Path; 4] {
    [
        &bundle.rust_path,
        &bundle.module_path,
        &bundle.api_path,
        &bundle.receipt_path,
    ]
}
fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "_" | "as"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "try"
            | "gen"
    )
}
fn clear(bundle: &Bundle) -> io::Result<()> {
    for path in paths(bundle) {
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn resolve_executable(path: &Path) -> io::Result<PathBuf> {
    if path.components().count() > 1 || path.is_absolute() {
        return Ok(path.to_owned());
    }
    for directory in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        let candidate = directory.join(path);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("VERA executable not found: {}", path.display()),
    ))
}
struct Temp(PathBuf);
impl Temp {
    fn new(out: &Path) -> io::Result<Self> {
        loop {
            let path = out.join(format!(
                ".vera-build-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
