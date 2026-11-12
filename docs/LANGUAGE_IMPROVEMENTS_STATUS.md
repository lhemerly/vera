# Language-wide contract improvements

Audit base: `0972d8be62466f2798c7654f46d9fd6a7c850c3a`.

This increment improves VERA's observable transactional interface and repair
feedback. It does not claim to implement the entire longer-term language plan.

## Implemented

- Typed postconditions over exact transaction-local effect count, variant,
  order and named payload fields. These work in ordinary ensures and pure
  bounded specification folds.
- Shared SMT arrays, conditional prefix-count stores and explicit event budgets.
  No enumeration of all effect sequences or payload combinations.
- Interpreter/replay/native-fuzz semantics, exact GPU effect monitors and generated
  Rust's explicit named postcondition-check API. Invalid optional accesses are
  distinguishable from false laws; lazy selection remains essential.
- Replay-derived repair JSON: ordered named typed effects, failed ensure values
  versus evaluation errors, and precise source spans. Existing evidence fields
  remain available. A committing non-vacuity witness does not masquerade as an
  execution satisfying every ensure.
- Independent expression, SMT-byte and effect-event budgets; incremental
  candidate search; shared tracked-sum prefixes; removal of redundant row-local
  all-row selection expansion. Exhaustion/unknown still rejects verification.
- An additional runnable effect-counter example without changing historical
  examples or their persisted intent identities. Contract changes themselves
  participate in the normal semantic program fingerprint.

## Deliberate boundaries

| Priority | Status |
| --- | --- |
| Effect postconditions and bounded symbolic outbox | Implemented for existing scalar effect parameters |
| General structured record values and user-defined payload unions | Not implemented; distinct effect declarations remain existing typed outbox variants |
| Reusable modular verified functions and stateful component contracts | Not implemented; implementation-ready design records required proof-admission boundaries |
| Variable-length bounded bytes/strings and source-level codecs | Not implemented; existing `bytes32` and host persistence codecs are unchanged |
| Predictable proof resource budgets | Implemented encoding limits, not an absolute RSS bound or a promise of constant solver time |
| Autonomous AI repair | Better verifiable feedback, not an automatic law-rewriting or patch-admission system |

Do not implement function calls as unchecked assumed summaries or inline every
callee at every use. The [function design](COMPOSITIONAL_FUNCTIONS.md) explains
independent dependency proofs, private verified-contract admission, cache
invalidation, abstract SAT replay and call-graph budgets. Structured values and
codecs need one canonical type/layout/validation contract shared by runtime,
SMT, generated targets and persistence. Cartesian expansion of input/output
variants would work against the intended scaling goal.

## Verification evidence

The complete workspace suite passed with real GPU tests required: 257 tests,
zero failures and zero ignored tests. This included generated Rust execution,
CUDA host execution, EGL GLSL and WGSL-to-GLSL execution, persistence vectors,
verifier adversarial regressions and the 50,000-iteration runtime transfer test.
After adding three proof-metrics tests, the entire compiler suite was rerun:
255 passed, zero failed or ignored. Together with the five unchanged runtime
tests from the full workspace run, all 260 tests are covered against their final
source inputs. Effective proof configuration and completed encoding metrics are
included in machine-readable verification results; incomplete encodings report
no completed metrics.

Formatting and whitespace checks passed. On installed Rust 1.95, strict Clippy
encountered three existing `manual_is_multiple_of` warnings in unchanged
`vera-rt` code. All-target workspace Clippy passed with only that lint allowed;
no claim is made that the unmodified strict command passed on this toolchain.

For a fixed bounded emission loop with a symbolic queried position, measured
safety-query sizes at bounds 8/16/32/64 were 12,430 / 23,243 / 44,859 / 88,091
bytes. Sample proof times were 20 / 155 / 520 / 1188 milliseconds. Row-local
state proof sizes at capacities 8/16/32/64 were approximately 10 / 20 / 40 / 80
KB. Tests assert structural growth, not these machine-dependent timings.

An 8-iteration symbolic-length contract checking every payload proves. A more
complex 32-iteration version hit the default 10-second timeout and correctly
remained unverified. Shared representation removes avoidable expansion; it does
not establish general compositional proofs or eliminate hard bit-vector queries.

See [effect contracts](EFFECT_CONTRACTS.md) and
[proof budgets](PROOF_BUDGETS.md) for precise semantics and APIs.
