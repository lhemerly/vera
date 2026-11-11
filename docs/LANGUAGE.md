# VERA language reference

VERA programs describe finite transactional state machines. The implementation
has a Python reference interpreter, an SMT verifier, a Rust engine, and compute
furnaces for GLSL, WGSL and CUDA. Projection frames are boundary data rather than
HTML or hidden I/O.

## Modules, tables, and values

A file optionally begins with `module Name;`. Names are case-sensitive. Tables
have a power-of-two capacity from 1 through 2^32, a primary `id64<Table>` key and
fixed-width columns. Table memory uses column arrays. A table's capacity is part
of its type and state schema; execution never silently reduces it.

```vera
table Account capacity 8 {
  key id : id64<Account>;
  col balance : dec64 [tracked_sum];
  col external_id : bytes32 [unique];
  col status : enum { Open, Closed };
}
```

| Source type | Representation / domain |
|---|---|
| `bool` | Exactly `false` or `true` |
| `u8`, `u16`, `u32`, `u64` | Unsigned integers of the stated width |
| `i64` | Signed two's-complement 64-bit integer |
| `dec64` | Signed 64-bit scaled integer, scale 10^-8 |
| `dec128` | Signed 128-bit scaled integer, scale 10^-18 |
| `sym32` | Deterministic 32-bit symbol value |
| `bytes32` | Exactly 32 bytes |
| `id64<T>` | Strongly typed table handle |
| `enum { Tag, ... }` | One of 1–256 declared tags, encoded as an unsigned byte |

A handle contains a slot in the low `log2(capacity)` bits and a 32-bit
generation above it. Other high bits must be zero. Allocation chooses the lowest
free slot and increments its generation. Missing or stale handles fail
`table_get` and `table_set`; `table_exists` returns false. Exhausted generations
and full tables revert without changing state. There is no deletion syntax.

