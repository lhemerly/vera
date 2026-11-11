# Validation of the completed v1.1 implementation

Reviewed baseline: `84badd1ed1372484f995586c71529320305b82d1`.

The validation groups cover **175 Python/backend test cases**, **29 native
compiler tests**, and **5 Rust runtime tests**. The PR follow-up used Python 3.12.14, Z3 5.4.1, Rust 1.89.0,
glslangValidator, GCC/C++, and wgpu with Mesa llvmpipe. CI runs Python 3.11–3.14.

| Check | Result |
|---|---|
| `python -m pytest tests -q -ra` | 175 passed, including actual GLSL/WGSL execution and CUDA C++ host-mode checks |
| `cargo test --manifest-path rust/vera-rt/Cargo.toml --locked` | 4 library tests + 1 dispatcher stress test passed |
| Ruff formatting and fatal-error checks; `git diff --check` | Passed |
| `python -m build --no-isolation` | Wheel and source distribution built |
| Fresh wheel extraction/import | Proved a program, emitted all four targets, compiled/executed Rust, and round-tripped a snapshot |
| Exported SMT-LIB execution through Z3 | `unsat`, `unsat`, `sat`; exports declare `QF_AUFBV` to admit slot functions |

Actual WGSL kernels and GLSL furnaces executed on the software OpenGL adapter.
CUDA's shared arithmetic and transaction implementation compiled and executed in
portable C++ host mode. Native NVIDIA CUDA compilation/device execution was not
available. The kernel denied io_uring setup, so persistence ran end to end using
the positional-write fallback; the io_uring path was compiled and reviewed.

## Native Rust migration validation

The first native increment is syntax-only and is described in
`docs/RUST_MIGRATION.md`. Its executable has no Python subprocess bridge.

| Check | Result |
|---|---|
| `cargo test --workspace --locked` | 7 literal/lexer tests, 14 parser tests, 8 CLI tests, and 5 existing runtime tests passed |
| `cargo fmt -p vera-compiler -- --check` | Passed |
| `cargo clippy -p vera-compiler --all-targets --locked -- -D warnings` | Passed |
| `python tools/check_rust_frontend.py --binary target/release/vera` | 148 unique sources: 144 accepted, 4 syntax rejections, zero AST/verdict mismatches; native `PATH` empty |
| Standalone native executable | Parsed examples and emitted exact canonical AST JSON with an empty `PATH`, including Unix non-UTF-8 filenames |
| Native crate packaging | `cargo package -p vera-compiler --locked --allow-dirty` built and verified the package independently |
| Python distribution | Wheel/source distribution built, including the root workspace and native crate sources in the source distribution |

The differential corpus exposed Python's context-rounding unary minus for long
decimal literals. Two runtime regressions reproduced the loss of the least
significant dec128 digit in a body and a precondition. Decimal negation now uses
`copy_negate()` in both parser contexts; native syntax preserves sign and all
digits. The native CLI also handles non-UTF-8 OS paths through `args_os()` instead
of panicking in argument decoding.

## PR follow-up regressions

The original `test_unique_key_verifies` timeout was reproduced: the lookup's
trap-freedom query returned `unknown` after the configured 10-second solver
budget. Identical secondary lookups now share results for the same index,
occupancy and column versions, and retain the reference runtime's occupied-row
check. Eight consecutive reruns discharged the query in roughly 0.4–0.6 seconds
including parsing. The timeout and fail-closed verdicts remain unchanged.

Twenty additional cases cover guarded lookups for six key types, distinct and
missing keys, allocation after a miss, independent SMT state symbols, typed
constant bounds, and generated Rust naming. The naming and constant-bound
regressions reproduced false `VERIFIED` verdicts before their fixes; their exact
counterexamples now replay through the interpreter. Internal Rust accessors and
allocation parameters also avoid collisions between table/column pairs and
runtime variables. The script-emission test executes all three exported queries
through the solver's strict SMT-LIB entry point instead of matching an internal
symbol spelling.

## Behavioral regression coverage

- Over-limit folds cannot be replaced with zero and reported verified; admitted
  folds are lowered through the language bound and lower resource limits fail
  closed.
- Full-table allocation does not remove earlier arithmetic-failure paths.
- Rejected writes, allocation/index swaps and emitted effects restore all state.
- Tracked sums and aggregate-bearing expressions retain their widened domains;
  binding them to scalar values checks narrowing explicitly.
- Sequential replay matches ordered optimistic batch execution, including
  data-dependent accesses and repeated process restarts.
- Intent encoding preserves arbitrary argument counts, complete bytes32 values
  and full-width dec128 values; program fingerprints reject incompatible replay.
- Snapshot checks include contents, domains, unused bitmap bits, effects,
  aggregates, indexes, schema and invariants, with failed saves preserving the
  previous file.
- All numeric types are compared against independent integer calculations across
  Python, generated Rust and the GPU compiler's C++ execution mode.
- Cross-backend cases cover eager operands, lazy selects, source-order
  allocation fields, fold initialization order, pure folds inside old-state
  reads, generated identifier collisions, and missing-row traps.

The legacy dependent-column positive fixture now declares upper bounds and
preserves them explicitly. Its previous unbounded checked precondition could
overflow; adding only pre-state bounds would also allow a post-state bound
violation. The positive test remains a proof of a safe program. GPU smoke tests
check public entry points rather than an obsolete private helper name, alongside
the new compilation/execution tests.

## Runtime tradeoffs

Rust transactions use owned staged snapshots. Optimistic batches validate the
whole pre-state and re-execute stale work in intent order. This is a correct
conservative fallback for both footprint tiers; it does not claim fine-grained
Block-STM performance or zero-allocation transactions. GPU furnaces retain
committed effect counts and value digests, rather than delivering outbox messages.

The verifier proves source-program obligations using trusted lowering. These
regressions validate the implementation; they do not constitute a formal proof
of the compiler itself.
