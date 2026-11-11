# Native Rust toolchain migration

## Problem and current ownership

VERA's Python frontend parses syntax, resolves declarations and contextual types,
and supplies the interpreter, SMT verifier and code generators. The resolved
types and exact literal values currently live in maps keyed by Python object
identity. Some consumers reconstruct type context. The existing `vera-rt` crate
provides execution and persistence infrastructure with a generated concrete
`spec.rs`; it is not a general-purpose compiler.

The Rust port will make the compiler and CLI native while preserving the v1.1
language contract, transaction ordering, checked arithmetic, proof obligations,
and durable data compatibility. Python remains an independent development
oracle during migration. The native executable never launches Python.

## Usage of the first increment

```bash
cargo run --bin vera -- parse examples/bank.vera
cargo run --bin vera -- parse examples/counter.vera --json
cargo test --workspace --locked
cargo build -p vera-compiler --release --locked
python tools/check_rust_frontend.py --binary target/release/vera
```

```rust
use vera_compiler::parse_source;

let module = parse_source("module Example;")?;
let syntax = module.canonical_json();
# Ok::<(), vera_compiler::Diagnostic>(())
```

`parse` checks syntax only. It does not establish type correctness, valid
footprints, invariant preservation, or executable behavior. Native `check`,
`verify`, `run` and `codegen` are rejected explicitly until their implementations
are complete. The established Python commands remain available separately.

The canonical AST JSON is a development comparison format. Integer values are
decimal strings inside `{"integer":"..."}`; decimals preserve sign, coefficient
digits and scale inside `{"decimal":{...}}`. This avoids JSON number rounding and
does not replace the existing WAL/snapshot program fingerprint format.

## Shape and synthesis decision

Use one `vera-compiler` crate with a thin `vera` binary and an existing `vera-rt`
member in a root Cargo workspace. Parsing owns a recursive syntax AST, exact
literal representations and UTF-8 byte spans with source line/character column.
`parse_source` hides tokenization and grammar mechanics behind one fallible
interface. Canonical serialization excludes source locations.

Two designs were considered: a recursively typed tree and an arena of resolved
typed operations. The syntax tree remains recursive because that matches the
grammar and gives a small parsing API. Analysis will lower it once to an
immutable arena-backed checked program. Stable declaration, column, expression
and binding IDs prevent spelling collisions and object-identity side tables.
The analyzer will be the sole constructor of that checked program; execution,
proof and code generation will accept only its resolved representation.

The planned checked representation has these boundaries:

```rust,ignore
fn analyze(syntax: ast::Module) -> Result<CheckedProgram, Vec<Diagnostic>>;

struct TypedExpr {
    span: Span,
    domain: ValueDomain,
    operation: TypedOperation,
}

enum ValueDomain {
    Scalar(ScalarType),
    Aggregate { scalar: ScalarType, width: AccumulatorWidth },
}
```

These are design sketches, not stub APIs in the crate. The implemented syntax
types must not be mistaken for a checked program. An arithmetic operation's
domain is distinct from the boolean result of a comparison. Checked handle reads
and total physical-slot specification reads remain different operations; lazy
selection remains distinct from eager boolean evaluation. Pre-state reads are
explicit, including keys computed by pure folds.

We accept a temporary duplicate syntax frontend in exchange for an independent
behavioral oracle. We accept arbitrary-precision integer parsing through
`num-bigint`, and `serde`/`serde_json` for deterministic development interchange,
instead of narrowing literals or implementing a second big-integer library.
Verifier, interpreter and backend crates will be split out only when they have
meaningful implementations. A Python subprocess bridge and multiple untyped
backend-specific semantic models were rejected: both preserve the dependency
and divergence this migration is intended to remove.

## Verifiable delivery sequence

| Increment | Native behavior | Status |
|---|---|---|
| 1 | Cargo workspace, exact literals, complete v1.1 syntax, diagnostics, AST JSON and `parse` CLI | Implemented in this branch |
| 2 | Full declaration/expression analysis, lexical bindings, footprints, folds, effects, purity and resolved typed IR; `check` | Next |
| 3 | Checked interpreter, atomic transactions, projections and persistence compatibility | Pending |
| 4 | Z3 integration, exact/frame proofs, fail-closed verdicts and counterexample replay; `verify` | Pending |
| 5 | Native Rust/GLSL/WGSL/CUDA emitters, proof-gated CLI, packaged single binary | Pending |
| 6 | Retire Python production entry points, retain optional differential tests | Pending |

Each increment must pass its native tests and the relevant Python differential
corpus before the next begins. The first increment compares full normalized
ASTs against the examples and statically extracted fixtures from the existing
test suite. Native CLI tests run with an empty `PATH`. The compiler has no
runtime dependency on the test harness or Python package.

## Compatibility and remaining risks

Parser diagnostics carry source positions; their wording need not match Python.
The parser enforces a 64-level nesting resource limit with an error rather than allowing
unbounded call-stack growth. Large integer and decimal syntax is preserved until
contextual range checking in increment 2; successful parsing does not admit an
oversized scalar into execution.

Later native code generation must reproduce the current canonical program
identity or explicitly version it. Arena IDs are internal and must not silently
become persistent wire identifiers. The existing little-endian argument codecs,
WAL sequences/checksums, snapshots and restart behavior remain compatibility
tests for the interpreter/persistence increment.

The standalone runtime retains its own lockfile for existing workflows that copy
that crate outside this repository. The root `Cargo.lock` is authoritative for
workspace builds.
