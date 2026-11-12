# Language-wide contract improvements

Audit base: `0972d8be62466f2798c7654f46d9fd6a7c850c3a`.

This work improves VERA's observable transactional interfaces, compositional
verification and repair feedback. It does not claim that every longer-term
language feature is implemented.

## Implemented

- Exact transaction-local effect postconditions for count, variant, order and
  named scalar payload fields, including pure specification folds.
- Shared symbolic outbox arrays and conditional prefix-count stores rather than
  enumeration of all event sequences or payload combinations.
- Interpreter/replay/native-fuzz semantics, exact GPU monitors and generated
  Rust's explicit named postcondition-check API.
- Replay-derived JSON with ordered named typed effects, false versus undefined
  ensures, concrete values/errors and precise source spans.
- Pure, first-order, acyclic scalar functions with independently verified
  contracts; source-order-independent dependency resolution; real-body runtime
  execution; one generated helper per definition.
- Private invocation-scoped proof admission. Callers check callee preconditions
  as obligations and use admitted deterministic summaries, not inlined bodies.
  Weak summaries fail closed as unknown when candidates cannot replay.
- Iterative proof-DAG traversal and direct-edge dependency evidence. Each helper
  is proved once per session; transitive proof graphs are not copied into every
  caller's result.
- CLI verification of functions as well as transitions, selected `--function`
  verification, and function-aware artifact proof gates. Function-only programs
  cannot pass vacuously because they have no transitions.
- Shared tracked-sum prefixes, removal of redundant row-local all-row selection
  expansion, incremental physical-witness candidate search and actual encoding
  metrics.

## No imposed validation ceilings

The owner's correction supersedes the initial budget defaults. There is now no
default solver timeout, state-cell, expression-step, SMT-byte, candidate-effect,
fold-unroll or witness-attempt ceiling. All optional controls are absent by
default (`None` / JSON `null`) and apply only when explicitly selected. The
arbitrary fixed parser-nesting, fold-depth/product, function-count/call-depth,
GPU effect-buffer and interpreter-state-byte policy ceilings have been removed.
No large sentinel defaults stand in for unbounded configuration.

Declared finite loop bounds, scalar/handle representations and soundness remain.
Unknown/errors are never called proofs. Actual `u32` bound/product/GPU-index,
`u64` instruction-cost, host address-space, allocation, target compiler/device and
solver limits still exist and are reported rather than silently reducing state.
Recursive expression parsing/evaluation remains dependent on available host
stack; removing a policy check is not a claim of unlimited physical resources.
Explicit host execution/batching configuration is separate from proof defaults.

## Current scope and remaining work

| Priority | Status |
| --- | --- |
| Effect postconditions and symbolic outbox | Implemented for existing scalar payloads |
| Pure reusable verified functions | Implemented for scalar and structured values with independent dependency proofs and reusable summaries |
| Stateful component contracts | Not implemented; need invariant ownership and explicit read/write/effect frames |
| General records and tagged unions | Implemented across functions, effects, locals, state, projections and canonical host codecs |
| Variable-length bounded bytes/strings and source codecs | Not implemented; bytes32 and host persistence codecs remain existing facilities |
| AI self-repair | Precise verifiable evidence and proof boundaries, not autonomous specification weakening or automatic patch admission |

Structured values now share one canonical type/layout/validation contract across
runtime, SMT, generated targets and persistence. Shared datatypes and helpers avoid
Cartesian expansion of interface variants. Bytes/codecs need explicit
length, capacity, endianness, canonical representation, checked offsets and
round-trip obligations. Source-level bytes/codecs remain substantive implementation, not a capability
implied by the canonical host codecs.

## Validation and scaling evidence

Tests cover independent function obligations, missing caller preconditions,
invalid dependency admission, purity through dead code, deterministic UF
congruence without conflating distinct arguments/functions, weak-summary replay,
source order, binding isolation, atomic argument evaluation, named requirement
traces, function-only CLI gates and actual generated artifacts.

Scaling regressions retain one helper for 1/16/256 call sites; independently
verify a 64-helper chain with exactly 63 direct evidence edges; preserve an
identical caller SMT encoding as callee body unrolling grows from 8 to 128; and
admit more than the removed function-count/fold/state-cell policy thresholds.
These are structural tests, not a promise of linear solver runtime.

For fixed bounded emissions with a symbolic queried position, sampled safety
SMT sizes at bounds 8/16/32/64 were 12,430 / 23,243 / 44,859 / 88,091 bytes.
Row-local state proof sizes at capacities 8/16/32/64 were approximately
10 / 20 / 40 / 80 KB. A complex 32-iteration symbolic-length/all-payload proof
exceeded an explicitly selected 10-second timeout and remained unverified.
There is no timeout by default.

Full validation includes generated Rust, CUDA host execution, real EGL GLSL and
WGSL-to-GLSL execution, persistence/canonical-identity vectors, verifier review
regressions and the 50,000-iteration runtime transfer test. Formatting and
whitespace checks are required. On installed Rust 1.95, strict Clippy encounters
three pre-existing `manual_is_multiple_of` warnings in unchanged `vera-rt` code;
all-target workspace Clippy is run allowing only that legacy lint. The delivery
report records the final test totals and actual outcomes.

See [effect contracts](EFFECT_CONTRACTS.md),
[compositional functions](COMPOSITIONAL_FUNCTIONS.md) and
[optional proof controls](PROOF_BUDGETS.md).

Final validation for this increment: full workspace run 304 passed, zero failed
or ignored, followed by a final compiler rerun after the direct-edge evidence
regression: 300 passed, zero failed or ignored. Together with the five unchanged
runtime tests, all 305 tests are covered against their final source inputs.
The workspace runtime stress completed 50,000 iterations. Formatting, whitespace
and all-target Clippy (with only the pre-existing lint noted above allowed) passed.

The structured increment includes cold-cache GPU timing and growth evidence in
[GPU scaling](GPU_SCALING.md). It removes a reproduced wide-multiply shader
compiler bottleneck without weakening arithmetic/hash semantics or adding a
validation cutoff. Explicit runtime value trees can still expand repeated nested
shapes; shared type metadata does not imply compact physical values.

Final structured milestone validation: 356 workspace tests passed, zero failed
and zero ignored, with real GPU execution required and Mesa shader caching
disabled. All-target lint passed allowing only the pre-existing runtime lint
noted above; formatting and whitespace checks passed.
