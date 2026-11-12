# Proof budgets and scaling

Verification has **no default policy limits** on solver time, state cells, fold
unrolling, witness attempts, expression visits, SMT output, or effect candidates.
Every `VerifierConfig` budget defaults to `None`, serialized as JSON `null`.
Omitted CLI flags likewise leave the corresponding budget unset; no large sentinel
value substitutes for an unlimited setting.

The following diagnostic budgets are available only when a caller explicitly
selects them on `verify`, `run`, `replay`, or `codegen`:

| CLI option | Default | Unit / scope when explicitly selected |
| --- | --- | --- |
| `--timeout` | Unset | Milliseconds of solver/witness-search time per obligation |
| `--max-state-cells` | Unset | Complete finite pre-state cells |
| `--max-unroll` | Unset | Symbolic fold bound / nested product |
| `--witness-attempts` | Unset | Concrete witness candidates per obligation |
| `--max-expression-steps` | Unset | Symbolic expression visits, including repeated folds |
| `--max-smt-bytes` | Unset | Generated query bytes, including witness-blocking clauses |
| `--max-effect-events` | Unset | Symbolic candidates, including inactive unrolled emissions |

An explicitly selected limit fails closed when exhausted. It never reduces a
declared capacity, drops an invariant, accepts a partial proof, or substitutes
fuzzing for proof. Expression and output budgets are independent of state size.
Explicit zero count/byte budgets allow no counted work; the CLI requires an
explicit timeout to be positive. Without a timeout, no SMT timeout option is
emitted or applied to the native solver. Without an attempt budget, nonphysical
witnesses are blocked and search continues until a real witness, exhaustion of
models, a solver result, or an external interruption. Solver `unknown`, errors,
and invalid concrete replays are never accepted as a proof.

Unlimited policy does not imply unlimited physical resources: native memory,
address-space/index widths and the solver's supported numeric representations
still apply. An explicit timeout uses Z3's unsigned 32-bit millisecond interface;
explicit count/byte budgets use host `usize`. Declared finite table capacities,
scalar widths and source fold iteration bounds still define program semantics.
No model is silently shrunk to fit the host.
`State::empty`, including concrete witness materialization, likewise has no
implicit state-byte budget. `State::empty_with_limit` retains an explicit opt-in
storage-estimate check. Default construction checks dimensions and uses fallible
vector reservation, reporting `ERR_STATE_ALLOCATION` when reservation fails.
Operating-system overcommit and later native allocation failures can still
terminate a process; removing policy ceilings is not a guarantee that every
representable program fits available RAM.

When a byte budget is explicitly selected, the encoder accounts for retained
SMT declarations/assertions during construction, checks the final obligation
before constructing its query, and checks witness-blocking clauses before adding
them. That opt-in budget bounds the admitted encoding, not total process RSS: temporary expression strings, native solver
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