Integer literals are raw integer/scaled values. Fractional literals are exact
human decimals: `1.0` means 100000000 in dec64 and 1000000000000000000 in dec128.
Excess fractional precision is rejected rather than rounded. Strings denote
`sym32` literals (SHA-256's first four bytes, interpreted little endian), with
compile-time collision detection. Enum tags can be bare identifiers. An integer
literal in a bytes32 context is encoded as 32 little-endian bytes; the CLI accepts
an explicit 64-hex-digit byte string.

There are no implicit numeric conversions. Literal types are inferred from their
context. `id64<T>(integer)` explicitly interprets an integer's 64-bit bit pattern
as a handle; it does not allocate or validate a row.

## Expressions and evaluation

Canonical operators use prefix notation. The specification parser also accepts
ordinary infix arithmetic, comparison and boolean operators.

| Operators | Meaning |
|---|---|
| `add`, `sub`, `mul`, `div`, `rem` | Checked arithmetic |
| `min`, `max` | Ordered minimum and maximum |
| `eq`, `ne`, `lt`, `le`, `gt`, `ge` | Typed comparisons |
| `and`, `or`, `xor`, `not` | Boolean operations |
| `select(condition, a, b)` | Evaluate only the selected branch |
| `table_get(T, key, column)` | Read a live row or trap |
| `table_exists(T, key)` | Test the whole handle, including generation |
| `table_lookup(T, column, value)` | Resolve a unique secondary key to a handle |
| `table_exists_unique(T, column, value)` | Probe a unique secondary index |
| `T[key].column` | Total physical-slot read in specification expressions |
| `old(T[key].column)` | Transaction-start column value |
| `sum(T.column)`, `old_sum(T.column)` | Current or transaction-start tracked sum |

Expression operands evaluate left to right. Both operands of boolean `and`,
`or`, and `xor` evaluate; use `select` when an unsafe expression must be skipped.
Integer division truncates toward zero and remainder has the dividend's sign.
Division by zero, signed MIN/-1 and out-of-domain results trap. Decimal
multiplication/division use widened intermediates, require an exact scaled
result, and reject overflow or a nonzero fractional remainder.

Tracked accumulators cover capacity times the column's entire range. Columns up
to 64 bits use a 128-bit accumulator; dec128 columns use 256 bits. Arithmetic
containing an aggregate is checked in that accumulator width, retaining decimal
scaling. Ordinary per-row values remain in their declared scalar width.

`[unique]` columns are immutable after allocation. Secondary indexes use bounded
Robin Hood probing with at most 16 probes. Duplicate values and index saturation
are handled reverts. Index mutation is part of the transaction.

## Transitions, preconditions, and effects

```vera
effect Changed(key : id64<Account>, value : dec64);
transition freeze(k : id64<Account>)
  reads [Account[k].status] writes [Account[k].status]
  require exists : table_exists(Account, k);
  body {
    let %status : enum { Open, Closed } = table_get(Account, k, status);
    guard(eq(%status, Open), ERR_ALREADY_CLOSED);
    table_set(Account, k, status, Closed);
    commit;
  }
```

A transition has typed parameters, declared read/write footprints, named
`require` clauses, optional named `ensure` clauses and a body ending in exactly
one final `commit;`. Requires execute in declaration order and stop on the first
false clause. Earlier arithmetic failures are not hidden by a later false
require. Each `%name` has a single definition in its lexical scope.

Body statements are `let`, `guard`, `table_set`, allocation bindings, `emit`, and
`commit`. Allocation initializes every column exactly once:

```vera
let %k : id64<Account> = table_alloc(Account, {
  balance : 0, external_id : 0x1234, status : Open
});
emit Changed(%k, 0);
```

A successful transition publishes state and effects together. Every failure
restores columns, occupancy, generations, aggregates, secondary indexes and the
outbox. Effects are typed records in an outbox; the language itself performs no
network, filesystem, clock or random operations.

Declared footprints must cover actual reads and writes. `T[*].column` denotes
all slots. Input-derived footprints are Tier 1; state-dependent/allocated
footprints are Tier 2. The Rust optimistic dispatcher speculates using owned
snapshots, validates in input order and re-executes stale candidates. Its
conservative validation covers the complete state, so all tiers have sequential
intent-log semantics without unsafe mutable aliasing.

## Bounded folds

```vera
let %total : u64 = fold_bounded<8>(acc : u64 = 0, i in [0..7]) {
  yield add(acc, 1);
};
```

The inclusive integer range performs at most the declared number of iterations.
The initializer evaluates first, followed by the lower and upper bounds.
An empty range returns the initial accumulator. The index never wraps; reaching
its type's maximum ends iteration. Accumulator, index and body bindings are
lexically scoped. Up to two nested folds are admitted, with a maximum declared
bound and nested bound product of 1024. Body folds may allocate, write and emit.
Specification/projection expressions may use only pure folds.

## Invariants and verification

```vera
invariant nonnegative {
  forall k in Account : ge(Account[k].balance, 0);
}
invariant bounded_total { le(sum(Account.balance), 1000000000); }
```

A row quantifier ranges over occupied rows. Global invariants may also relate
aggregates and foreign-key existence. A caller must supply a well-formed initial
state satisfying all invariants; CLI setup and snapshot restore validate state
before admission. Transition proofs are inductive relative to those invariants.

The verifier discharges three obligations for each transition:

1. No committed execution from a valid pre-state violates an invariant or ensure.
2. No reachable, evaluated expression has an arithmetic or missing-row trap.
3. At least one concrete valid pre-state/input can commit.

The third obligation is existential non-vacuity, not a temporal progress/fairness
claim. Handled reverts such as false guards or exhausted allocation remain
allowed. Any frontend/lowering error, timeout, or solver `unknown` fails closed.
SMT encodings are part of the trusted compiler; the runtime conformance tests
check that execution agrees with their intended semantics.

## Projections

```vera
projection AccountView(k : id64<Account>) {
  allow_if : table_exists(Account, k);
  expose : [Account[k].balance, Account[k].status];
  enable_intents : [freeze when (eq(Account[k].status, Open))];
}
```

A projection returns an allow/deny result, authorized state slices, and explicitly
configured intent affordances. Stale handles do not expose a different row's
value. Simple parameter bounds are extracted from requires as UI hints; callers
must still submit intents through normal validation. Exposing a complete column
is bounded by table capacity, rather than constant time.

## Persistence and compiler entry points

Typed intent arguments use canonical little-endian bytes and strict decode
validation. A canonical program fingerprint binds intents to their transition
semantics and ignores comments and source locations. Version-2 WAL frames
contain version/length, sequence, transition ID, full payload and integrity
checks, aligned to 64 bytes. Reopening validates the
complete prefix, recovers an incomplete final frame, rejects corrupt complete
records, and resumes sequence numbers. One writer owns a log. `append` completes
the write; `sync` acknowledges durability. io_uring and positional-write backends
use the same format.

Snapshots serialize fields and effects explicitly, include a schema identity
and content checksum, validate invariants and derived data, and are atomically
replaced after sync. Old raw-memory snapshots and legacy WAL
records are incompatible with the new formats.

`check` analyzes source; `verify` proves it; `run` executes an intent; `project`
returns a projection frame; `fuzz` explores reference trajectories; `codegen`
emits Rust/GLSL/WGSL/CUDA. `run` and `codegen` require verification by default.
`--unverified` is an explicit development/fuzzing opt-out. The Python runtime and
backend APIs also support unverified programs for counterexample replay and
conformance testing.
