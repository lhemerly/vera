# VERA implementation contract

The native Rust compiler implements the language admitted by the v1.1 parser.
The optional Python reference completed the original correctness repairs from
upstream commit `84badd1`; it remains an independent development oracle.

## Delivery sequence

1. Ground the existing parser, type checker, verifier, execution engines, and
   persistence paths; reproduce the seven review failures.
2. Establish shared typed expression and literal information, transaction
   outcomes, accumulator widths, and canonical wire formats.
3. Repair verifier soundness and transaction atomicity, keeping each regression
   executable before and after its change.
4. Complete admitted language constructs across reference, Rust, SMT, and GPU
   targets; implement safe optimistic execution for data-dependent footprints.
5. Validate cross-target behavior, package installation, compiler gates, durable
   replay, and documented examples; remove obsolete placeholders and claims.

## Architecture decision

The native implementation lowers the exact syntax AST once to an immutable
typed arena. Stable declaration, binding, expression and column IDs replace
object identity and repeated type inference. The interpreter, verifier and all
four generators consume `CheckedProgram`. Scalar and aggregate evaluation widths
are explicit, including comparison operands and scalar narrowing boundaries.
Backends reject resource limits or invalid lowering; returning a dummy value is
never a successful compilation. See [the native design](RUST_MIGRATION.md).

The Python reference retains an undo log with complete mutation coverage. Rust
executes against private staged state and publishes only successful outcomes.
Both preserve an immutable pre-state for `old` and `old_sum`. Optimistic batch
execution must produce the same state and effects as input-order execution.

## Semantic commitments

- False requires and guards, duplicate keys, full tables, saturated indexes and
  exhausted generations are handled deterministic reverts. Evaluated arithmetic
  failures and missing-row accesses remain visible to panic-freedom proofs.
- Every rejected transaction preserves rows, occupancy, generations, aggregates,
  indexes, and outbox. A commit publishes all of them together.
- Generation storage is uniformly 32 bits; keys use the table slot bits plus
  generation, with unused high bits zero.
- `select` evaluates only the selected branch. `and`, `or`, and `xor` evaluate
  both operands. Requires execute in declaration order.
- Decimal source literals are exact; arithmetic has explicit range and
  divisibility checks. Tracked sums cover the mathematical capacity-scaled range.
- Fold iteration is inclusive, bounded by its declared count, lexically scoped,
  and cannot wrap its index. Mutating folds are forbidden in specification and
  projection expressions.
- Canonical typed intent bytes preserve every argument, including all bytes of
  `bytes32` and all bits of `dec128`; decoding validates domains and lengths.
- Snapshots serialize fields explicitly, with schema/version checks. WAL frames
  have sequence and integrity checks; incomplete final frames are recoverable,
  while complete corrupt records are errors.
- Solver timeout, unknown, frontend error, or lowering error cannot produce
  `VERIFIED`. CLI execution and compilation require proof unless the caller
  explicitly selects unverified development/fuzzing mode.

## Verification

Regression tests exercise observable values, committed effects, rejected-state
equality, safe/unsafe proof verdicts, and replay equivalence. Native toolchains
compile generated Rust, GLSL, WGSL, and CUDA-compatible C++ where available.
Hardware tests report unavailable adapters explicitly; they cannot silently turn
a generated placeholder into a passing backend.
