# Owned state and independently verified operations

A component exclusively owns mutation of its declared tables and the invariants
that define its public boundary. Its operations have explicit read, write and
effect frames and independently verified contracts. Transitions compose calls
without expanding operation bodies in their SMT proofs.

```vera
table T capacity 2 { key id : id64<T>; col v : u8; }
effect Tick(v : u8);
component Counter owns [T] {
  invariant bounded { forall k in T : le(T[k].v, 10); }
  operation inc(k : id64<T>)
    reads [T[k].v] writes [T[k].v] effects [Tick]
    require live : table_exists(T, k);
    require room : lt(table_get(T, k, v), 10);
    ensure exact : eq(table_get(T, k, v), add(old(T[k].v), 1));
    ensure count : eq(effect_count(), 1);
    ensure tag : effect_is(Tick, 0);
    ensure payload : eq(effect_arg(Tick, 0, v), table_get(T, k, v));
    body {
      let %v : u8 = table_get(T, k, v);
      table_set(T, k, v, add(%v, 1));
      emit Tick(add(%v, 1));
      commit;
    }
}
transition twice(k : id64<T>) reads [T[k].v] writes [T[k].v]
  require live : table_exists(T, k);
  require room : le(table_get(T, k, v), 8);
  ensure exact : eq(table_get(T, k, v), add(old(T[k].v), 2));
  body { call Counter.inc(k); call Counter.inc(k); commit; }
```

## Ownership and frames

Each table has at most one owner. An operation and its component invariants may
observe only tables owned by that component, including existence tests, old-state
observations and dead expression branches. Transitions may read owned tables
through their declared read frames and specifications, but may mutate them only
through operations. Component declarations are source-order-independent.

Operation frames retain their declarations separately from inferred body access.
A wildcard `T[*].v` covers every row; a keyed frame uses an input-only key.
Both read and write frame keys must be proved defined under the operation's
requirements before admission, including unused declared frames. Undefined keys
produce concrete frame-contract evidence, not invented interpreter body traps.
State-dependent operation frame keys are rejected rather than assumed stable
across mutation. Operation postconditions must observe columns covered by its
read or write frame. Occupancy and generation are immutable within this supported
operation subset; live-handle tests do not grant an allocation capability.

The effect frame lists the only permitted emitted variants, including emissions
inside folds. Counts, order and payload constraints belong in ensures. Declaring
an effect frame alone does not promise an emission count.

## Independent proofs and sound composition

An operation is proved from structural state validity, its own component's
invariants and its named requirements. Unrelated global or other component
invariants are not assumptions of that proof. It must preserve its owner's
invariants, satisfy ensures, avoid traps, and have a concretely replayed successful
execution. Initial state admission still validates all invariants.

A private per-verification session admits an operation summary only after these
obligations pass, including any pure function dependencies. At each call the
caller must establish the operation's requirements and owner boundary invariant.
Missing operation requirements are caller trap obligations, not assumptions or
handled transition-level precondition failures.

The caller summary freshens only the declared write-frame cells and preserves
all other cells, occupancy and generations. Tracked sums are recomputed exactly.
It appends a typed, framed effect suffix and assumes only independently admitted
postconditions and owner invariants. This is an overapproximation, not body
execution. A weak summary can produce an abstract candidate that does not replay;
that result remains `unknown`, never a proof or a fabricated counterexample.

Global relational invariants remain obligations of the enclosing transition.
They are not silently treated as part of an operation's private contract.
All operations, including unused operations in operation-only modules, appear in
full verification and CLI code-generation proof gates.

## Invocation boundaries and targets

Call arguments evaluate once, left to right, before the invocation snapshot.
Each operation's `old` is its call-entry state, and its effect observations start
at that call's outbox boundary. The enclosing transition retains its own original
snapshot and complete transaction-local outbox. A failed call or later guard
rolls back the complete outer transaction, including prior operation writes and
effects. Local parameter/SSA bindings never leak between repeated calls.

The native interpreter and generated Rust execute operation bodies, not summaries.
As with other contracts, these APIs do not dynamically assert postconditions.
Generated Rust uses one private helper per operation, not new public intent
variants. GPU targets execute helpers with call-local state/effect monitors;
CUDA host, real EGL GLSL and WGSL-to-GLSL execution share these boundaries.

## Current supported subset

This increment supports leaf, no-return stateful operations over existing rows.
Operations may use pure verified functions and bounded folds. Transitions may
compose multiple operations, including calls inside body folds. Operation-to-
operation calls and allocation inside operations are rejected explicitly. An
allocation frame must account for occupancy, generations, indexes and row
initialization before allocation can be supported soundly. These are explicit
language-feature boundaries, not validation count/depth/resource ceilings.

There are no new default proof, operation-count or call-count caps. Existing
optional diagnostic budgets remain unset by default. Finite declared fold bounds,
checked representation widths, host allocation and solver/device resources still
apply. Independent operation proof currently models the complete table schema,
even though its invariant assumptions are owner-scoped; this is not a claim that
proof memory is independent of unrelated table capacities.

```bash
vera verify examples/components.vera
vera verify examples/components.vera --operation Counter.inc
vera run examples/components.vera --transition twice --param k=2 \
  --param accept=true --setup examples/components_setup.json
```
