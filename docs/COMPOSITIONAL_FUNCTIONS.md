# Compositional pure functions

Pure scalar functions are implemented across the frontend, native execution,
verification and generated Rust/GLSL/WGSL/CUDA targets. They provide reusable
verified contracts without copying a callee's body into every caller proof.

```vera
function increment(x : u64) -> u64
  require room : lt(x, 18446744073709551615);
  ensure exact : eq(result, add(x, 1));
  body { yield add(x, 1); }
```

A call is `increment(value)`. Function declarations may appear in any source
order. Dependencies must be acyclic: recursive functions are not part of this
terminating first-order function feature. `result` is visible only in function
ensures. Parameters and fold/local bindings have isolated resolved identities;
caller and callee names cannot capture one another.

## Scope and execution

Functions take and return existing scalar types, including enums, bytes32,
decimals and typed handles. Bodies are pure expressions, pure bounded folds, and
calls to other pure functions. They cannot read tables, occupancy, aggregates,
old state or effects, or perform writes, allocations or emissions. Purity is
checked even in inactive/dead branches and unused functions.

Function requires and ensures refer only to parameters and, for ensures, result.
Calls are not yet supported inside a function's own contract. Call expressions
are supported in ordinary transition/specification/projection expressions;
the frontend checks their types once. General structured signatures, recursion,
higher-order functions and stateful component contracts are not implemented.

Actual arguments evaluate left to right exactly once, then narrow to declared
scalar types. A function evaluates its requires in source order. A false
requirement traps with `ERR_FUNCTION_PRECONDITION`; an earlier arithmetic error
is not hidden by a later false requirement. A caller may explicitly guard a call
when rejection is intended. Lazy `select` does not evaluate an unchosen call.
The actual body executes in a fresh binding scope and its return value narrows
to the declared type. Normal execution does not replace a body with a contract
or automatically enforce ensures. The existing verified workflow is the proof
admission gate; `--unverified` remains an explicit bypass, not a semantics change.

Runtime instruction metadata accounts for transitive calls. Helper bodies are
retained once in IR and generated once per target, in dependency order. Generated
Rust uses scalar `Result` helpers; GPU helpers propagate scalar results and
failures explicitly rather than pretending every function returns a boolean.

## Independent proof admission

Frontend `CheckedProgram` means well-formed and typed, not proved. A private
invocation-scoped verification session is the only authority to use function
contracts as assumptions. Each reachable dependency must first pass:

1. Safety: every return from inputs satisfying the requires satisfies the
   ensures, including postcondition expression definedness.
2. Panic freedom: reachable evaluated requires/body operations cannot trap.
3. Non-vacuity: a concrete typed input can satisfy the requirements and return.

A function proof models only its scalar parameters and return, not unrelated
program tables or state invariants. An inconsistent state invariant therefore
cannot make a false pure-function contract appear proved. Native replay of a
function witness allocates no unrelated table state.

At a call site, the encoder checks the instantiated requires as caller trap
obligations. It may assume ensures only after independent dependency admission.
A deterministic uninterpreted function application represents the result;
identical arguments to the same helper have equal results, but different
arguments/functions are not conflated. Return-domain and postcondition
assumptions are guarded by the actual successful call path. An inactive branch
cannot constrain a live caller or make an invalid precondition disappear.

This is a sound abstraction, not unrestricted inference from the callee body.
A weak but correct contract can leave a caller unproved. Every candidate SAT
answer still requires real native execution. If a summary-based candidate does
not replay, verification reports `unknown` with a contract-summary precision
reason; it does not fabricate a counterexample, silently strengthen the summary,
or turn blocked candidates into a proof. Strengthen an independently justified
contract when the caller needs facts it does not expose.

## Reuse and repair evidence

Dependency discovery and proof admission are iterative and follow a deterministic
DAG order. A diamond dependency is proved once per verification session.
`verify_all` emits each function proof once, followed by transition proofs.
Selected-transition verification proves its reachable helper dependencies,
including helpers in invariants and declared footprints. There is no persistent
proof cache or externally supplied proof token. Changing a program/body/contract
starts a fresh session and changes its canonical semantic identity.

Verification JSON identifies `target_kind` (`function` or `transition`) and
includes direct-edge `contract_dependencies` evidence rather than copying
transitive proof DAGs into each caller. `verify_all` emits each target proof once;
selected-target evidence reports its direct admitted dependencies. A function counterexample reports concrete parameter and return
values, the failing named ensure/value or error and source spans. Function
precondition failures in transition replay retain their source location. A pure
function witness has empty state tables/outbox, not a fabricated stateful input.
Non-vacuity means a function returned, not that every ensure held on that witness.

## Scaling and actual limitations

Caller SMT instantiates contract-sized obligations rather than a callee's whole
body. Structural tests confirm an unchanged caller encoding when an independently
verified helper's fold body grows from 8 to 128 iterations. Generated-artifact
tests cover 1, 16 and 256 call sites retaining one helper, and frontend tests
retain one definition per node of repeated-call DAGs while tracking their
transitive runtime cost.

There are no default solver timeout, expression, state-cell, SMT-byte, event,
fold-unroll, witness-attempt, call-depth or function-count policy ceilings.
[Proof budgets](PROOF_BUDGETS.md) are explicit opt-in controls. Declared finite
loops and scalar representations retain their semantics; `unknown` and errors
never count as proof. Current bound/product metadata uses checked `u32`, runtime
instruction costs checked `u64`, and targets retain their actual numeric/index
representability limits. Host memory, solver capacity and recursive expression
parsing/evaluation can still be exhausted. In particular, removing the old
parser nesting policy does not turn the recursive parser into an iterative one:
sufficiently deep source/execution trees depend on available host stack.

## Remaining language-wide work

Stateful component contracts require explicit read/write/effect frames, alias
rules and invariant ownership; pure scalar functions do not supply those.
General named records and tagged unions need a single resolved type/layout
contract across all targets and persistence, without enumerating Cartesian
products of variants. Bounded bytes need explicit logical length/capacity,
consistent trailing-byte semantics and checked indexing/slicing. Source-language
codec primitives need precise endianness, canonical encoding, length/tag checks,
error behavior and independent round-trip proofs. Those remain future
implementation, not capabilities implied by this function feature.
