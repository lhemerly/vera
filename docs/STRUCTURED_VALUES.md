# Nominal records and tagged unions

Records and tagged unions are shared value types, not effect-only encodings or
integers packed into a GPU scalar. They work in parameters, locals, pure function
signatures, effects, table columns, projections, JSON, intents and snapshots.

```vera
record Transfer {
  destination : u64;
  amount : u64;
}
record Empty {}
union Message {
  Send : Transfer;
  Cancel : u64;
  None : Empty;
}
```

Names are nominal: two separately named records with identical fields are still
different types. Definitions may refer forward to other finite types. By-value
type cycles are rejected because they have no finite storage representation.
There is no fixed field, variant, type-count or nesting policy ceiling. Empty
records are unit values; a union needs at least one variant to be inhabited.

## Construction and observation

| Expression | Meaning |
| --- | --- |
| `record<Transfer>(destination:7, amount:10)` | Construct every declared field exactly once |
| `field(transfer, amount)` | Read a statically checked field |
| `variant<Message,Send>(transfer)` | Construct the named union variant |
| `is_variant<Message,Send>(message)` | Total variant test |
| `payload<Message,Send>(message)` | Read that variant's typed payload, or trap |

Record field expressions evaluate once in written source order, even when fields
are written in a different order from the declaration. Values and codecs store
fields in declaration order. Missing/duplicate/unknown fields, unknown tags and
wrong nominal payload types are frontend errors. No implicit structural or
numeric conversion is added.

Equality and inequality compare complete nominal values structurally. Arithmetic,
ordered comparisons and tracked sums remain numeric operations on scalar leaves.
Extract a numeric field before using arithmetic. Record fields and union payloads
retain their exact scalar types, enum domains, decimal scales and handle table
identities; widened aggregate comparisons do not alter their physical scalar
representation.

A payload read for a different active variant traps with `ERR_VARIANT_ACCESS`.
The verifier must prove that a reachable read selects the right tag. Protect an
optional read using lazy `select` or a guard:

```vera
select(is_variant<Message,Send>(message),
       field(payload<Message,Send>(message), amount),
       0)
```

An eager `and` is not a safe-access guard. This matches the existing distinction
between total type tests and partial payload access in effect contracts.

## Compositional proof representation

One resolved type definition is retained per nominal type. Runtime values carry
that nominal identity and recursively typed fields or one active payload. SMT
uses one native datatype per record/union, with shared validity/default-value
functions. Scalar-leaf constraints remain explicit. Field selectors and variant
recognizers are typed operations; a partial payload selector adds a recognizer
obligation rather than a fabricated default payload.

Function UFs, table values, unique inverse maps and symbolic outbox payloads all
use those same datatype sorts. Caller summaries therefore carry structured
values without cloning helper bodies or enumerating surrounding variant
combinations. Typed native constructor ASTs, not parsed model text, reconstruct
concrete witnesses; ordinary runtime validation and replay still admit every
reported SAT result.

A regression uses a 300-variant union in eight record fields and verifies its
identity/effect contract without expanding the 300^8 possible combinations.
Another covers records larger than the old 256-bit scalar GPU carrier. Cached
physical/wire layouts and shared schema definitions avoid repeated type-DAG
expansion. These structural properties do not promise constant solver runtime
or unlimited physical resources.

## Native storage and canonical wire format

Generated Rust uses named structs/enums. GPU targets use named product/sum
representations and typed result helpers; the existing scalar carrier remains
only a scalar-leaf representation. Native padding/layout and wire encoding are
separate contracts.

Canonical value encoding is:

- Scalars: unchanged historical little-endian representation.
- Records: canonical field encodings in declaration order, without native padding.
- Unions: a little-endian `u32` declaration-order tag, then the active payload,
  then zero padding to the largest declared variant's canonical payload width.

Decode rejects invalid tags, invalid active scalar domains, truncation, trailing
bytes and nonzero inactive padding. Empty records encode to zero bytes. The
canonical encoder/decoder APIs are `encode_value` and `decode_value`; these are
host APIs for declared value types, not yet source-language byte manipulation
or codec expressions.

Structured `[unique]` keys use canonical bytes, FNV-1a64 and the existing final
mix step in every runtime target. Scalar hashes remain unchanged. Exact nominal
equality is still the uniqueness rule; hash collisions never redefine equality.
The SMT inverse relation models logical uniqueness, while physical bounded-index
saturation remains a handled runtime rejection as before.

Schema identities use named references and each reachable definition once,
independently of incidental resolved IDs. Unrelated definitions do not silently
change a state schema. Programs without named types retain their historical
scalar schema, intent, snapshot and WAL vectors.

## JSON and repair feedback

A record uses a JSON object with exactly its named fields. A union uses exactly
`tag` and `payload`, for example:

```json
{"tag":"Send","payload":{"destination":7,"amount":10}}
```

Typed conversion rejects wrong shapes/domains/tags. Projections and emitted
effect evidence resolve source names and nested values. Failed variant access
has its own runtime error and source span, distinct from an ordinary false law.
Snapshots/intent logs retain their existing checksums and identity gates, now
with canonical structured payload validation.

See `examples/structured.vera` for a complete verified example. Variable-length
bounded bytes/strings, source-language codecs, higher-order/recursive functions
and stateful component ownership/frame contracts remain separate future work.
