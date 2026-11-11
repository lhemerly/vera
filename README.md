# VERA — Verified Relational Architecture

[![Tests](https://img.shields.io/badge/tests-37%20passing-brightgreen)](https://github.com/lhemerly/vera/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Python](https://img.shields.io/badge/Python-%3E%3D3.11-blue.svg)](https://www.python.org)
[![Rust](https://img.shields.io/badge/Rust-production%20runtime-orange.svg)](rust/vera-rt)

> **A programming language where the compiler proves your state machine correct
> before it ever runs.**

VERA is a language for writing transactional state machines: ledger systems,
order books, governance protocols, access control, auctions — any program whose
correctness depends on invariants over data that must hold across every possible
sequence of updates.

You write *what must stay true*. VERA proves it *does*.

---

## Why VERA?

Most software bugs are invariant violations: a balance goes negative, an account
is frozen twice, a user can withdraw funds they never deposited. The usual
defences — unit tests, code review, runtime checks — are all *sampling*. They
check a handful of the paths and hope the rest behave.

VERA replaces sampling with **proof**:

* **You declare invariants once**, as ordinary boolean expressions over your
  tables (`forall k in Account : ge(Account[k].balance, 0)`).
* **The compiler proves every transition preserves them** using an SMT solver.
  Not tests. A mathematical proof, for every input, forever.
* **If a transition is wrong, the compiler hands you the exact input that
  breaks it**, together with the pre-state and a step-by-step execution trace.

Because the language is deliberately small — no loops, no pointers, no
undefined behaviour, no hidden I/O — the proof is always reachable. The
compiler rejects code it cannot prove, rather than shipping it.

---

## Features

| | |
|---|---|
| **Formal verification** | Three machine-checked proof obligations per transition: safety, panic-freedom, and non-vacuity. |
| **Three targets from one source** | SMT-LIB proofs, GPU fuzzing kernels, and zero-allocation SoA Rust. |
| **Deterministic by construction** | No clocks, no entropy, no hidden I/O. Same input, same state, everywhere. |
| **Total termination** | All iteration is bounded. The halting problem is trivially decidable. |
| **Hardware-scale fuzzing** | The same program compiles to a GPU compute kernel that explores millions of trajectories. |
| **LLM-friendly IR** | Prefix-canonical operators, no precedence, no branching — designed for grammar-constrained code generation. |
| **Production runtime** | Lock-free conflict analysis, io_uring write-ahead log, copy-on-write snapshots, deterministic replay. |

---

## Quick start

VERA's compiler is a Python package; the generated runtime is Rust.

```bash
# 1. Create an environment and install the toolchain
python3 -m venv .venv
.venv/bin/pip install z3-solver pytest

# 2. Check the example
.venv/bin/python -m vera check  examples/bank.vera

# 3. Prove it
.venv/bin/python -m vera verify examples/bank.vera

# 4. Run it
.venv/bin/python -m vera run examples/bank.vera --transition transfer \
        --param from=32 --param to=33 --param amount=100000000 \
        --setup examples/bank_setup.json
```

Then generate a production Rust engine:

```bash
.venv/bin/python -m vera codegen rust examples/bank.vera --out /tmp/bank.rs
rustc --edition 2021 -O /tmp/bank.rs -o /tmp/bank && /tmp/bank
```

Or blast it on the GPU (OpenGL 4.6, headless):

```bash
.venv/bin/python -m vera codegen glsl examples/bank.vera > /tmp/bank.comp
gcc tools/furnace_host.c -o /tmp/host -lEGL -lGL && /tmp/host /tmp/bank.comp 64
```

---

## A tour of the language

### Tables are Structure-of-Arrays

A `table` declares a fixed capacity and fixed-width columns. Memory is laid out
as 64-byte-aligned column vectors, so a warp of GPU threads reads a whole row
with one coalesced transaction.

```vera
table Account capacity 32 {
  key id      : id64<Account>;      // (generation << 5) | slot
  col balance : dec64 [tracked_sum]; // fixed-point, scale 10^-8, summed in hardware
  col frozen  : bool;
}
```

`id64<Account>` is a strongly-typed handle: the low bits index a slot, the upper
bits are a generation counter that makes stale handles fail safely.

### Invariants are ordinary expressions

```vera
invariant non_negative_balance { forall k in Account : ge(Account[k].balance, 0); }
invariant balance_bounded       { forall k in Account : le(Account[k].balance, 1000000000); }
invariant order_owner_live      { forall k in Order : table_exists(Account, Order[k].owner); }
```

`forall` reads like mathematics, but the compiler does not trust it literally. It
instantiates the invariant over the **footprint** — the finite set of rows a
transition actually touches — turning an infinite universal quantifier into a
finite conjunction the solver can decide.

### Transitions are total functions

Every transition takes the state and an input intent, and returns a new state or
a typed revert. Nothing else. All iteration is a bounded `fold_bounded<N>`.

```vera
transition transfer(from : id64<Account>, to : id64<Account>, amount : dec64)
  reads  [Account[from].balance, Account[to].balance,
          Account[from].frozen, Account[to].frozen]
  writes [Account[from].balance, Account[to].balance]

  require positive_amount : gt(amount, 0);
  require from_exists     : table_exists(Account, from);
  require to_exists       : table_exists(Account, to);
  require not_frozen      : eq(Account[from].frozen, false);
  require no_bound_breach : le(amount, sub(1000000000, Account[to].balance));

  ensure conservation_of_funds : eq(sum(Account.balance), old_sum(Account.balance));

  body {
    let %bal_from : dec64 = table_get(Account, from, balance);
    let %bal_to   : dec64 = table_get(Account, to, balance);
    let %frozen_to : bool  = table_get(Account, to, frozen);

    guard(ge(%bal_from, amount), ERR_INSUFFICIENT);
    guard(ne(from, to), ERR_SELF_TRANSFER);
    guard(eq(%frozen_to, false), ERR_RECIPIENT_FROZEN);

    let %new_from : dec64 = sub(%bal_from, amount);
    let %new_to   : dec64 = add(%bal_to, amount);

    table_set(Account, from, balance, %new_from);
    table_set(Account, to,   balance, %new_to);
    commit;
  }
```

The syntax is intentionally spartan: every statement starts with one of six
keywords, every value has exactly one definition, and there are no implicit
conversions. That is what makes it possible for an LLM to emit valid VERA —
and for a proof to reason about it.

### A wrong program, rejected with a witness

Remove the `guard(ne(from, to), ...)` line and the compiler refuses to accept
the transition:

```
[VERIFICATION FAILED: Obligation 1 - Invariant or Postcondition Violation]
Transition:   transfer
Violated Law: ensure conservation_of_funds

MINIMAL COUNTEREXAMPLE WITNESS:
1. Pre-State (Sigma_0):
   - Account[id=25165912] = { id: 25165912, balance: 403960524, frozen: False }
2. Input Intent (iota):
   - from   = 25165912
   - to     = 25165912        <-- aliasing: from == to
   - amount = 67877164
3. SSA Execution Trace:
   - let %bal_from : dec64 = 403960524
   - let %bal_to   : dec64 = 403960524   <-- stale read before the first write
   - table_set(Account, from, balance, 336083360)
   - table_set(Account, to,   balance, 471837688)
4. Violated Post-Condition / Invariant:
   - ensure conservation_of_funds: expected true, got False

REPAIR DIRECTIVE:
   Add a disjointness guard `guard(ne(from, to), ERR_SELF_TRANSFER);`
```

This is the classic aliasing bug that sampling almost never finds, produced by
the solver in milliseconds. See [`examples/bank_buggy.vera`](examples/bank_buggy.vera).

---

## How VERA works

### The three proof obligations

For every transition, the compiler asks the solver three questions:

1. **Safety** — is there any valid pre-state and input that reaches a committed
   state violating a postcondition or a global invariant? Must be **impossible**.
2. **Panic freedom** — can any input cause an unhandled arithmetic overflow,
   division by zero, or a missing-row access? Must be **impossible**.
3. **Non-vacuity** — is there at least one input that commits? Must be
   **possible**. This defeats the vacuous "guard(false)" implementation that
   trivially satisfies the first two.

### One source, three targets

```
                       ┌── Target A ─ SMT-LIB 2.6 ─ Z3 ────────► machine-checked proof
VERA source ──┼── Target B ─ GLSL / CUDA / WGSL ─ GPU ───────► massive fuzzing
                       └── Target C ─ SoA Rust ──────────────► production runtime
```

* **Target A — proofs.** Footprint-localized quantifier elimination, slot-indexed
  functional state, and a frame-decomposed representation of summed aggregates
  that keeps proof cost linear in the number of *touched rows*, not table size.
* **Target B — fuzzing.** The same transition compiles to a compute kernel with
  a branchless SSA core and a warp-interleaved sandbox, running hundreds of
  millions of trajectories per second.
* **Target C — production.** `#[repr(C, align(64))]` SoA structs, checked
  arithmetic, commit-after-validate transitions, and a lock-free dispatcher.

### The production runtime

* **Lock-free dispatch.** The compiler extracts each transition's exact read and
  write footprint. The dispatcher partitions a batch of intents into
  conflict-free waves and runs them on pinned threads with no mutexes.
* **Durable persistence.** A canonical 64-byte `(IntentID, BinaryArgs,
  TxSequenceNumber)` record is appended to an io_uring write-ahead log and
  fsynced, paired with copy-on-write snapshots.
* **Deterministic replay.** Replaying the intent log reproduces the live state
  byte-for-byte.

```text
wal backend: io_uring
batch committed 4/4
balance[0] = 450000000  balance[1] = 350000000
replay matches live state: deterministic
```

---

## The example in this repository

| file | what it is |
|---|---|
| [`examples/bank.vera`](examples/bank.vera) | A verified banking core: accounts, transfers, freezing, and orders. |
| [`examples/bank_buggy.vera`](examples/bank_buggy.vera) | The same, minus the self-transfer guard. Rejected with a counterexample. |
| [`examples/bank_setup.json`](examples/bank_setup.json) | Seed state for running the example. |

Try it yourself:

```bash
.venv/bin/python -m vera verify examples/bank.vera        # the correct version: VERIFIED
.venv/bin/python -m vera verify examples/bank_buggy.vera  # the buggy version: REJECTED
```

---

## Performance

On a 6-core Linux host the verifier proves the banking core's three transitions
in about **1.5 seconds** (capacity 32/8).

Two encoding techniques keep proofs fast as tables grow:

* **O(1) per-access writes.** The last-write map and an occupancy array with
  symbolic-slot stores replace per-slot ite-chains; a known-slot fast path
  resolves `table_get` on an allocation result without symbolic key extraction.
* **Frame-decomposed sums.** `tracked_sum` aggregates are split into an exact
  sum over the touched footprint and a bounded symbolic rest frame:

  ```
  σ₀ = σ_local,₀ + σ_rest,     n_rest·L_c ≤ σ_rest ≤ n_rest·U_c
  ```

  where `[L_c, U_c]` comes from the invariants themselves. Proof cost is
  O(|footprint|), not O(capacity):

  | capacity | frame (panic) | exact O(C) sum |
  |---|---|---|
  | 256 | **48 ms** | >30 s (timeout) |
  | 1024 | 130 ms | — |
  | 4096 | 439 ms | — |

The GPU furnace runs 2048 threads through 64-step trajectories and finds
invariant violations in a deliberately broken variant:

| spec | threads | violations | committed |
|---|---|---|---|
| `bank.vera` | 2048 | 0 | 48144 |
| bound-breaking variant | 2048 | **426** | 38910 |

---

## Project layout

```
vera/                 compiler and verifier (Python)
  lexer.py  parser.py     the language front end
  types.py                fixed-width scalars and the SoA layout
  semantic.py             type checking, access footprints, WCET bounds
  smt_emit.py             AST → SMT-LIB 2.6, footprint localization, frame sums
  verifier.py             counterexample lifting and repair directives
  runtime.py              reference SoA engine with an undo log
  codegen_rust.py         Target C: SoA Rust + dispatcher + WAL
  codegen_glsl.py         Target B: OpenGL 4.6 compute furnace
  codegen_gpu.py          Target B: CUDA / WGSL emission
  fuzz.py                 reference trajectory furnace
rust/vera-rt/         production runtime crate (io_uring WAL, snapshots, replay)
tools/                EGL host for the GLSL furnace, cargo build helper
examples/             the banking core, correct and buggy
tests/                37 tests across the compiler, runtime, GPU and WAL
```

---

## Status

VERA is a working implementation of the v1.1 language specification: a complete
front end, a three-obligation verifier, a GPU fuzzing target, and a production
Rust runtime.

The language is intentionally narrow today. If your problem needs unbounded
collections, dynamic dispatch, or raw pointers, VERA is not the right tool —
that narrowness is exactly what makes the proofs possible.

**Not yet implemented:** the MVMemory Tier-2 Block-STM fallback for
data-dependent (Tier-2) access footprints; Robin Hood probing for secondary
unique-key indexes (bounded linear probing is used instead).

---

## Tests

```bash
.venv/bin/python -m pytest tests -q
```

37 tests cover the parser and type checker, access footprints, WCET bounds,
capacity and fold limits, all three proof obligations, counterexample lifting,
runtime atomicity, projections, the fuzzer, Rust codegen compilation and
execution, secondary indexes, fold/alloc scaling, frame-decomposed sums
(including two dependent columns and a joint bound), the GPU backend
end-to-end, the Tier-1 dispatcher, and the io_uring WAL.

---

## Contributing

Contributions are welcome — especially new example programs, additional proof
obligation shapes, and GPU fuzzing strategies.

1. Fork and clone.
2. Create a virtual environment and install `z3-solver` and `pytest`.
3. Make your change and add tests.
4. Run the full suite and open a pull request.

Please keep the language's core promise intact: **if a transition cannot be
proven, the compiler must refuse it.**

---

## License

VERA is released under the [MIT License](LICENSE).
