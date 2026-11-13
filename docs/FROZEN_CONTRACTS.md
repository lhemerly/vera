# Frozen contracts

`vera_compiler::contract` provides parser-backed snapshots for a caller that
separates specification authorship from implementation repair. It does not
replace verification and does not decide which specification the user intended.

## API and schema

- `eligible_bodies(source)` enumerates a safe default editing policy. It includes
  transition and component-operation bodies, and function bodies that are not
  referenced, even transitively, by protected specification syntax. Enumeration
  is not authorization: a trusted caller must approve and retain the policy.
- `freeze_source(source, &editable_bodies)` parses and semantically checks the
  source, then returns a schema-version-1 `FrozenContract`.
- `compare_source(&frozen, candidate_source)` validates the snapshot, parses and
  semantically checks the candidate, then returns `matches` and structured
  differences. Invalid source or snapshot data is an error, not a match.

Body targets are tagged JSON objects:

```json
{"kind":"transition","name":"transfer"}
```

Functions use `{"kind":"function","name":"calculate"}`. Component operations
use `{"kind":"operation","component":"Account","name":"credit"}`.

A snapshot contains `schema_version`, the original `source`, its sorted
`editable_bodies`, and `canonical`. The canonical value is the compiler's
span-free AST with only explicitly authorized body fields removed. Source
retention permits independent re-parsing and semantic validation when reading
stored JSON. Formatting and comments are not contract changes. Declaration order,
names and all other AST fields remain protected conservatively.

The snapshot protects types, capacities, tables and flags, component ownership,
effect signatures, transition/operation/function signatures, footprints,
preconditions, postconditions, invariants, projection access rules and all bodies
not explicitly selected. Adding/removing declarations or changing helper
signatures requires a new trusted specification approval. A function referenced
by protected syntax cannot have an editable body, because changing such a helper
could indirectly redefine a frozen property.

A comparison has `schema_version`, `matches` and `differences`. Every difference
contains an RFC 6901 JSON-pointer `path` plus `expected` and `actual` canonical
values. A missing value is represented by an omitted `expected` or `actual` key,
distinguishing absence from an explicit AST null. This is syntactic identity, not a claim that
unmatched programs are semantically inequivalent.

## CLI

```sh
vera contract approved.vera --json > approved.contract.json
vera contract candidate.vera --frozen approved.contract.json
```

The first command uses `eligible_bodies` as the default policy. To restrict edits
further, pass `--editable targets.json` with an explicit JSON array (including
`[]` to permit no body edits). Comparison rejects `--editable`: a candidate
cannot override the frozen policy. A mismatch exits unsuccessfully and emits
structured differences; malformed inputs fail closed. Keep snapshot output in
controller-owned storage, not a path the candidate can replace.

## Enforcement boundary

The trusted controller must store the entire snapshot and editing policy outside
the repair model's write authority. Internal consistency checks reject malformed
versions, invalid programs, duplicate/unknown editing targets and canonical data
that does not match the retained source. They **cannot authenticate a wholly
replaced snapshot**. Hashing and comparison are useful only when the approved hash
or snapshot is itself protected.

Compare before verification or code generation, and verify the exact same
candidate bytes that passed comparison. Do not re-read a mutable path between
checks without retaining/checking its content identity. A matching contract says
nothing about a body's correctness: selected bodies can be syntactically valid
and violate every postcondition. Require independent verification and relevant
host/integration tests before execution. This mechanism does not freeze the host
application, external schemas, tool configuration, verifier binary or adapters;
those remain separate controller responsibilities.
