# Bounded bytes and composable canonical codecs

`bytes<N>` is a logical byte sequence whose length is at most its declared
capacity. Capacity is part of the static type; zero capacity is valid. Values
are not packed into a 256-bit integer and have no imposed capacity policy cap.
They work in records/unions, functions, effects, locals, columns, projections,
JSON and persistence.

```vera
let %data : bytes<8> = hex<8>("00ff10");
```

Hex literals preserve leading zeros and logical length: this value contains
three bytes. Existing ordinary string literals keep their symbol semantics.
UTF-8 strings, Unicode indexing and normalization are not silently supplied by
this byte feature.

## Operations

| Expression | Behavior |
| --- | --- |
| `bytes_len(value)` | Logical length as `u64` |
| `bytes_get(value, index)` | `u8`; requires index less than logical length |
| `bytes_slice<N>(value, start, count)` | Exactly the requested logical slice, requiring bounds and count <= N |
| `bytes_concat<N>(left, right)` | Ordered concatenation, requiring combined length <= N |
| `bytes_resize<N>(value)` | Change static capacity; preserve every logical byte or reject |
| `encode<T>(value)` | Canonical encoding, type `bytes<wire_width(T)>` |
| `can_decode<T>(buffer)` | Total canonical-validity test |
| `decode<T>(buffer)` | Typed value or `ERR_DECODE` trap |

Operands evaluate left to right exactly once before an operation's checks.
Bounds failures are `ERR_BYTES_BOUNDS`; insufficient destination capacity is
`ERR_BYTES_CAPACITY`. Resizing never truncates. Slicing allows an empty slice
at logical length. Full widened aggregate indices are checked, not truncated.
Scalar values are checked/narrowed before encoding, retaining normal overflow
behavior.

Capacity conversions are explicit. A source value `bytes<2>` does not implicitly
become `bytes<4>` even if its contents fit; use `bytes_resize<4>`. Equality compares
logical contents of matching static types, not unused storage.

Guard optional decoding with `can_decode` and lazy `select` or a guard. An eager
boolean `and` does not protect a partial decode. `can_decode` never asserts that
malformed data was valid merely to simplify a proof.

## Canonical wire contract

A bounded byte value encodes as little-endian `u64` logical length, logical bytes,
and zero padding through capacity N. Decode rejects excessive lengths, nonzero
unused padding, truncation and trailing bytes. Scalar and structured encodings
remain the same established canonical formats: record fields in declaration
order; union u32 tag plus active payload and zero inactive padding.

`encode<T>` produces exactly that format. Its logical output length is the
schema's fixed wire width. Therefore actually encoding a huge-capacity byte
field requires proportional output storage even when its logical input is
empty; this is an explicit wire-format cost, not a hidden verification budget.
Existing scalar-only persisted vectors remain unchanged.

The actual definitions establish both round-trip directions:

- `decode<T>(encode<T>(x)) == x` for valid typed values.
- `encode<T>(decode<T>(b)) == b` for canonically accepted bytes of the matching
  wire-width type. A larger-capacity input buffer needs an explicit static resize
  after its exact accepted logical length has been established.

Tests prove representative scalar, enum, byte, record and union laws from codec
semantics. No trusted round-trip axiom replaces the implementation.

## Shared proof and generated representation

SMT uses native sequences of 8-bit values with a length bound. Index, slice and
concatenation have exact sequence semantics. Schema helpers are emitted once per
type and compose through records/unions rather than enumerating byte contents
or combinations of variants.

Decoder helpers are emitted independently of encoders. Padding validation uses
an exact all-zero sequence-language predicate, not a capacity-sized zero value.
A reproduced billion-byte-capacity decoder-only probe initially forced about
8.8 GB of unnecessary solver model materialization. After this fix, the same
8/1024/1,000,000,000-capacity cases complete in about 0.05 seconds total, with
no timeout or capacity cutoff. Nested record/union decoder-only regressions also
ensure no unused encoder/zero objects are emitted. Encoders use an exact relational
zero-padding definition: the requested length and membership in the all-zero
sequence language uniquely determine every padding byte. Constraints compose
through all record fields and the active union payload, including inactive wire
padding. No round-trip law is assumed. Constructor-boundary facts are consequences
of the real sequence operations, covered by independent primitive tests. The
4/8/16/32-capacity byte and packet round-trip regressions complete in about one
second total on the development host; solver timing is not a complexity guarantee.
Actual runtime output retains its proportional storage cost.

Generated Rust uses a reusable `VeraBytes<N>` physical length/storage wrapper;
GPU targets use bounded storage and typed helpers. Copy/feed/codec operations use
loops rather than one emitted statement per capacity byte. GPU capacity growth
0/64/1024/8192 keeps helper and Naga-expression counts constant. Capacity/source
schema sharing and actual cold EGL tests are separate from SMT proof timings.

Typed native byte-sequence model inspection and concrete runtime replay still
admit every reported SAT witness. Unknown/error remains unverified. All proof
controls remain explicitly opt-in and unset by default.

## Interfaces and limits

JSON uses logical hex bytes; projections and repair evidence report canonical
logical values. Shared nominal schemas include byte capacity in their identity.
Native/Rust/GPU canonical hashes agree for unique byte keys. Host adapters and
fuzz sample coverage are described in [byte host support](BYTES_HOST.md).

Actual u64/usize layout arithmetic, allocation failure, GPU u32 buffer indexing,
device storage and target compiler limits still apply. No large sentinel default
stands in for a validation cap. UTF-8/string semantics, alternate compact wire
formats and stateful component ownership/frame contracts remain separate work.

See `examples/bytes.vera` for reusable typed codec composition.
