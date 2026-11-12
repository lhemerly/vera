# GPU pipeline scaling measurements

The records/unions integration exposed a slow **shader driver compilation**,
not slow SMT verification. The affected test's pipeline is parse/analyze,
WGSL generation, Naga parse/validation, Naga GLSL translation, and real EGL
compile/dispatch. That path does not call Z3.

For the structured fixture, measured stages after the exact arithmetic fixes
with Mesa shader caching disabled were:

| Stage | Time |
| --- | --- |
| WGSL generation | 9.5 ms |
| Naga parse/validation | 128.8 ms |
| GLSL translation | 26.9 ms |
| Mesa host compile/dispatch | 16.611 s |

The generated artifacts were approximately 44 KB WGSL and 124 KB GLSL. Earlier
stale generic-multiply versions were still compiling after several minutes,
with substantial CPU and around 1.15 GB resident memory. Redundant stale runs
were stopped before measuring the current artifact.

## Root cause and correction

Canonical structured hashing repeats FNV-1a64 and the existing 64-bit mix.
Previously these operations routed through generic 256-bit/512-bit arithmetic.
Shader drivers inline and specialize helpers, making repeated overly general
multiplication expensive even when the intended operation only needs low64 bits.

FNV's constant multiply and `v_mul64` now use exact small-limb arithmetic with
modulo-2^64 semantics, across CUDA, GLSL and WGSL. No timeout, model truncation,
validation threshold or weaker hash was introduced. Independent tests cover
64 boundary pairs and 10,000 random multiplication/mix64 vectors, canonical
high-word zeros and cross-target structured unique-key hashes.

## Growth checks

A regression doubles record fields, union alternatives and helper-chain sizes
from 8 to 16 to 32. Current measurements were:

| Size | WGSL bytes | Functions | Naga expressions | Generate + parse/validate |
| --- | --- | --- | --- | --- |
| 8 | 24506 | 64 | 2809 | 0.197 s |
| 16 | 36777 | 72 | 4137 | 0.295 s |
| 32 | 61433 | 88 | 6793 | 0.462 s |

These show linear source/interface expansion, not a Cartesian product of
variants or callee-body inlining. The same exercise found and fixed a generated
WGSL member-name collision (`f16`); numeric member names now use a safe prefix.

All 41 GPU tests in the focused final run passed: 26 CUDA-host artifact tests,
6 real EGL GLSL tests, and 9 WGSL/Naga-to-GLSL/EGL tests. This is separate from
SMT proof performance and not a promise of linear driver runtime.

## Remaining physical costs

GPU values currently store scalar leaves in the existing limb carrier and union
representations contain tagged payload slots. Explicit nested value shapes can
therefore use substantial physical storage. Shared helper/type definitions do
not make an exponentially large logical value small. Recursive runtime values
also materialize explicit trees even when type metadata is a compact DAG; shared
or lazy values would be a separate representation optimization, especially for
pathological repeated zero-width shapes.

Generic arithmetic dispatch still contains operations beyond a particular call's
constant opcode; a future measured regression may justify operation-specific
emission. No such claim is needed to explain the reproduced hash bottleneck
fixed here. Actual host/device memory, target compiler and numeric/index limits
remain real constraints, not imposed verification ceilings.
