import test from "node:test";
import assert from "node:assert/strict";
import { ContractWorkflow, digest } from "../workflow.js";
import { roleGuard } from "../index.js";
const signal = () => new AbortController().signal;
const spec = (source = "contract A|body placeholder") => ({
  source,
  requirements_map: "R1: conserve amount",
  assumptions: "finite capacity",
  acceptance_tests: "independent transfer test",
});
const impl = (source = "contract A|body valid") => ({
  kind: "implementation",
  source,
  explanation: "implemented",
  evidence_ids: [],
});
function fixture(outputs) {
  const calls = [],
    events = [];
  let n = 0;
  const backend = {
    identity: { sha256: "compiler-test" },
    async freeze(source) {
      return { canonical: source.split("|")[0], source };
    },
    async compare(frozen, source) {
      return { ok: frozen.canonical === source.split("|")[0] };
    },
    async verify(source) {
      return {
        ok: source.endsWith("valid"),
        result: {
          status: source.includes("unknown")
            ? "unknown"
            : source.endsWith("valid")
              ? "verified"
              : "counterexample",
        },
      };
    },
    async codegen() {
      return { ok: true, code: "generated artifact" };
    },
  };
  const runRole = async (p) => {
    calls.push(p);
    const result = outputs[n++];
    if (result instanceof Error) throw result;
    return typeof result === "function" ? result(p) : result;
  };
  return {
    machine: new ContractWorkflow({
      backend,
      runRole,
      journal: async (e) => events.push(e),
    }),
    backend,
    calls,
    events,
  };
}
const start = (f) =>
  f.machine.start({
    owner: "owner",
    parent: { id: "owner" },
    requirements: "Transfer money without creation",
    signal: signal(),
  });
const step = (f, id) =>
  f.machine.step({
    owner: "owner",
    parent: { id: "owner" },
    id,
    signal: signal(),
  });
