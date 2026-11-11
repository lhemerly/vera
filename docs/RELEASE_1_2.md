# VERA 1.2 native release candidate

The production toolchain is Rust: syntax, semantic analysis, resolved typed IR,
interpreter, projections, three-obligation verification, concrete replay,
Rust/GLSL/WGSL/CUDA emitters, persistence, ordered optimistic batches and fuzzing.
The `vera` executable links native Z3 and never launches Python. Python remains
an optional conformance oracle, installed as `vera-reference`.

## Verified release checks

On the final implementation tree:

| Check | Result |
|---|---|
| Native workspace tests, including actual Mesa WGSL/GLSL dispatch | 219 passed |
| Complete historical language/backend suite | 175 passed |
| Native/Python syntax corpus | 148 sources, zero mismatches |
| Native/Python semantic, layout and WCET corpus | 148 sources, zero mismatches; three expected conservative tier upgrades |
| Native/Python execution and projection differential | 1,968 cases, zero mismatches |
| Formatting and strict workspace/all-target Clippy | Passed |
| Standalone Rust engines and shared-runtime engines | Compiled and executed |
| CUDA lowering | Compiled and executed in portable C++ host mode |
| Typed intents, snapshots and WAL | Exact historical golden byte compatibility |
| Packaged executable | Extracted and run with empty PATH, including proofs |

`tools/package_native.sh` produces the Linux archive, third-party notices and
SHA256 checksum. CI repeats native tests and packaging and retains the artifact;
it also runs the reference suite on Python 3.11–3.14. No tag or GitHub Release is
published automatically.

## Compatibility and operating limits

The language contract remains v1.1. Stateful folded footprint keys conservatively
use Tier 2, including old-state dependencies. A legal column named `id` is
represented using a nested `columns` map in CLI rows/wildcard projections to keep
its value separate from the row handle. Ordinary row shapes remain compatible.
WAL v2, snapshot v2, scalar encodings and program/schema identities remain byte
compatible with the previous engine.

Snapshot v2 does not carry a WAL checkpoint cursor. CLI recovery therefore
rejects combining a snapshot input with a WAL, and rejects snapshot outputs that
alias the log. Recover a WAL from the same original setup baseline and explicitly
rotate logs when checkpointing. Failed transactions and failed durable writes
preserve the previously published state.

All solver/resource failures reject verification. Limits are explicit rather
than silently reducing source capacities. The default interpreter state budget
is 256 MiB; verifier limits and native batch budgets are configurable. The Linux
archive requires x86-64, glibc 2.39+ and a compatible C++ standard library.

Native NVIDIA device execution and the accelerated io_uring path on an enabled
kernel still require validation on those target hosts. The mandatory software
GPU tests and positional-write fallback do not claim to validate that hardware.
