# VERA — Verified Relational Architecture

[![Tests](https://github.com/lhemerly/vera/actions/workflows/tests.yml/badge.svg)](https://github.com/lhemerly/vera/actions/workflows/tests.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

VERA is a programming language for transactional systems with rules that must
hold after every committed change. Define finite tables, write transitions, and
state your invariants. The verifier checks that those transitions preserve the
rules for every valid input and pre-state within the declared table capacities.

Use it for the stateful core of a ledger, inventory system, registry, order book,
or access-control service. The host application handles I/O; VERA describes the
state and the changes it permits.

## Why VERA?

- **Rules live with the code.** Express balance conservation, ownership,
  uniqueness, limits, and other properties as invariants and postconditions.
- **Changes are atomic.** A rejected transaction preserves rows, indexes,
  aggregates, and emitted effects together.
- **Arithmetic is checked.** Integers and fixed-point decimals reject overflow
  and invalid operations. Aggregate sums use wider accumulators.
- **Execution is bounded.** Tables have declared capacities and iteration uses
  bounded folds. These limits make the state machine amenable to verification.
- **Programs become embeddable engines.** Generate standalone Rust that uses
  only the standard library, with typed intents, snapshots, and durable replay.

## A small program

This counter can never exceed 1000. An increment must address a live row and
fit within the remaining room; otherwise the transaction is rejected.

```vera
module Counters;

table Counter capacity 4 {
  key id : id64<Counter>;
  col value : u64 [tracked_sum];
}

invariant bounded {
  forall k in Counter : le(Counter[k].value, 1000);
}

effect Increased(counter : id64<Counter>, amount : u64);

transition increase(counter : id64<Counter>, delta : u64)
  reads [Counter[counter].value]
  writes [Counter[counter].value]
  require live : table_exists(Counter, counter);
  ensure total : eq(sum(Counter.value), add(old_sum(Counter.value), delta));
  body {
    let %before : u64 = table_get(Counter, counter, value);
    guard(le(delta, sub(1000, %before)), ERR_LIMIT);
    let %after : u64 = add(%before, delta);
    table_set(Counter, counter, value, %after);
    emit Increased(counter, delta);
    commit;
  }
```

`id64<Counter>` is a typed row handle. `require` states a precondition; `guard`
rejects a transaction with an application error. `%` bindings hold intermediate
values. `ensure` describes a postcondition, and `old_sum` refers to the sum at
transaction start. The effect enters an outbox only when `commit` succeeds.

See the complete [counter](examples/counter.vera) and [bank](examples/bank.vera)
examples, including allocation, transfers, foreign-key rules, and projections.
The [language reference](docs/LANGUAGE.md) explains the syntax and semantics.

## Get started

Building requires Rust 1.89 or newer, a C++ linker, and Z3 4.13.3 or newer.
On Linux x86-64 with glibc 2.39 or newer, the following setup installs a pinned
Z3 library using `curl` and `unzip`:

```bash
git clone https://github.com/lhemerly/vera.git
cd vera
sh tools/setup_z3.sh
. ./.deps/z3/env.sh
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"

vera check examples/counter.vera
vera verify examples/counter.vera
vera run examples/counter.vera --transition increase \
  --param counter=4 --param delta=3 --setup examples/counter_setup.json
```

The setup contains a live counter with value 7; the transition commits value 10
and emits an `Updated` effect. `check` checks syntax, types, footprints, and bounds.
`verify` checks the proof obligations. `run` verifies the selected transition
before executing it.

On other hosts, install a compatible native Z3 library and build with Cargo.

Generate a Rust library for an application:

```bash
vera codegen rust examples/counter.vera --no-main --out counter.rs
rustc --edition 2021 --crate-type lib counter.rs
```

## Language features

| Feature | What it provides |
|---|---|
| Tables | Fixed capacity, typed generation-checked handles, unique secondary keys |
| Values | Booleans, fixed-width integers, exact fixed-point decimals, symbols, bytes32, enums |
| Specifications | Row/global invariants, preconditions, postconditions, old-state values |
| Transactions | Checked reads and writes, allocation, guards, atomic commit, typed effects |
| Folds | Inclusive bounded iteration with scoped accumulator and index bindings |
| Projections | Authorized state slices and available intents with parameter hints |
| Persistence | Typed intent logs, checksummed snapshots, deterministic replay |

`select` evaluates only its chosen branch. Boolean `and`, `or`, and `xor` evaluate
both operands. Decimal literals must be exactly representable in their type.

## Verification

For each transition, VERA uses Z3 to check that:

1. Every committed execution preserves the invariants and postconditions.
2. Reachable evaluated operations cannot trap on arithmetic or missing rows.
3. At least one valid state and input can commit, so the implementation is not
   vacuous.

Guards may reject inputs. Proofs assume a well-formed initial state satisfying
the declared invariants; setup and snapshot loading validate that state.
A timeout, resource limit, or unknown solver result rejects verification.

## Explore a program

```bash
# Read an authorized state view and available intents.
vera project examples/counter.vera --projection CounterView \
  --param counter=4 --setup examples/counter_setup.json

# Search bounded native execution traces for invariant violations or traps.
vera fuzz examples/counter.vera --steps 64 --threads 256

# Emit a compute furnace for GPU-based bug searches.
vera codegen wgsl examples/counter.vera --out counter.wgsl
```

GLSL and CUDA furnaces are also available. Fuzzing searches for bugs; it does not
replace verification. Pass `--json` for machine-readable command output and use
`vera --help` for command options. `--unverified` explicitly bypasses proof for
development when running or generating intentionally broken programs.

VERA is released under the [MIT License](LICENSE).