test("separate roles, frozen contract and receipt bind exact compiler/source/artifact", async () => {
  const f = fixture([spec(), impl()]);
  const j = await start(f);
  const r = await step(f, j.id);
  assert.equal(r.status, "admitted");
  assert.deepEqual(
    f.calls.map((c) => c.role),
    ["specification", "executor"],
  );
  assert.notEqual(f.calls[0].schema, f.calls[1].schema);
  assert.equal(r.admitted.receipt.contract_hash, j.current.contract_hash);
  assert.equal(r.admitted.receipt.source_hash, digest(impl().source));
  assert.equal(r.admitted.receipt.compiler.sha256, "compiler-test");
  assert.equal(r.admitted.receipt.artifact_hash, digest("generated artifact"));
  assert.match(f.calls[1].prompt, /UNKNOWN/);
});
test("cannot weaken contracts through implementation source", async () => {
  const f = fixture([spec(), impl("contract weakened|body valid")]);
  const j = await start(f);
  assert.equal((await step(f, j.id)).status, "not_verified");
  assert.equal(f.machine.view(j.id, "owner").current.number, 1);
});
test("unknown is not admitted or automatically amended", async () => {
  const f = fixture([spec(), impl("contract A|body unknown")]);
  const j = await start(f);
  const r = await step(f, j.id);
  assert.equal(r.status, "not_verified");
  assert.equal(r.admitted.ok, false);
  assert.equal(r.current.number, 1);
});
test("only independent authority approves versioned petition", async () => {
  const f = fixture([
    spec(),
    {
      kind: "petition",
      source: "contract B|body placeholder",
      explanation: "Requirement conflict needs review",
      evidence_ids: [],
    },
    {
      ...spec("contract B|body placeholder"),
      decision: "approve",
      rationale: "Original task permits this precise bound",
    },
    impl("contract B|body valid"),
  ]);
  const j = await start(f);
  const r = await step(f, j.id);
  assert.equal(r.status, "amended");
  assert.equal(r.current.number, 2);
  assert.deepEqual(
    f.calls.map((c) => c.role),
    ["specification", "executor", "specification"],
  );
  assert.equal(r.events.filter((e) => e.type === "contract_frozen").length, 2);
  await assert.rejects(
    f.machine.verify({
      id: j.id,
      owner: "owner",
      version: 1,
      source: impl().source,
      signal: signal(),
    }),
    /Stale/,
  );
  assert.equal((await step(f, j.id)).status, "admitted");
});
test("denied petition cannot change frozen contract", async () => {
  const f = fixture([
    spec(),
    {
      kind: "petition",
      source: "contract weaker|body valid",
      explanation: "hard to prove",
      evidence_ids: [],
    },
    {
      ...spec(),
      decision: "deny",
      rationale: "Difficulty is not contradiction",
    },
  ]);
  const j = await start(f);
  const r = await step(f, j.id);
  assert.equal(r.status, "petition_denied");
  assert.equal(r.current.contract_hash, j.current.contract_hash);
});
test("unknown evidence cannot be fabricated or reused across workflow", async () => {
  const f = fixture([
    spec(),
    {
      kind: "petition",
      source: "",
      explanation: "claim",
      evidence_ids: ["invented"],
    },
  ]);
  const j = await start(f);
  await assert.rejects(step(f, j.id), /Unknown or stale/);
  assert.equal(f.calls.length, 2);
});
test("owner isolation and detached snapshots", async () => {
  const f = fixture([spec()]);
  const j = await start(f);
  assert.throws(() => f.machine.view(j.id, "stranger"), /wrong owner/);
  j.current.source = "weakened";
  assert.equal(f.machine.view(j.id, "owner").current.source, spec().source);
});
test("concurrent steps rejected, cancellation cannot admit", async () => {
  let release;
  const f = fixture([spec(), () => new Promise((r) => (release = r))]);
  const j = await start(f);
  const ctl = new AbortController();
  const p = f.machine.step({
    id: j.id,
    owner: "owner",
    parent: { id: "owner" },
    signal: ctl.signal,
  });
  await new Promise((r) => setImmediate(r));
  await assert.rejects(step(f, j.id), /already active/);
  ctl.abort();
  release(impl());
  await assert.rejects(p, /abort/i);
  assert.equal(f.machine.view(j.id, "owner").status, "failed");
});
test("journal chain records full provenance without mutable evidence", async () => {
  const f = fixture([spec(), impl()]);
  const j = await start(f);
  await step(f, j.id);
  for (let i = 1; i < f.events.length; i++)
    assert.equal(f.events[i].previous, f.events[i - 1].digest);
  assert.ok(Object.isFrozen(f.events[0]));
});
test("capability gate denies arbitrary shell/eval/fs/delegation/bypass including retired roles", () => {
  const roles = new Map([
      ["exec", { role: "executor" }],
      ["spec", { role: "specification" }],
    ]),
    known = new Set(["exec", "spec", "retired"]);
  for (const name of [
    "bash",
    "run_code",
    "write_file",
    "spawn_agent",
    "vera_verify",
    "vera_codegen",
    "vera_workflow_start",
    "vera_workflow_step",
  ])
    assert.ok(roleGuard(roles, known, { agent: { id: "exec" }, name }), name);
  for (const name of [
    "vera_help",
    "vera_check",
    "vera_contract_verify",
    "vera_contract_codegen",
    "structured_output",
  ])
    assert.equal(
      roleGuard(roles, known, { agent: { id: "exec" }, name }),
      undefined,
    );
  assert.equal(
    roleGuard(roles, known, { agent: { id: "spec" }, name: "vera_verify" }),
    undefined,
  );
  assert.ok(
    roleGuard(roles, known, {
      agent: { id: "spec" },
      name: "vera_contract_codegen",
    }),
  );
  assert.ok(
    roleGuard(roles, known, {
      agent: { id: "retired" },
      name: "structured_output",
    }),
  );
  assert.equal(
    roleGuard(roles, known, { agent: { id: "normal" }, name: "bash" }),
    undefined,
  );
});
test("concurrent proof evidence keeps journal chain serial", async () => {
  const f = fixture([spec()]);
  const j = await start(f);
  await Promise.all(
    Array.from({ length: 10 }, () =>
      f.machine.verify({
        id: j.id,
        owner: "owner",
        version: 1,
        source: impl().source,
        signal: signal(),
      }),
    ),
  );
  for (let i = 1; i < f.events.length; i++) {
    assert.equal(f.events[i].seq, i);
    assert.equal(f.events[i].previous, f.events[i - 1].digest);
  }
});

test("denial and failed attempts are carried into fresh executor context", async () => {
  const f = fixture([
    spec(),
    {
      kind: "petition",
      source: "",
      explanation: "difficult",
      evidence_ids: [],
    },
    {
      ...spec(),
      decision: "deny",
      rationale: "Use the existing bound; difficulty is not contradiction",
    },
    impl(),
  ]);
  const j = await start(f);
  await step(f, j.id);
  await step(f, j.id);
  assert.match(f.calls.at(-1).prompt, /Use the existing bound/);
  assert.match(f.calls.at(-1).prompt, /PRIOR ATTEMPT/);
});
test("rejected final candidate remains repairable", async () => {
  const f = fixture([spec(), impl("contract weaker|body valid"), impl()]);
  const j = await start(f);
  assert.equal((await step(f, j.id)).status, "not_verified");
  assert.equal((await step(f, j.id)).status, "admitted");
  assert.match(f.calls.at(-1).prompt, /Contract mismatch/);
});
