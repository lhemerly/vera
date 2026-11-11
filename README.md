# VERA — Verified Relational Architecture (v1.0 implementation)

A deterministic, transactional programming system with a dual representation:
humans author the **specification plane** (schema, invariants, projections,
transition contracts) and an LLM synthesizes the **synthesis plane** (canonical
SSA). Every transition is lowered simultaneously to

* **Target A** — SMT-LIB 2.6 (`QF_ABV`) verified with Z3 (three proof obligations),
* **Target B** — CUDA / WGSL fuzzing-furnace kernels,
* **Target C** — zero-allocation `#[repr(C, align(64))]` SoA Rust.

```
examples/bank.vera ──► vera check      parse + type/access/WCET analysis
                 ──► vera verify     safety / panic / liveness proofs + counterexamples
                 ──► vera run        reference runtime execution
                 ──► vera project    State & Affordance Frame
                 ──► vera fuzz       reference trajectory furnace
                 ──► vera codegen    rust | cuda | wgsl
```

## Quick start

```bash
python3 -m venv .venv && .venv/bin/pip install z3-solver pytest
.venv/bin/python -m pytest tests -q          # 22 tests

.venv/bin/python -m vera check   examples/bank.vera
.venv/bin/python -m vera verify  examples/bank.vera --timeout 20000
.venv/bin/python -m vera run     examples/bank.vera --transition transfer \
        --param from=32 --param to=33 --param amount=100000000 \
        --setup examples/bank_setup.json
.venv/bin/python -m vera project examples/bank.vera --projection AccountView \
        --param from=32 --setup examples/bank_setup.json
.venv/bin/python -m vera fuzz    examples/bank.vera
.venv/bin/python -m vera codegen rust examples/bank.vera --out /tmp/bank.rs
rustc --edition 2021 -O /tmp/bank.rs -o /tmp/bank && /tmp/bank
```

`examples/bank.vera` is a verified banking core; `examples/bank_buggy.vera`
removes the self-transfer guard and is rejected with the Section 10.2
counterexample packet.

## The example

```vera
module Bank;

table Account capacity 32 {
  key id     : id64<Account>;
  col balance : dec64 [tracked_sum];
  col frozen  : bool;
}

invariant non_negative_balance { forall k in Account : ge(Account[k].balance, 0); }
invariant balance_bounded       { forall k in Account : le(Account[k].balance, 1000000000); }

transition transfer(from : id64<Account>, to : id64<Account>, amount : dec64)
  reads [Account[from].balance, Account[to].balance, Account[from].frozen, Account[to].frozen]
  writes [Account[from].balance, Account[to].balance]
  require positive_amount  : gt(amount, 0);
  require from_exists      : table_exists(Account, from);
  require to_exists        : table_exists(Account, to);
  require not_frozen       : eq(Account[from].frozen, false);
  require no_bound_breach  : le(amount, sub(1000000000, Account[to].balance));
  ensure conservation_of_funds : eq(sum(Account.balance), old_sum(Account.balance));
  body {
    let %bal_from : dec64 = table_get(Account, from, balance);
    let %bal_to   : dec64 = table_get(Account, to, balance);
    let %frozen_to : bool = table_get(Account, to, frozen);
    guard(ge(%bal_from, amount), ERR_INSUFFICIENT);
    guard(ne(from, to), ERR_SELF_TRANSFER);
    guard(eq(%frozen_to, false), ERR_RECIPIENT_FROZEN);
    let %new_from : dec64 = sub(%bal_from, amount);
    let %new_to   : dec64 = add(%bal_to, amount);
    table_set(Account, from, balance, %new_from);
    table_set(Account, to, balance, %new_to);
    commit;
  }
```

## Implementation map

