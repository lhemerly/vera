# Compositional pure functions: implementation plan

Status: **design only; not implemented or accepted VERA syntax**. This document
records the next language increment, not a capability of the current compiler.

## Scope and syntax

Start with pure, first-order, scalar functions. No table access, allocation,
mutation, emission, old-state operators, recursion, function values, or external
calls. Existing scalar types and pure bounded folds remain usable. Functions may
call earlier-verified functions in an acyclic dependency graph, independent of
source declaration order.

Proposed syntax:

```text
function increment(x : u64) -> u64
  require room : lt(x, 18446744073709551615);
  ensure exact : eq(result, add(x, 1));
  body { yield add(x, 1); }
```

A call expression is `increment(value)`. `result` is reserved only in a function
postcondition. Function parameters and local/fold bindings have distinct resolved
identities; callers cannot capture callee bindings. A violated call precondition
is a trap, not a silently assumed fact or handled guard. A caller can explicitly
guard a call when rejection is intended. Function requires evaluate in order;
a later false requirement must not hide an earlier arithmetic trap.

Do not call this a stateful component-contract feature. Stateful components need
explicit read/write frames, alias rules, invariant ownership, effect frames, and
resource contracts, none of which are supplied by a pure function contract.

## Typed representation and compiler boundaries

Add `FunctionId`, a function declaration containing typed parameters, result type,
result binding, named requires/ensures, body/result expression, and dependency IDs.
Add `ExprKind::Call { function, args }`. Semantic analysis checks function purity,
arity, exact scalar types, result scope, cycle rejection and lexical binding
isolation once. Do not lower calls by cloning/substituting a callee AST.

The current `CheckedProgram` proves only frontend well-formedness. It must never
be sufficient authority to assume a function's postcondition. Introduce an
invocation-scoped `VerificationSession` with private `VerifiedContract` entries.
A contract entry is installed only after all of its required obligations and
its dependencies succeed. No deserialized or caller-constructed proof token is
accepted. A single transition verification must prove its reachable dependencies;
`verify_all` must share one session rather than recreate it per transition.

Cache identity must include the immutable program, function body/signature,
contracts, dependency identities, verifier semantics version, and configuration.
An in-memory session bound to one `&CheckedProgram` and one immutable configuration
is the simplest initial implementation. Do not ship persistent proof caching in
the first increment. Program fingerprints already serialize the source AST;
function definitions and contracts must participate in that canonical identity.

## Obligations and symbolic calls

For each function, prove independently over all well-typed scalar arguments:

1. On inputs satisfying its requires, evaluating the body is total and produces
   a value in its declared result domain.
2. Every such result satisfies each ensure, including expression definedness.
3. Its requires have at least one concrete satisfying input. This prevents an
   inconsistent specification from making every caller appear verified.

Requires themselves need ordered definedness semantics. Function postconditions
may refer only to parameters and result. Initially forbid function calls in
contracts: a contract dependency cycle otherwise needs its own analysis, and
recursive contract expansion defeats the intended scaling bound. This restriction
can be relaxed separately with explicit dependency and expansion budgets.

At a call site, evaluate actual arguments left to right once and narrow them to
the declared parameter domains. Bind the callee requires to those values and
record a trap if any evaluated requirement is undefined or false. Only if the
callee has a private verified-contract token may the encoder introduce a fresh
symbolic return value, constrain its scalar domain, and assume the instantiated
ensures. Scope these assumptions to the actual call path and successful
preconditions. Inactive `select` branches and inactive fold iterations must not
constrain an otherwise live caller.

Use one fresh symbolic result per dynamic symbolic call. Treating separate calls
as independent is a sound overapproximation, but may lose proofs involving equal
arguments. Do not “fix” that by globally equating calls with unequal arguments.
An uninterpreted function application can preserve deterministic congruence while
remaining quantifier-free; instantiated contracts, not quantified body axioms,
constrain each application. Either encoding must clearly document its precision.

A SAT answer from an abstract caller is only a candidate. Preserve current native
replay admission for safety, panic and non-vacuity. If replay rejects a candidate,
block the complete relevant assignment and continue within the configured budget.
Never report UNSAT after excluding spurious abstract candidates as a proof of the
original obligation. A weak but valid contract may lead to `unknown`, which must
fail closed. Do not invent a function result merely to make liveness pass.

## Execution and generated backends

The interpreter executes a function body, not its summary, with a fresh isolated
binding environment and eager argument evaluation. Check call requires with the
same trap behavior used by the verifier. Pure calls cannot modify transactional
state or the outbox, including through nested folds or dependencies.

Generated Rust and GPU code should emit one helper per function and invoke it;
never paste the body into each call site. Helpers preserve checked arithmetic,
lazy select, integer-width narrowing, exact decimals, enum domains and argument
evaluation order. GPU function emission follows the dependency topological order.
Any target restriction is an explicit generation error, not dropped validation.

