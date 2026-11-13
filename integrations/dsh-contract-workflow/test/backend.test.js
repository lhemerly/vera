import test from "node:test";
import assert from "node:assert/strict";
import { compilerBackend } from "../backend.js";
import { ContractWorkflow } from "../workflow.js";
export const source =
  "module ContractFixture; effect Value(value:u64); transition emit_value(value:u64) reads [] writes [] ensure value_matches:eq(effect_arg(Value,0,value),value); body { emit Value(value); commit; }";
const binary = process.env.VERA_CONTRACT_BINARY;
const signal = new AbortController().signal;
test(
  "real compiler freeze comparison proof footprint and generation",
  { skip: !binary },
  async () => {
    const backend = await compilerBackend(binary);
    const frozen = await backend.freeze(source, signal);
    assert.equal(frozen.schema_version, 1);
    const matching = await backend.compare(frozen, source, signal);
    assert.equal(matching.ok, true, JSON.stringify(matching));
    const weakened = source.replace(
      "eq(effect_arg(Value,0,value),value)",
      "true",
    );
    assert.equal((await backend.compare(frozen, weakened, signal)).ok, false);
    assert.equal(
      (await backend.compare(frozen, "invalid syntax", signal)).ok,
      false,
    );
    const proof = await backend.verify(source, signal);
    assert.equal(proof.ok, true, JSON.stringify(proof));
    assert.ok(proof.result.results[0].specification_footprint);
    const failed = await backend.verify(
      source.replace("emit Value(value)", "emit Value(0)"),
      signal,
    );
    assert.equal(failed.ok, false);
    assert.ok(failed.result.results[0].counterexample);
    const generated = await backend.codegen(source, signal);
    assert.equal(generated.ok, true);
    assert.match(generated.code, /pub/);
    assert.equal(generated.host_api.schema_version, 1);
    assert.equal(generated.host_api.target, "rust");
    assert.deepEqual(generated.host_api.functions, []);
  },
);
test(
  "real compiler integrated workflow admission rejects spec drift and repairs with footprint",
  { skip: !binary },
  async () => {
    const backend = await compilerBackend(binary);
    let calls = 0;
    const w = new ContractWorkflow({
      backend,
      runRole: async () => {
        calls++;
        return calls === 1
          ? {
              source,
              requirements_map: "Emit exact input",
              assumptions: "finite u64",
              acceptance_tests: "Check actual emitted value",
            }
          : {
              kind: "implementation",
              source:
                calls === 2
                  ? source.replace(
                      "eq(effect_arg(Value,0,value),value)",
                      "true",
                    )
                  : source,
              explanation: "candidate",
              evidence_ids: [],
            };
      },
    });
    const j = await w.start({
      owner: "owner",
      parent: { id: "owner" },
      requirements: "Emit supplied u64 exactly",
      signal,
    });
    assert.equal(
      (
        await w.step({
          id: j.id,
          owner: "owner",
          parent: { id: "owner" },
          signal,
        })
      ).status,
      "not_verified",
    );
    const done = await w.step({
      id: j.id,
      owner: "owner",
      parent: { id: "owner" },
      signal,
    });
    assert.equal(done.status, "admitted");
    assert.equal(done.admitted.receipt.admission_scope, "verified_core_only");
    assert.equal(done.admitted.receipt.host_integration, "not_checked");
    assert.ok(
      done.admitted.evidence.proof.result.results[0].specification_footprint,
    );
  },
);

test("real compiler returns stable source-named pure-function host API", { skip: !binary }, async () => {
  const backend = await compilerBackend(binary);
  const generated = await backend.codegen("module Host; function identity(x:u64)->u64 ensure exact:eq(result,x); body { yield x; }", signal);
  assert.equal(generated.ok, true);
  assert.equal(generated.host_api.functions[0].name, "identity");
  assert.equal(generated.host_api.functions[0].rust_path, "functions::call_identity");
  assert.equal(generated.host_api.functions[0].host_domain_validation, true);
  assert.match(generated.code, /pub fn call_identity/);
});