| Module | Responsibility |
| --- | --- |
| `vera/lexer.py`, `vera/parser.py` | Full EBNF grammar, prefix-canonical synthesis IR, infix sugar normalized in the human layer |
| `vera/types.py` | Fixed-width scalars, domains, SoA layout (64-byte-aligned columns), id64 generation |
| `vera/semantic.py` | Symbol tables, strict coercion-free typing, access-footprint extraction (Tier 1/2), WCET bounds, fold depth/product |
| `vera/smt_emit.py` | AST → SMT-LIB 2.6 (QF_ABV), three proof obligations, versioned state, footprint localization, frame-decomposed tracked sums |
| `vera/verifier.py` | Model projection, runtime replay, counterexample lifting, diagnostic packet |
| `vera/runtime.py` | Reference SoA engine: undo log, checked arithmetic, tracked sums, atomic revert |
| `vera/projection.py` | State & Affordance Frame with parameter interval bounds |
| `vera/codegen_rust.py` | Target C: `#[repr(C, align(64))]` SoA structs, checked helpers, commit-after-validate, intent enum, Tier-1 dispatcher, WAL, snapshot |
| `vera/codegen_glsl.py` | Target B: OpenGL 4.6 GLSL compute furnace (warp-interleaved sandbox, xoshiro256**, branchless guards) |
| `vera/codegen_gpu.py` | Target B: CUDA / WGSL emission (warp-interleaved, branchless, xoshiro256**) |
| `rust/vera-rt` | Production Rust crate: io_uring intent WAL, record encode/decode, replay |
| `tools/furnace_host.c` | EGL surfaceless host for the GLSL furnace |
| `tools/build_cargo.py` | Regenerates the Rust crate's `spec.rs` from a VERA source |
| `vera/fuzz.py` | Reference trajectory furnace (delta shrinking, invariant + postcondition checks) |
| `vera/cli.py` | `check`, `verify`, `run`, `project`, `fuzz`, `codegen` |

## The three proof obligations

For a transition `f` with preconditions `Pre`, postconditions `Post`, global
invariants `I`, and path predicate `π` (the conjunction of every guard and
`¬trap`), the encoder emits:

1. **Safety / partial correctness** — `I(Σ₀) ∧ Pre ∧ π ∧ ¬(Post ∧ I(Σ'))` must be
   **UNSAT**.
2. **Absence of unhandled traps** — `I(Σ₀) ∧ Pre ∧ ⋁ (π_{s-1} ∧ trap_s)` must be
   **UNSAT**. Traps are arithmetic overflow/underflow, division by zero,
   missing-row accesses, and tracked-sum accumulator overflow. Allocation
   saturation and generation exhaustion are *handled* deterministic reverts and
   are therefore part of `π`, not the panic disjunction.
3. **Non-vacuity / liveness** — `I(Σ₀) ∧ Pre ∧ π` must be **SAT**; otherwise the
   compiler rejects the body with `ERR_VACUOUS_IMPLEMENTATION`.

`∀` invariants are eliminated by footprint localization: the inductive
pre-invariant is instantiated at the relevant slots, and the post-invariant is
checked on the modified slots plus a fresh witness (the Section 6.1 theorem).
All-slot instantiation is used when exact column sums are requested.

## Verifier modes

* **Default (dual mode).** Obligations 1 and 3 use the *bounded free accumulator*
  abstraction: the pre-state tracked-sum register is a free scalar constrained
  to the column domain, which is exactly the runtime guarantee (the compiler
  reverts on accumulator overflow). Obligation 2 uses the *exact* O(C) column
  sum, because the intermediate accumulator range check genuinely depends on the
  whole table.
* **`--exact-sum`.** All three obligations use the exact column sum, tying the
  accumulator register to the table contents. Stronger, but solver time grows
  with capacity.

Both modes emit a complete SMT-LIB 2.6 script (`--emit-smt <prefix>`).

## Counterexample lifting

When Z3 returns SAT, the model is projected onto the pre-state rows reachable
from the input parameters, replayed through the reference runtime to obtain an
SSA execution trace, and lifted into the canonical packet:

