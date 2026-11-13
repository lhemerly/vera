# Specification reference footprint

`vera verify FILE --json` adds `specification_footprint` to each target result.
The Rust API exposes the same metadata. Existing proof status, witnesses, and
counterexamples retain their meaning. Unknown target names have a null footprint.

This is an **objective syntactic inventory, not a completeness or semantic coverage
score**. It does not claim that a specification captures the user's intent.
For example, `ensure same: eq(x,x);` mentions `x` without constraining it.

## Version 1

- `schema_version`: `1`.
- `interpretation`: `syntactic_references_only_not_semantic_coverage`.
- `universe_scope`: a pure function's inputs and return, or a transition/operation's
  inputs, program-declared tables/columns/effects, and guard error codes found in
  that target's body. Unrelated program declarations can therefore be unmentioned.
- `inventory`: stable IDs, kinds and source names. IDs use `input:NAME`, `result`,
  `table:NAME`, `state:TABLE.COLUMN`, `effect:NAME`, `effect_arg:EFFECT.PARAMETER`,
  `effect_count`, and `error:CODE`.
- `properties`: each `requires`, `ensures`, or applicable invariant, with its source
  span and reference IDs. Operation inventories include only their owner's
  invariants, matching their independently checked boundary. These lists describe
  references, not separate proof results for each property.
- `mentioned`: sorted distinct references across those properties. Function calls
  appear as `function:NAME`, with argument references, without expanding the callee.
- `unmentioned`: inventory IDs absent from the property reference lists.
- `implementation_references`: target-body references, including named operation
  calls and errors. These are not proof coverage or evidence that a path executes.
- `joint_safety_status`: the existing safety-obligation result. It is not a
  per-reference or per-property success classification.
- `committing_witness` and `counterexample`: `replayed` only when the corresponding
  existing result field holds an independently replayed witness. `result_field`
  identifies that field. Otherwise status is `not_established`. A committing
  witness establishes one committing transition/operation or returning function
  invocation, not reachability of every inventoried item. It can still violate
  ensures: inspect its `failed_ensures`.
- `unknowns`: explicit limits on interpretation, retained in the machine output.

Type domains, capacities, uniqueness rules, access frames and projection rules
are not counted in these property lists. Thus unmentioned does not mean
unconstrained.

State and structured values are inventoried at containing-column/input/payload
level. Snapshot phases, nested fields, byte elements, local data-flow propagation,
branch feasibility, property independence and semantic strength are not assessed.
Callee contracts/bodies are not recursively expanded; inspect their separate
results and the existing contract dependency evidence. Guard-code presence does
not establish expected error behavior. Host I/O and external requirements are
outside the checked-program universe.

The implementation traverses the finite IR without unrolling declared capacities
or introducing new SMT queries, hard validation limits, or aggregate percentages.
Use this inventory to find questions worth investigating, not as permission to
call an unmentioned item wrong or a mentioned item fully specified.
