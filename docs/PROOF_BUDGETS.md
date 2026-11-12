# Proof budgets and scaling

Proof-resource exhaustion rejects verification. It never reduces a declared
capacity, drops an invariant, accepts a partial proof, or substitutes fuzzing for
proof. These API fields in `VerifierConfig` are also available on `verify`,
`run`, `replay`, and `codegen`:

| CLI option | Default | What it limits |
| --- | --- | --- |
| `--timeout` | 10000 ms | Solver and witness-search elapsed time per obligation |
| `--max-state-cells` | 4096 | Complete finite pre-state cells |
| `--max-unroll` | 1024 | Fold bound / nested product |
| `--witness-attempts` | 8 | Concrete witness candidates per obligation |
| `--max-expression-steps` | 4194304 | Symbolic expression visits, including unrolled specification folds |
| `--max-smt-bytes` | 67108864 | Generated query bytes, including witness-blocking clauses |
| `--max-effect-events` | 4096 | Symbolic effect candidates, including inactive unrolled emissions |

Expression and output limits are independent of state capacity. Raising a state
limit no longer silently increases the expression-expansion budget. A zero
expression/effect limit allows no such visits/events; a zero SMT limit rejects
all queries. The CLI requires a positive timeout.

The encoder accounts for retained SMT declarations/assertions while constructing
them, checks the final obligation before constructing its query, and checks
witness-blocking clauses before adding them. These limits bound the admitted
encoding, not total process RSS: temporary expression strings, native solver
memory, concrete witness state and source parsing have separate costs.

Row-local invariant instantiation uses the physical slot already known from
finite enumeration, rather than selecting that same slot again across every
row. Full handle generation and occupancy checks remain in place for existence
queries, and post-state reads still apply writes in order. Symbolic keys and
cross-row references retain the general exact encoding. Tracked sums use named
prefix terms instead of repeatedly copying a growing expression. Witness search
parses the base query once per obligation and adds candidate-blocking constraints
incrementally; only independently validated concrete replays count as SAT.

These changes avoid specific unnecessary expansion. They do not promise linear
solver runtime, make arbitrary cross-row specifications compositional, or remove
the inherent difficulty of nonlinear bit-vector arithmetic. The structural
regression in `tests/proof_budgets.rs` checks query growth rather than relying on
unstable wall-clock thresholds.

## Machine-readable observations

Every `VerificationResult` includes `config`, the verifier configuration used for
that attempt, including when no complete encoding could be built. Successful
encoding adds `encoding_metrics`: complete `state_cells`, actual symbolic
`expression_steps`, candidate `effect_events`, `retained_smt_bytes` for the base
declarations/assertions, and `max_query_bytes` for the largest exported
obligation query, including any witness-blocking clauses. These are observations,
not proof outcomes or estimates of total memory/time. Check `ok()` and the three
obligation statuses independently. Encoding failure leaves `encoding_metrics`
null rather than reporting partial work as a completed model.
