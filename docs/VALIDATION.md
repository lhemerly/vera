# Validation of the completed v1.1 implementation

Reviewed baseline: `84badd1ed1372484f995586c71529320305b82d1`.

The final validation groups cover **153 Python/backend test cases** and **5
Rust unit tests**. Toolchain checks used Python 3.14, Z3 5.4.1, Rust/Cargo,
glslangValidator, GCC/C++, and wgpu with Mesa llvmpipe.

| Check | Result |
|---|---|
| `python -m pytest tests -q -ra --ignore=tests/test_gpu_backend.py` | 150 passed |
| Three GLSL backend tests (`tests/test_gpu_backend.py`) | 3 passed, including actual correct/broken furnace execution |
| `cargo test --manifest-path rust/vera-rt/Cargo.toml --locked` | 4 library tests + 1 dispatcher stress test passed |
| Ruff formatting and fatal-error checks; `git diff --check` | Passed |
| `python -m build --no-isolation` | Wheel and source distribution built |
| Fresh wheel extraction/import | Proved a program, emitted all four targets, compiled/executed Rust, and round-tripped a snapshot |
| Exported SMT-LIB execution through Z3 | `unsat`, `unsat`, `sat` for the counter's three obligations |

Actual WGSL kernels and GLSL furnaces executed on the software OpenGL adapter.
CUDA's shared arithmetic and transaction implementation compiled and executed in
portable C++ host mode. Native NVIDIA CUDA compilation/device execution was not
available. The kernel denied io_uring setup, so persistence ran end to end using
the positional-write fallback; the io_uring path was compiled and reviewed.

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