```
[VERIFICATION FAILED: Obligation 1 - Invariant or Postcondition Violation]
Transition:   transfer
Violated Law: ensure conservation_of_funds

MINIMAL COUNTEREXAMPLE WITNESS:
1. Pre-State (Sigma_0):
   - Account[id=25165912] = { id: 25165912, balance: 403960524, frozen: False }
2. Input Intent (iota):
   - from = 25165912
   - to   = 25165912
   - amount = 67877164
3. SSA Execution Trace:
   - let %bal_from : dec64 = 403960524
   - let %bal_to   : dec64 = 403960524      <-- stale read before the first write
   ...
4. Violated Post-Condition / Invariant:
   - ensure conservation_of_funds: expected true, got False

REPAIR DIRECTIVE:
   The postcondition fails on a reachable input. Add a guard that excludes the
   counterexample input class, or re-read the mutated row after the first
   table_set so later reads observe the new value.
```

## Design decisions and refinements

The specification leaves a few corners undefined; the implementation resolves
them as follows and documents each choice.

1. **Slot-indexed functional state.** Columns are modelled as uninterpreted
   functions `T_col(slot)` rather than SMT arrays. The post-state is an
   ite-chain over the pre-state plus the bounded write set. This removes array
   store reasoning, the dominant cost at large capacities.
2. **id64 generation width.** The physical generation array is `[u32; C]`
   (Section 8.1), so the counter is `min(64 − k, 32)` bits and the remaining high
   bits of the handle must be zero. Validity is
   `occupied[slot] ∧ gen[slot] = (key ≫ k) ∧ (key ≫ (k + g)) = 0`.
3. **Missing-row semantics.** `table_get` on a non-live key reverts with
   `ERR_MISSING_ROW`. This is the only total, verifiable choice; unguarded
   missing-row accesses are reported by Obligation 2.
4. **Checked fixed-point.** `dec64`/`dec128` are scaled integers. `mul`/`div`
   require exact scaling (no truncation) and range-check the scaled result.
5. **Folds.** `fold_bounded<N>` executes at most `N` iterations of the inclusive
   range `[lo..hi]`; the compiler enforces `N ≤ 1024`, nesting depth `≤ 2`, and
   `N₁·N₂ ≤ 1024`. Fold-body `let` bindings are iteration-local.
6. **Effects.** `emit` is modelled by the runtime and the fuzzer (transactional
   outbox, discarded on revert). The grammar's invariant language does not
   reference the outbox, so the SMT encoding does not model it.
7. **Capacity and bitmap.** Capacities are powers of two; the occupancy bitmap is
   `ceil(C/64)` 64-bit words (the `C/64` formula is rounded up for `C < 64`).

## Performance

On a 6-core Linux host, the dual-mode verifier proves the banking core's three
transitions in ~1.5 s total (capacity 32/8).

The fold/allocation encoding was rewritten for O(1) per-access cost: the
last-write map (`widx`/`wval` arrays), an occupancy array with symbolic-slot
stores, a known-slot fast path for `table_get` on an allocation result, and a
symbolic allocator slot with an O(C) lowest-free constraint. A `fold_bounded<8>`
with an allocation and a read per iteration verifies in ~0.8 s (previously
>60 s); `fold_bounded<16>` in ~15 s.

### Frame-decomposed tracked sums (Spec 6.2)

`tracked_sum` pre-aggregates use the frame decomposition
`σ₀ = σ_local + σ_rest`:

* `σ_local` is the exact deduplicated sum over the transition's touched
  footprint `K_rw` (typically ≤ 4 keys), with the aliasing guard
  `δ_i = occ₀(k_i) ∧ ∧_{j<i} k_i ≠ k_j`;
* `σ_rest` is a symbolic scalar for the untouched frame, bounded by the
  **invariant-derived convex hull** `n_rest·L_c ≤ σ_rest ≤ n_rest·U_c`, where
  `[L_c, U_c]` is extracted from the `forall` invariants on the column;
* the post-state identity is the telescoping equation
  `σ_final = σ_rest + σ_local(Σ_final)`.

This keeps Footprint Localization sound (the rest frame never reads the SMT
array) and makes the proof O(|K_rw|) in capacity instead of O(C):

