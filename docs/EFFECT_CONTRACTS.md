# Ordered effect contracts

An effect declaration defines a typed variant of the transaction outbox. A
transition's `ensure` clauses can now constrain exactly which effects a commit
produces, their order, and each named payload field:

```vera
effect Changed(key : id64<Account>, before : u64, after : u64);

// In a transition with parameter key and the corresponding write footprint:
// ensure count : eq(effect_count(), 1);
// ensure kind : effect_is(Changed, 0);
// ensure key_matches : eq(effect_arg(Changed, 0, key), key);
// ensure before_matches : eq(effect_arg(Changed, 0, before), old(Account[key].value));
// ensure after_matches : eq(effect_arg(Changed, 0, after), Account[key].value);
```

These are ordinary typed expressions, not a separate verifier or an unchecked
host assertion. A missing emission, extra emission, wrong variant, reordered
emission, or incorrect payload produces a failed safety obligation and an
independently replayed counterexample. See `examples/effect_counter.vera` for runnable
state-plus-effect contracts.

## Exact semantics

| Expression | Type and behavior |
| --- | --- |
| `effect_count()` | `u64`, number emitted by this transaction |
| `effect_is(Name, index)` | `bool`, whether the zero-based position exists and has this declared effect variant |
| `effect_arg(Name, index, parameter)` | The named parameter's scalar type; traps with `ERR_EFFECT_ACCESS` if that position is absent or has another variant |

Indices have scalar type `u64`; widened aggregate carriers retain all their bits.
An index above the outbox length, including above 2^64, never wraps into a valid
entry. An effect name or payload field that is not declared is a frontend error.
Payload reads preserve ordinary scalar inference, enum identity, decimal scale,
and aggregate-carrier promotion.

Only transition ensures may observe effects. Observations inside an `old(...)`
row-key expression are rejected, so the complete old key retains transaction-start
semantics. An index supplied by `old(T[key].index)` to an outer effect observation
is allowed. Invariants, preconditions, bodies,
projections, and footprint expressions cannot inspect the outbox. This prevents
the history of externally consumed effects from becoming hidden state that a
transition proof or dispatcher must validate. The query names are contextual
calls, not newly reserved identifiers.

Queries describe **new emissions from this one transition**, not entries already
present in the persistent outbox. Emissions from nested folds and effectful
arguments occur in actual left-to-right execution order. Unchosen select
branches and inactive fold iterations produce no entries. An aborted transition
publishes neither its state changes nor its effects.

`effect_is` is total. `effect_arg` is deliberately partial: an invalid payload
read makes an ensure false/undefined, so verification fails rather than proving a
property about a fabricated default value. Use lazy `select` to protect a
conditional read:

```vera
ensure conditional_payload :
  select(effect_is(Changed, 0),
         eq(effect_arg(Changed, 0, after), Account[key].value),
         true);
```

`and` evaluates both operands and is not a safe-access guard. When the protocol
requires an effect, assert its count and variant too; a conditional payload law
alone does not require an emission.

Pure bounded folds in ensures can check a sequence. Their bounds and expression
visits remain subject to normal proof budgets. Querying the outbox does not
permit specification folds to write, allocate or emit.

## Encoding and backend boundaries

The SMT model uses an exact transaction-local count and shared arrays for tags
and typed payloads. Each syntactically unrolled candidate emission conditionally
stores at the current prefix count. Each observation selects from those arrays.
The encoding shares store prefixes and does not enumerate event sequences or
Cartesian products of possible payload values. Declaring additional unrelated
effects does not enlarge a queried payload's representation. `--max-effect-events`
limits candidate emissions, including inactive unrolled ones; script bytes and
expression expansion have independent limits. Exhaustion fails closed.

GPU furnaces allocate exact transaction-local buffers only for transitions with
effect observations. They retain the existing digest for ordinary effect
tracking. Tag and payload bounds are calculated without expanding folds, with
per-transition payload stride. A buffer exceeding 4096 tag/payload cells is an
explicit `D_GPU_RESOURCE` generation error, not a truncated model.

The interpreter, verifier replay, native fuzzing, and GPU furnaces evaluate the
same effect observations. Generated Rust additionally exposes:

```rust
intent.check_postconditions(&pre_state, &committed_state)
```

This returns the first named `PostconditionFailure`, distinguishing false from a
trapped expression with its optional `cause`. It does not execute the intent,
check invariants, validate arbitrary supplied states, or establish that the
result belongs to the given pre-state and intent. It is a conformance/diagnostic
API. Normal generated execution and the interpreter still do not automatically
enforce ensures; production admission remains the verified compiler workflow.
An explicit `--unverified` invocation is still a proof bypass.

## Repair feedback

Verification JSON retains existing `law`, `violated`, pre-state, parameters and
execution trace fields. Replay witnesses also expose ordered `emitted_effects`
with source effect names and typed parameter-name maps, and `failed_ensures`
with source spans and evaluated values or runtime errors. An invalid payload
access is distinguishable from an ordinary `false` ensure. These are observations
from native replay, not unvalidated SMT assignments. Correct the implementation
or an independently justified specification; do not delete a law merely to make
the solver accept a program.

## Measured coverage and limitations

Tests reject missing, extra, wrong-kind, reordered, and incorrect-payload
emissions; test every scalar payload representation; cover empty/symbolic folds,
lazy branches, nested emitting arguments, old-outbox isolation and rollback;
and execute generated Rust and real GLSL/WGSL GPU artifacts.

Structural scaling tests double fixed emission bounds from 8 through 64 while
querying a symbolic position and verify linear retained SMT growth. A separate
8-iteration symbolic-length contract checks every payload. This is not a claim
of linear solver runtime: a 32-iteration symbolic-length/all-payload proof was
observed to hit the default 10-second timeout. It correctly remained unverified.
Arbitrary cross-row, nonlinear, or heavily nested specifications can still be
expensive. See [proof budgets](PROOF_BUDGETS.md).

General named record values, user-defined tagged payload unions, modular verified
function calls, variable-length bounded bytes and source-language codec primitives
are **not implemented by this increment**. Existing distinct effect declarations
provide typed outbox variants, not general-purpose structured values.
[Compositional functions](COMPOSITIONAL_FUNCTIONS.md) records the proposed next
increment and its required soundness and scaling boundaries.
