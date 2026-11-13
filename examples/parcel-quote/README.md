# Parcel Quote: a Cargo-native hybrid application

This executable quotes individual parcels or a batch file. The Rust shell handles
command parsing, files, stdin, errors and JSON output. A VERA function controls
the returned fee. It is an illustrative policy: express costs 1500 cents;
standard costs 500 below a 5000-cent subtotal and is otherwise free. Supported
weight is 1–20000 grams. These are not real carrier rates.

```sh
# From the VERA repository, with a trusted built compiler available:
export VERA="$(pwd)/target/release/vera"
cargo test --manifest-path examples/parcel-quote/Cargo.toml
cargo run --manifest-path examples/parcel-quote/Cargo.toml -- quote 1000 false 4000
# {"shipping_cents":500}
printf '1000,false,4000\n1000,false,5000\n1000,true,5000\n' | \
  cargo run --manifest-path examples/parcel-quote/Cargo.toml -- batch -
# {"line":1,"shipping_cents":500}
# {"line":2,"shipping_cents":0}
# {"line":3,"shipping_cents":1500}
```

`batch FILE` accepts UTF-8 lines of `weight_grams,express,subtotal_cents` with no
header, quotes or escaped fields. `batch -` reads stdin. The simple numeric
format is intentional, not a general CSV parser. Rejected records exit 2 and
emit no partial quote output. Empty input produces no records. The host buffers
the batch in memory; use bounded input files for production or extend the shell
with explicitly tested streaming/error semantics.

`build.rs` uses `vera-build` to prove and generate the core in Cargo's `OUT_DIR`.
A changed policy triggers generation; an unproved policy fails the build. The
application has no runtime VERA/Z3 dependency. Generated APIs validate domains
and preconditions; their proof does not cover parsing, file I/O or whether the
business policy is appropriate.

Tests exercise distinct outputs, weight limits, malformed inputs and batch
behavior. The compiler's `cargo_workflow` integration test additionally compiles
two bad consumers (discarded result and swapped inputs) and confirms the same
observable-output test fails. No claim is made that these finite tests prove all
possible host behavior.

See [the hybrid workflow guide](../../docs/HYBRID_RUST.md) for the reusable
integration API, provenance and trust boundaries.
