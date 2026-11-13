# A useful verified decision, end to end

This example is a small shipping-price policy. It is intentionally independent
of any benchmark. The application takes weight, service and subtotal, calls a
verified VERA function, and prints **that returned price** as JSON. It does not
replace the result with a constant or reduce it to `is_ok()`.

The illustrative policy charges 1500 cents for express, otherwise 500 cents
below a 5000-cent subtotal and zero at or above it. Weight must be 1–20000 grams.
These are example business rules, not real shipping rates.

With a built `vera` and `rustc` on PATH:

```sh
sh examples/shipping/build.sh /tmp/shipping-demo
/tmp/shipping-demo/shipping 1000 false 4000  # {"shipping_cents":500}
/tmp/shipping-demo/shipping 1000 false 5000  # {"shipping_cents":0}
/tmp/shipping-demo/shipping 1000 true 5000   # {"shipping_cents":1500}
/tmp/shipping-demo/shipping 0 false 4000    # policy rejection, exit 2
```

Use a fresh output directory. Generation verifies the policy before emitting
Rust and its host API manifest. The resulting executable needs neither the
compiler nor Z3 at runtime, and uses only the Rust standard library.

## What is and is not proved

VERA proves the function's fee rules for its declared input domain. The public
host wrapper validates input domains and checks the function's precondition.
Host argument parsing, JSON output and the business requirements themselves are
not formally verified. The integration test builds the real executable, checks
observable outputs and errors, and deliberately substitutes a constant for the
verified result to show those cases catch a disconnected core. Such tests cover
those cases; they are not a proof of arbitrary host code or a universal coverage
score.

The generated API is `verified::functions::call_shipping_fee`, not a private
numeric helper. Keep the generated file intact. Re-run proof-gated codegen after
changing the policy, and re-run executable tests after changing its caller.
