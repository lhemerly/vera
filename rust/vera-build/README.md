# vera-build

A small Cargo build helper for **verified VERA cores and ordinary Rust hosts**.
It invokes a trusted installed `vera` compiler; it does not link Z3 into the
application or reimplement verification.

```rust,ignore
// build.rs
fn main() {
    vera_build::Build::new("policy.vera")
        .module_name("verified")
        .compile()
        .expect("the policy must verify before Rust builds");
}
```

```rust,ignore
// src/main.rs (or src/lib.rs)
include!(concat!(env!("OUT_DIR"), "/verified.module.rs"));
// Consume verified::functions::call_FUNCTION(...) in real host behavior.
```

Set `VERA=/path/to/vera`, or place `vera` on PATH. An explicit
`.compiler(path)` overrides environment selection. Generation uses
`codegen rust --no-main --out ... --api-out ...`, without `--unverified`.
A false or unresolved proof blocks the build. Runtime precondition/domain
errors remain `Result` values that the Rust host must handle.

The helper emits `verified.rs`, an include-compatible module binder,
`verified.api.json`, and `verified.receipt.json` into Cargo's OUT_DIR. The
compiler-generated API manifest describes stable typed function names. The
receipt binds the exact source bytes, selected compiler executable, generated
Rust, binder and manifest using SHA-256. Source and compiler changes during
invocation are rejected. A source snapshot is used for the compiler invocation.
Prior output files are cleared before generation, and any failure clears partial
outputs. Accidental output aliases of source/compiler are rejected without
removing those inputs. Cargo is instructed to rerun for source/compiler paths
and VERA/PATH changes.

## Evidence, not a whole-application claim

The receipt records **trusted compiler proof-gated generation succeeded**.
It is neither a signed proof certificate nor evidence that the host consumes
the result. Trust includes the compiler and its runtime dependencies, Rust's
compiler, Cargo/build scripts and build host. Do not treat this as tamper-proof
isolation from malicious host code or a hostile filesystem. The helper does not
validate arbitrary changes made to OUT_DIR after generation; keep generated
files private to the build and rebuild after changing the core.

Rust type checks, lint checks, host tests and end-to-end result-use tests remain
separate. In particular, an application can compile while ignoring a correct
answer. Test observable behavior and include mutations that replace/ignore the
core's result. This crate creates no verification timeouts or policy limits.

Use a pinned explicit compiler path for reproducible builds. Cargo does not watch
its runtime dependencies; cleanly rebuild when that environment changes. A bundle
is for one build writer: do not concurrently generate the same module into a
shared output directory.
