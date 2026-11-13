# Verified core, conventional Rust shell

A VERA application does not need to express I/O, networking, files, UI or every
algorithm in the verifier. Put decisions with useful, explicit contracts in a
VERA core and use ordinary Rust for the shell. Rust's compiler, lints, tests and
other conventional tools check that shell. They do **not** turn it into a
formally verified program.

`vera-build` makes this split an ordinary Cargo build: a failed or unknown proof
fails the build, and the generated typed core is available as a Rust module.
No Z3 or VERA executable is required by the resulting application at runtime.
The build helper itself does not link the compiler or solver into the host.

## Minimal integration

Add the build dependency (until a registry release exists, use the repository
path or pin a published Git revision):

```toml
[build-dependencies]
vera-build = { path = "/path/to/vera/rust/vera-build" }
```

Write `build.rs`:

```rust
fn main() {
    vera_build::Build::new("policy.vera")
        .module_name("policy")
        .compile()
        .expect("policy must verify");
}
```

Use the generated module in Rust:

```rust
include!(concat!(env!("OUT_DIR"), "/policy.module.rs"));

// Use the returned decision, not just is_ok() or a ceremonial call.
let fee = policy::functions::call_shipping_fee(weight, express, subtotal)?;
println!("{fee}");
```

Run `VERA=/path/to/vera cargo build`. The executable defaults to `$VERA`, then
`vera` on PATH. It must be a trusted compiler installation. Cargo watches the
policy file, compiler executable and compiler selection variables; changing the
core runs proof-gated generation again. There is no unverified switch in the
helper. When generation fails, generated outputs for that bundle are removed.
Never ship a previous executable merely because one remains on disk after a
failed Cargo command.

## What the files mean

`OUT_DIR` contains the generated Rust, its module binder, the compiler's host API
manifest and a provenance receipt. The receipt records hashes of the source,
compiler executable and generated files, and the exact generation mode. The
source is snapshotted for generation; changes detected during that operation
fail rather than mixing identities. Runtime dependencies of the compiler and
the Rust toolchain remain in the trust boundary. Cargo does not watch compiler
runtime dependencies; pin an explicit compiler path and rebuild cleanly when
its dependency environment changes. Each build should own its output directory;
do not generate the same module concurrently into a shared directory.

A receipt is **not a signature, security certificate or proof of the host**.
The helper trusts the selected compiler and local build host. An unrestricted
host author can bypass `build.rs`, edit generated files or ignore values. These
are not prevented by a JSON receipt. Treat artifacts as immutable in your CI,
regenerate from source, and run host tests against the resulting executable.

This build helper does not replace the specification-author/implementer separation
of the Invariant God workflow. It proves the contracts present in the supplied
source, not their independence or completeness. If your harness freezes contracts,
keep its admission gate and contract revision checks; do not use this helper to
silently accept an implementer-rewritten specification.

## Keep the evidence separate

| Layer | Evidence | Does not establish |
|---|---|---|
| Core | VERA proof for the stated contracts | Requirements are complete or correct |
| Boundary | Typed API, input-domain checks, precondition rejection | Inputs were mapped from the right source fields |
| Build | Rust compilation, source/compiler/artifact provenance | Correct observable host behavior |
| Host | Lints, independent tests, malformed-input and mutation tests | Universal correctness of arbitrary Rust or I/O |

For a result-use test, choose inputs with distinct correct outputs. Substitute
a constant for the verified result **while retaining the call**, and check the
test fails. Also deliberately swap/mis-map an input. Both mutants should compile;
a compilation error is not evidence that a behavioral test detects the mistake.
Tests are finite evidence, not a universal adoption proof.

## Runnable application

[Parcel Quote](../examples/parcel-quote) is a CLI for individual and batch parcel
quotes. Conventional Rust reads files/stdin and validates the request syntax;
the proved policy computes the fee that reaches JSON output. It demonstrates a
complete useful workflow with an illustrative business policy, not a certified
carrier-rate service.

The repository regression builds the app from source through Cargo, runs its
executable tests, checks two compiling consumer mutants, changes the policy to
an unprovable one and ensures Cargo rejects it, then repairs and rebuilds it.

Use this pattern for authorization decisions, limits, eligibility rules,
accounting calculations or state transitions that actually fit VERA's model.
Do not force an arbitrary task into a token verified function. Keep peripheral
logic conventional when a useful contract cannot be expressed, and state that
boundary honestly.
