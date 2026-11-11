# VERA language reference

VERA programs describe finite transactional state machines. The compiler checks
and verifies those programs, executes their transitions, and generates Rust
engines or GLSL/WGSL/CUDA compute furnaces. Host applications supply initial
state, submit typed intents, and consume committed effects.

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
Values must be exactly representable at the type's scale; extra trailing zeros
are allowed and other excess fractional digits are rejected. Strings denote
`sym32` literals (SHA-256's first four bytes, interpreted little endian), with
compile-time collision detection. Enum tags can be bare identifiers. An integer
literal in a bytes32 context is encoded as 32 little-endian bytes; the CLI accepts
an explicit 64-hex-digit byte string.

There are no implicit numeric conversions. Literal types are inferred from their
context. `id64<T>(integer)` explicitly interprets an integer's 64-bit bit pattern
as a handle; it does not allocate or validate a row.

## Expressions and evaluation

Canonical operators use prefix notation. Specifications such as invariants,
preconditions, and postconditions also accept ordinary infix arithmetic,
comparison and boolean operators. Transition bodies use prefix operators.

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
Declare a column with `[tracked_sum]` to use `sum` and `old_sum`; successful
allocations and writes maintain its sum automatically. `old` evaluates its whole
argument against the transaction-start state, including expressions that compute
a row key.

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
The guarantee is relative to the declared invariants and the compiler/solver
implementation. Callers choose the invariants that express their application
rules.

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
replaced after sync. Restore rejects incompatible formats, identities and
invalid state.

The CLI provides these commands:

| Command | Purpose |
|---|---|
| `parse` | Parse source syntax; `--json` emits its syntax tree |
| `check` | Check types, declarations, footprints and bounds |
| `verify` | Run all three proof obligations, or select `--transition NAME` |
| `run` | Execute a transition with `--transition NAME` and typed `--param NAME=VALUE` arguments |
| `project` | Evaluate an authorized `--projection NAME` |
| `replay` | Recover a `--wal PATH` from its original initial-state baseline |
| `fuzz` | Search bounded execution trajectories for violations |
| `codegen` | Emit a Rust engine or GLSL/WGSL/CUDA compute furnace |

`run`, `replay`, and `codegen` require verification by default. `--unverified` is
an explicit development opt-out. `--json` supplies machine-readable output for
commands that return data.

### Initial state and CLI values

`--setup FILE` supplies occupied rows in JSON. For example:

```json
{"tables":{"Counter":[{"id":4,"value":7}]}}
```

Here `id` is the row handle, including its generation. A row can also use
`{"id":4,"columns":{"value":7}}`. The nested form keeps a column named `id`
separate from the row handle; the CLI uses it when such a column exists.
Unspecified columns receive their type's zero value. Unknown fields, invalid
handles or scalar values, duplicate slots, invalid indexes, sums, or invariants
are rejected.

CLI integer parameters accept decimal and `0x`/`0o`/`0b` forms. Decimal parameters
containing a fractional point are scaled exactly; an integer parameter is the raw
scaled value. Boolean parameters accept `true`/`false` or `1`/`0`. Enum parameters
accept a tag or its numeric index. Symbol parameter text is hashed, including text
consisting of digits; typed JSON values can supply a numeric symbol ID.
A bytes32 parameter contains exactly 64 hexadecimal digits, optionally preceded
by `0x`.

### CLI recovery

`run --wal PATH` replays existing intents from the original setup baseline, then
appends and syncs the new attempt before publishing a successful state change.
Use the same initial setup on subsequent invocations. `replay --wal PATH` recovers
without submitting another intent. Failed transition attempts remain part of the
ordered intent log.

`--snapshot-in FILE` restores a snapshot and `--snapshot-out FILE` writes the
resulting state. Snapshots do not contain a WAL checkpoint cursor, so snapshot
input cannot be combined with a WAL. Rotate logs explicitly when checkpointing;
replaying an already-checkpointed log would repeat its intents. Snapshot output
must not alias the WAL file.