| capacity | frame (panic) | exact O(C) sum |
|---|---|---|
| 256 | 48 ms | >30 s (timeout) |
| 1024 | 130 ms | — |
| 4096 | 439 ms | — |

Two dependent columns are handled with one shared `n_rest` per table and one
`σ_rest` per column; a joint invariant such as
`sum(A.assets) + sum(A.liabilities) ≤ cap` is bounded by the per-column hulls.
That joint query is correct but solver-hard (~20 s at capacity 2); the
per-column and single-column cases are fast.

## GPU furnace (OpenGL 4.6 / Target B)

`vera codegen glsl` emits a GLSL 4.6 compute shader implementing the Section 7
design on a warp-interleaved sandbox (lane `L`'s row `S` at `col[S*32+L]`):
xoshiro256** PRNG, branchless `select`/`guard`, a D-step trajectory per lane, and
global-invariant checks after every step. `tools/furnace_host.c` runs it
headless through EGL surfaceless (no window) and reads back the violation /
commit counters.

Verified on the local AMD Radeon RX 6600 XT (Mesa radeonsi, GL 4.6 core):

| spec | threads | violations | committed |
|---|---|---|---|
| `bank.vera` | 2048 | 0 | 48144 |
| bound-breaking variant | 2048 | **426** | 38910 |

## Rust production runtime (Target C)

The Python package is the compiler/verifier; the production runtime is Rust.

* `vera codegen rust` emits a zero-allocation `#[repr(C, align(64))]` SoA engine
  plus the runtime: a `Params` intent enum with `reads()`/`writes()` metadata,
  `execute()`, and `encode()`.
* `run_batch` performs conflict analysis (write-write, write-read, and
  read-write on the same table/column/slot) and executes each conflict-free
  wave in parallel with `std::thread::scope`; waves are serializable by
  construction.
* `rust/vera-rt` is the production crate. `IoUringWAL` appends canonical
  64-byte `(IntentID, BinaryArgs, TxSequenceNumber)` records with
  `IORING_OP_WRITE` and waits for the CQE (falling back to `pwrite` if the
  kernel rejects io_uring). `snapshot` writes copy-on-write packed state bytes,
  and `replay_file` + `Params::decode` reproduce the live state deterministically.

```
wal backend: io_uring
batch committed 4/4
balance[0] = 450000000  balance[1] = 350000000
replay matches live state: deterministic
```

## Limitations

* The CUDA/WGSL backends are emission-only; the OpenGL backend is the validated
  GPU target on this host.
* Secondary unique-key indexes are implemented as bounded linear probing
  (`P_max = 16`); the spec's Robin Hood variant is not used, and `table_set` on
  a unique column is forbidden (indexes are insert-only).
* The lock-free Tier-1 dispatcher and the io_uring WAL are implemented for the
  generated Rust engine; the MVMemory Tier-2 Block-STM fallback is not.
* The frame-decomposed sum is the default and is O(|K_rw|) in capacity. The
  exact O(C) sum remains as a fallback for wildcard / Tier-2 footprints, and a
  joint bound over two tracked_sum columns is correct but solver-hard.

## Tests

33 tests across five files:

* `tests/test_vera.py` — parsing, type errors, access footprints, WCET,
  capacity/fold limits, all three obligations (including a vacuous transition
  and a missing-row trap), counterexample lifting, runtime atomicity, projections,
  the fuzzer, Rust codegen compilation/execution, unique-key indexes, fold/alloc
  scaling, frame-decomposed tracked sums (scaling, two-column, joint bound), and
  the CLI.
* `tests/test_gpu_backend.py` — GLSL compilation, end-to-end furnace run on the
  GPU, and violation detection on a bound-breaking spec.
* `tests/test_rust_runtime.py` — Tier-1 batch dispatch, intent WAL, snapshot, and
  deterministic replay.
* `tests/test_io_uring_wal.py` — the io_uring-backed WAL crate end to end.
