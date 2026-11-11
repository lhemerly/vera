# VERA — Verified Relational Architecture

[![Tests](https://github.com/lhemerly/vera/actions/workflows/tests.yml/badge.svg)](https://github.com/lhemerly/vera/actions/workflows/tests.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

VERA is a language for finite transactional state machines: ledgers, order
books, registries, access control and governance. You declare invariants and
transitions; the verifier checks that every committed transition preserves
those invariants, that evaluated operations do not trap, and that a concrete
committing input exists.

The compiler rejects proof errors, unsupported lowering, solver timeouts and
unknown results. Its guarantee is relative to the declared invariants and a
valid initial state. The compiler's encodings and execution backends remain
part of the trusted implementation; cross-backend conformance tests are included.

## Quick start

Requires Python 3.11+, Z3, and Rust 1.89+ for generated Rust/persistence tools.

The compiler is being migrated to Rust. A Python-free native frontend is now
available for the complete v1.1 syntax:

```bash
cargo run --bin vera -- parse examples/bank.vera
cargo run --bin vera -- parse examples/counter.vera --json
cargo test --workspace --locked
cargo build -p vera-compiler --release --locked
```

The native command currently parses syntax; semantic checking, verification,
execution and code generation still use the established Python toolchain below.
It explicitly rejects those native commands until their ports are complete.
See the [Rust migration design and milestones](docs/RUST_MIGRATION.md).

```bash
git clone https://github.com/lhemerly/vera.git
cd vera
python3 -m venv .venv
.venv/bin/pip install -e '.[test]'

.venv/bin/vera check examples/bank.vera
.venv/bin/vera verify examples/bank.vera
.venv/bin/vera verify examples/bank_buggy.vera  # rejected: self-transfer aliasing

.venv/bin/vera run examples/bank.vera --transition transfer \
  --param from=32 --param to=33 --param amount=100000000 \
  --setup examples/bank_setup.json
```

`run` and `codegen` prove the selected program before executing/emitting it.
For deliberately broken programs used in development or fuzzing, explicitly
pass `--unverified`. `check` performs parsing and semantic analysis without proof.

Generate a standalone Rust engine or a library:

```bash
.venv/bin/vera codegen rust examples/bank.vera --out /tmp/bank.rs
rustc --edition 2021 -O /tmp/bank.rs -o /tmp/bank
(cd /tmp && ./bank)

.venv/bin/vera codegen rust examples/counter.vera --no-main --out /tmp/counter.rs
```

The generated demo recovers its previous WAL before appending another batch;
re-running it preserves deterministic replay. Application integrations can use
`Params::encode/decode`, atomic transition functions, `run_batch`, `snapshot`
and `restore` directly.

Demo seeds are illustrative and are validated before use. Programs requiring a
specific nonempty initial state must supply that state in their application.

## Language

```vera
table Counter capacity 4 {
  key id : id64<Counter>;
  col value : u64 [tracked_sum];
}
invariant bounded {
  forall k in Counter : le(Counter[k].value, 1000000000);
}
effect Updated(counter : id64<Counter>, before : u64, after : u64);

transition increase(counter : id64<Counter>, delta : u64)
  reads [Counter[counter].value] writes [Counter[counter].value]
  require exists : table_exists(Counter, counter);
  require room : le(delta, sub(1000000000, Counter[counter].value));
  ensure total : eq(sum(Counter.value), add(old_sum(Counter.value), delta));
  body {
    let %before : u64 = table_get(Counter, counter, value);
    let %after : u64 = add(%before, delta);
    table_set(Counter, counter, value, %after);
    emit Updated(counter, %before, %after);
    commit;
  }
```

The complete example is in [`examples/counter.vera`](examples/counter.vera).
See the [language reference](docs/LANGUAGE.md) for syntax, types, evaluation
order, error behavior, footprints, folds, effects, projections and persistence.

| Area | Behavior |
|---|---|
| Scalars | bool, u8/u16/u32/u64, i64, dec64, dec128, sym32, bytes32, typed id64 handles, inline enums |
| Arithmetic | Checked integer operations; exact fixed-point multiply/divide; widened 128/256-bit aggregates |
| Tables | Fixed capacity, generation-checked handles, lowest-slot allocation, immutable unique columns with bounded Robin Hood indexes |
| Transactions | Sequential requires, SSA bindings, guards, allocation, writes, typed effects, atomic rollback, immutable old/old_sum |
| Iteration | Inclusive bounded folds, two nesting levels, maximum bound/product 1024, scoped bindings and no index wrap |
| Specifications | Named global/row invariants, postconditions, safety, panic freedom and concrete non-vacuity |
| Boundary | Authorized projection slices, stale-handle checks and configured intent affordances |
| Execution | Reference interpreter, generated Rust, and shared exact-limb GLSL/WGSL/CUDA compute furnaces |
| Batches | Safe optimistic snapshots, ordered validation and re-execution for both input-derived and state-dependent footprints |
| Persistence | Versioned typed intents, program fingerprints, checksummed WAL/snapshots, replay, torn-tail recovery, optional io_uring |

`select` is lazy. Boolean `and`, `or` and `xor` evaluate both operands. Decimal
literals are exact and excess precision is rejected. Every failed transaction
preserves rows, generations, indexes, aggregates and effects.

A proof is inductive over valid initial states. Non-vacuity means at least one
concrete committing state/input exists; it is not a temporal fairness guarantee.
Projections expose data and hints, while execution still validates the intent.

## Compute furnaces

GPU targets share one lowering and exact 32-bit-limb arithmetic, including
512-bit multiplication/division intermediates. They execute transactions,
validate invariants/postconditions, preserve atomic rollback and track committed
effects by count and value digest; GPU furnaces do not deliver external outbox
messages. They retain declared table capacities and use a deterministic xoshiro
PRNG. Furnaces are bounded bug searches, not substitutes for the SMT proofs.

```bash
.venv/bin/vera codegen glsl examples/bank.vera --out /tmp/bank.comp
glslangValidator /tmp/bank.comp
gcc -O2 tools/furnace_host.c -o /tmp/vera-host -lEGL -lGL
EGL_PLATFORM=surfaceless /tmp/vera-host /tmp/bank.comp 8

.venv/bin/vera codegen wgsl examples/bank.vera --out /tmp/bank.wgsl
.venv/bin/vera codegen cuda examples/bank.vera --out /tmp/bank.cu
```

Tests compile GLSL, validate and dispatch WGSL with `wgpu`, and execute the CUDA
lowering's portable C++ host mode against mathematical results. The host mode
checks shared kernel semantics without requiring an NVIDIA device; native CUDA
compilation/execution additionally requires its own toolchain/device.

## Rust runtime and durable replay

The standalone generated engine embeds the canonical WAL implementation. The
optional `rust/vera-rt` crate uses io_uring on supported Linux kernels, with the
same positional-write persistence protocol when io_uring is unavailable.

```bash
.venv/bin/python tools/build_cargo.py examples/bank.vera
cargo test --manifest-path rust/vera-rt/Cargo.toml
cargo run --manifest-path rust/vera-rt/Cargo.toml --bin vera-furnace
```

Typed intent payloads preserve arbitrary argument counts, complete bytes32
values and full-width dec128 values. A canonical program fingerprint rejects
replay under different transition semantics. WAL frames contain sequence and
integrity checks and are aligned to 64 bytes; banking intents use 128-byte
frames. `append` finishes the write and `sync` acknowledges durability. A torn
final frame is recovered on reopen; complete corruption or sequence errors are
rejected. One writer owns a log.

Snapshots serialize fields explicitly, validate the schema/invariants/derived
indexes and sums, and use checksums and atomic replacement. Legacy raw-memory
snapshots and fixed-record WAL files are incompatible with these formats.

The optimistic dispatcher uses safe owned snapshots and conservative validation
of the whole pre-state. Conflicting candidates are re-executed in input order.
This prioritizes deterministic correctness; it does not claim zero-allocation
transactions, fine-grained Block-STM performance, or lock-free shared mutation.

## Verification and tests

```bash
.venv/bin/python -m pytest tests -q -ra
cargo test --manifest-path rust/vera-rt/Cargo.toml --locked
```

The suite covers the reviewed soundness failures, exact scalar boundaries,
transaction rollback (including indexes/effects), nested folds, projections,
foreign keys, all backend emitters, generated runtime execution, canonical
argument codecs, program identity, corrupted records, torn writes and repeated
restart/replay. Differential arithmetic tests compare Python, generated Rust
and the shared GPU implementation against independently computed integer results.

CI runs the full test suite on Python 3.11–3.14 with mandatory backend checks.
Unavailable optional hardware/toolchains are reported explicitly by tests;
backend failures are not hidden by `continue-on-error`.

## Layout

```text
Cargo.toml            native compiler/runtime workspace
rust/vera-compiler/   native syntax frontend, diagnostics and vera CLI
vera/                 parser, type checker, verifier, reference engine, backends
vera/templates/       canonical embedded Rust persistence source
rust/vera-rt/         optional Linux io_uring runtime and replay demo
tools/                crate regeneration and headless GLSL host
examples/             bank, deliberately broken bank, counter and setup states
tests/                language, verifier, backend and persistence conformance
docs/LANGUAGE.md      language semantics and API reference
docs/IMPLEMENTATION.md implementation decisions and validation sequence
```

Contributions should include an executable behavioral regression and preserve
fail-closed verification. VERA is released under the [MIT License](LICENSE).