Both normal verified workflows and explicit `--unverified` workflows must retain
runtime call semantics. Verification opt-out does not permit skipping a function
precondition check or changing evaluation order.

## Scaling and resource contracts

Proof work should be proportional to each reachable function body once plus each
call's contract size. Track independently:

- Distinct dependency count and maximum call-graph depth.
- Function-body expression expansion and bounded-fold expansion.
- Contract-instantiation expression steps and emitted SMT bytes.
- Symbolic calls after fold expansion.
- Solver time and concrete witness-search attempts.

Reject before exceeding a limit. Never trim a body, shrink a fold, omit a
precondition, or replace an unproved function with an unconstrained safe result.
Runtime WCET metadata must account for transitive calls using checked arithmetic
on the DAG; avoiding proof expansion is not permission to understate runtime cost.

## Required regression suite

- A correct arithmetic helper proves once and is called from many transitions.
- Wrong ensures, overflow, division by zero, invalid enum returns and empty
  requires fail closed; no dependent transition can assume those contracts.
- Caller missing a callee precondition is rejected, while a guarding caller is
  verified. A false later require does not hide an earlier trap.
- Lazy calls in an unchosen select branch do not trap or constrain a live path.
- Direct recursion, mutual recursion and contract-dependency cycles are rejected.
- Identical local names in caller/callee and nested callees do not capture values.
- Return values preserve all scalar representations, signedness and decimal scale.
- Interpreter, generated Rust and GPU agree on successful and failing calls.
- A deliberately weak contract produces replay-checked counterexamples or unknown,
  never a false proof; abstract non-vacuity must replay concretely.
- A diamond dependency graph verifies each function once. Compare 1, 16, 256 and
  1024 callers: retained IR/helper bodies and dependency-proof count stay constant;
  call-site SMT grows with contract size, not body size or dependency paths.
- Deep graphs and excessive symbolic calls exhaust explicit budgets predictably.
- Changing a body, contract, signature or dependency invalidates any cached result
  and changes program identity, while comments/source positions do not.

## Integration sequence

1. Add syntax, resolved function IR, purity/cycle checks and interpreter semantics.
   Keep unsupported callers rejected until all relevant backend matches are ready.
2. Add generated helpers and cross-backend conformance tests.
3. Add function-specific obligations and private verified-contract admission.
4. Add summarized calls and invocation-scoped dependency reuse, then adversarial
   replay and scaling tests.
5. Enable the public syntax only after the full suite verifies the final artifact.

## Machine-readable repair feedback

Keep proof dependency failures distinct from transition counterexamples. JSON
should identify `target_kind` (`function` or `transition`), qualified target,
obligation, source span, dependency chain, status, and the budget name when
exhausted. A function body failure includes concrete argument/result values and
an independently replayed trace. A caller precondition failure identifies the
callee and failing named requirement, plus actual arguments at that call site.
A summary-only candidate that cannot replay is `unknown` with a precision reason,
not a fabricated counterexample or a directive to weaken the specification.

Repair guidance must distinguish fixes: prove a missing guard at the caller,
correct a faulty callee body, strengthen an insufficient valid summary, or
increase an explicitly exhausted resource budget. Invalidating one function
must identify its dependent callers; do not report their old successful proofs
as current. Contracts are developer-authored requirements, not facts to rewrite
automatically to make a solver return success.

## Later records, payload unions, bytes and codecs

Introduce structured values through one resolved type algebra shared by all
backends, not independent dictionaries bolted onto the parser and runtime.
Named records require canonical field order and exact construction/projection
rules. Tagged unions require a closed tag set, tag-dependent payload domains,
and guarded projection: reading a payload for the wrong tag traps unless an
explicit lazy match/select avoids that read. Fixed-size product/sum layouts can
be lowered to shared scalar leaves and a discriminant without enumerating all
payload combinations. Never Cartesian-expand the union of input and output
variants merely to verify an interface. Charge fields, variants and active
contract branches independently against explicit budgets.

Function contracts initially support scalar signatures only. Admitting a record
or union later requires canonical argument/result domain validation, cross-backend
layout agreement, summary substitution without leaf duplication, and replay for
all new values. Effects can then carry those same typed values; they must not
acquire a second inconsistent payload type system.

Bounded bytes need an explicit capacity and a separate logical length. Canonical
values constrain the length and either zero trailing unused bytes or define them
as unobservable; every backend must make the same choice. Index/slice and codec
operations need checked offset/length arithmetic, endianness, exact integer-width
and tag validation, and typed error behavior. Verifier models must be bounded
arrays or shared expressions, not enumeration of every byte sequence.

Codec contracts should distinguish canonical round-trip guarantees:
`decode(encode(value)) = value` for every valid value, and re-encoding an accepted
byte string yields that byte string only when the decoder rejects noncanonical
encodings. Malformed lengths, trailing garbage, invalid union tags, wrong schema
identities and short payloads must not be accepted by assumption. Codec helpers
are part of the proof/implementation boundary until their byte semantics and
round-trip obligations are actually implemented and independently tested.
