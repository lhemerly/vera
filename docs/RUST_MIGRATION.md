# Native Rust toolchain

VERA 1.2 implements the complete v1.1 language contract in Rust. The production
`vera` executable supports parsing, checking, verification, execution,
projections, replay, Rust/GLSL/WGSL/CUDA code generation and bounded fuzzing.
Python remains an optional independent conformance oracle, exposed separately as
`vera-reference`. The native compiler never invokes it.

## Representation and ownership

The syntax tree preserves exact literals and source spans. `analyze(Module)` is
the sole constructor of an immutable `CheckedProgram`. Its arena uses stable
expression, binding, declaration and column IDs. Every expression has a resolved
scalar and evaluation width; aggregate arithmetic and its operands use explicit
128/256-bit domains. Assignment, allocation, effects and fold yields check any
narrowing back to scalars.

The interpreter, SMT encoder and all four generators consume that checked
program directly. Old-state selection applies to entire key subtrees, including
folds. Lazy selection and eager boolean operators are different IR operations.
Fold binding IDs remain distinct across sibling scopes. Stateful footprint keys
conservatively select Tier 2, including dependency through an old-key fold.

`runtime` owns validated state, atomic staged transactions, typed effects and
traces. `projection` uses the same evaluator and types. `batch` uses owned
candidate snapshots and ordered validation/re-execution; no shared mutable state
or unsafe optimistic memory access is involved.

## Verification

Z3 runs through native Rust bindings and a statically linked solver. The pinned
native solver version is recorded in `.cargo/config.toml`; Cargo.lock pins the
Rust dependencies. Building downloads the official solver archive. This is a
native C++ dependency, not a Python dependency.

The verifier checks partial correctness, evaluated-operation trap freedom, and
concrete non-vacuity separately. Tables retain their declared capacities. Exact
finite occupancy, generations, physical columns and aggregate sums constrain the
initial state. Aggregate SSA updates and ordered write logs avoid repeatedly
expanding equivalent state expressions. A row-local invariant is rechecked only
at touched handles when unchanged rows retain its precondition; global and
cross-row conditions remain exact. Foreign-existence framing requires the target
occupancy and generations to remain unchanged.

Logical unique lookup overapproximates physical bounded-index execution, allowing
extra committing paths if an index would saturate. Thus UNSAT remains sound.
Every SAT safety counterexample, trap or committing witness is reconstructed,
validated and replayed through the native interpreter. Unreplayable candidates
are blocked within an explicit attempt limit; exhaustion yields UNKNOWN. Timeout,
resource exhaustion, invalid models and UNKNOWN never authorize compilation.

Limits are explicit: the default verifier allows 4096 state cells, 1024 unrolled
iterations, eight witness attempts and 10000 ms per solver obligation. CLI options
can change those limits; no capacity is silently reduced. The interpreter limits
estimated state storage to 256 MiB by default; `State::empty_with_limit` admits a
larger explicit host budget. Parsing limits nesting to 64 levels.

## Persistence and CLI recovery

Canonical source/schema fingerprints retain the historical Python AST identity,
including exact decimal notation and Unicode escaping; internal arena IDs do not
enter the wire identity. Native codecs preserve all scalar widths. Golden vectors
from the previous standalone engine prove exact intent, snapshot and WAL bytes.
Snapshots validate scalar domains, generations, indexes, sums, effects, checksum
and invariants. A complete bad WAL is rejected before replay publishes state;
torn tails recover on reopen. Logged execution stages a transaction, appends and
syncs its intent, then publishes state. An I/O error never publishes that state.

`run --wal PATH` replays that log from the supplied original `--setup` baseline
(or the valid empty state), then durably records the new attempt. Reuse the same
baseline on subsequent invocations. `replay --wal PATH` recovers without a new
attempt. `--snapshot-out PATH` checkpoints the resulting state. Snapshot v2 does
not contain a WAL cursor, so the CLI rejects `--snapshot-in` with `--wal`; callers
must rotate/checkpoint logs explicitly rather than replaying old intents twice.
Snapshot output cannot alias the WAL, including existing hard links/symlinks.
Setup accepts flat rows with an `id` handle or nested
`{"id": HANDLE, "columns": {"column": VALUE}}` rows. A legal column named `id`
uses the nested form in CLI output and wildcard projection rows so its value
cannot overwrite the handle. Ordinary table output retains the historical shape.

## Backends and validation

Generated Rust uses only `std`, or optionally `vera-rt` for the shared WAL engine.
The GPU backends share exact limb arithmetic with 512-bit intermediates and
retain declared table capacities, sequential evaluation, rollback, checked
handles, indexes, sums, effects and laws. GPU effects are counted/digested rather
than delivered to an external consumer.

Native tests execute standalone Rust, CUDA's portable C++ mode, WGSL through Rust
wgpu on Mesa, and GLSL through EGL. The independent Python corpus checks full
ASTs, semantic admission, fold/WCET metadata and execution at scalar boundaries.
Three legacy Tier-1 classifications become conservative Tier 2 because their
keys depend on state. The fixtures remain admitted; the new classification is
intentional. The full historical language/backend suite is retained.

Native NVIDIA compilation/device execution and accelerated io_uring on an
enabled kernel remain target-host validation, not claims made by Mesa or the
portable fallback tests. Native Linux release archives require x86-64 glibc 2.39+
and a compatible C++ standard library. `tools/package_native.sh` creates the
archive/checksum; CI uploads it without automatically publishing a release.
