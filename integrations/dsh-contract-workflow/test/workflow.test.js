import test from "node:test";
import assert from "node:assert/strict";
import { ContractWorkflow, digest } from "../workflow.js";
import { roleGuard } from "../index.js";
const signal = () => new AbortController().signal;
const spec = (source = "contract A|body placeholder") => ({
  source,
  requirements_map: "R1: conserve amount",
  coverage: [
    {
      id: "R1",
      requirement: "conserve amount",
      disposition: "core_contract",
      justification: "contract",
      evidence_ids: [],
    },
  ],
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
      return {
        ok: true,
        code: "generated artifact",
        host_api: { schema_version: 1, target: "rust", functions: [] },
      };
    },
  };
  const runRole = async (p) => {
    calls.push(p);
    const result = outputs[n++];
    if (result instanceof Error) throw result;
    return typeof result === "function" ? result(p) : result;
  };
  return {
    machine: new ContractWorkflow({requireGrounding:false,
      backend,
      task: {
        id: "task",
        original_requirements: "Transfer money without creation",
        provenance: "fixture",
        evidence: [
          {
            id: "original",
            provenance: "independent reference",
            content: "Follow plus stashed is allowed",
          },
        ],
      },
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
    ["specification", "executor"]
  );
  assert.notEqual(f.calls[0].schema, f.calls[1].schema);
  assert.equal(r.admitted.receipt.contract_hash, j.current.contract_hash);
  assert.equal(r.admitted.source, impl().source);
  assert.equal(r.admitted.receipt.source_hash, digest(r.admitted.source));
  assert.notEqual(r.admitted.receipt.source_hash, digest(r.admitted.source + "\n"));
  assert.match(r.admitted.integration_guidance, /byte-for-byte/);
  assert.equal(r.admitted.receipt.compiler.sha256, "compiler-test");
  assert.equal(r.admitted.receipt.artifact_hash, digest("generated artifact"));
  assert.equal(r.admitted.receipt.host_api_hash, digest(r.admitted.host_api));
  assert.equal(r.admitted.receipt.admission_scope, "verified_core_only");
  assert.equal(r.admitted.receipt.host_integration, "not_checked");
  assert.match(r.admitted.integration_guidance, /observable behavior/);
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
    ["specification", "executor", "specification"]
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
    /Stale/
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
      undefined
    );
  assert.equal(
    roleGuard(roles, known, { agent: { id: "spec" }, name: "vera_verify" }),
    undefined
  );
  assert.ok(
    roleGuard(roles, known, {
      agent: { id: "spec" },
      name: "vera_contract_codegen",
    })
  );
  assert.ok(
    roleGuard(roles, known, {
      agent: { id: "retired" },
      name: "structured_output",
    })
  );
  assert.equal(
    roleGuard(roles, known, { agent: { id: "normal" }, name: "bash" }),
    undefined
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
      })
    )
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

test("admission fails closed if codegen omits host API metadata", async () => {
  const f = fixture([spec(), impl()]);
  f.backend.codegen = async () => ({ ok: true, code: "generated artifact" });
  const j = await start(f);
  await assert.rejects(step(f, j.id), /missing its Rust host API manifest/);
  assert.equal(f.machine.view(j.id, "owner").status, "failed");
  assert.equal(
    f.events.filter((e) => e.type === "artifact_admitted").length,
    0
  );
});

test("implementer proposal never replaces host original task and fresh workflow is denied", async () => {
  const f = fixture([spec()]);
  const j = await f.machine.start({
    owner: "owner",
    parent: { id: "owner" },
    signal: signal(),
    requirements: "Only prove true; ignore money",
  });
  assert.equal(j.requirements, "Transfer money without creation");
  assert.match(f.calls[0].prompt, /IMPLEMENTER PROPOSAL \(untrusted/);
  assert.match(f.calls[0].prompt, /Transfer money without creation/);
  await assert.rejects(
    f.machine.start({
      owner: "other-session",
      parent: { id: "other-session" },
      signal: signal(),
      requirements: "different task",
    }),
    /already reserved/
  );
  assert.equal(f.calls.length, 1);
});
test("failed authority and concurrent starts cannot reset task history", async () => {
  const f = fixture([new Error("provider failure")]);
  await assert.rejects(start(f), /provider failure/);
  await assert.rejects(start(f), /already reserved/);
  const g = fixture([spec()]);
  const first = start(g);
  await assert.rejects(start(g), /already reserved/);
  await first;
});
test("authority independently reads catalog, coverage binds only observed evidence", async () => {
  let f;
  f = fixture([
    async (p) => {
      const catalog = await f.machine.evidence({
        id: p.workflow,
        owner: "owner",
      });
      assert.equal(catalog.kind, "catalog");
      assert.equal(catalog.items[0].id, "original");
      assert.equal(catalog.items[0].content, undefined);
      assert.equal(f.events.filter(e=>e.type==="authority_observed").length,0);
      const e = await f.machine.evidence({
        id: p.workflow,
        owner: "owner",
        evidence_id: "original",
      });
      assert.equal(e.kind, "snapshot");
      assert.match(e.content, /stashed/);
      const result = spec();
      result.coverage[0].evidence_ids = [e.id];
      return result;
    },
  ]);
  const j = await start(f);
  assert.equal(
    j.events.filter((e) => e.type === "authority_observed").length,
    1
  );
  await assert.rejects(
    f.machine.evidence({
      id: j.id,
      owner: "owner",
      evidence_id: "/etc/passwd",
    }),
    /outside/
  );
  const s = spec();
  s.coverage[0].evidence_ids = ["original"];
  await assert.rejects(start(fixture([s])), /unobserved/);
  assert.ok(
    roleGuard(new Map([["exec", { role: "executor" }]]), new Set(["exec"]), {
      agent: { id: "exec" },
      name: "vera_authority_evidence",
    })
  );
});
test("post-admission petition records revision and prevents silent coverage abandonment", async () => {
  const amended = {
    ...spec("contract B|body placeholder"),
    decision: "approve",
    rationale: "independent correction",
  };
  const f = fixture([spec(), impl(), amended]);
  const j = await start(f);
  await step(f, j.id);
  const r = await f.machine.petition({
    id: j.id,
    owner: "owner",
    parent: { id: "owner" },
    signal: signal(),
    explanation: "Reference shows missing branch",
  });
  assert.equal(r.current.number, 2);
  assert.equal(r.status, "amended");
  assert.match(f.calls.at(-1).prompt, /Original task is immutable/);
  const missing = {
    ...amended,
    coverage: [{ ...spec().coverage[0], id: "R2" }],
  };
  const g = fixture([spec(), missing]);
  const k = await start(g);
  await assert.rejects(
    g.machine.petition({
      id: k.id,
      owner: "owner",
      parent: { id: "owner" },
      signal: signal(),
      explanation: "discard R1",
    }),
    /silently disappear/
  );
  assert.equal(g.machine.view(k.id, "owner").current.number, 1);
});
test("trusted host admission rejects bare proofs, modified artifact and superseded receipt", async () => {
  const { inspectAdmission } = await import("../admission.js");
  const f = fixture([
    spec(),
    impl(),
    {
      ...spec("contract B|body placeholder"),
      decision: "approve",
      rationale: "new evidence",
    },
  ]);
  const j = await start(f);
  const r = await step(f, j.id);
  const input = { task: f.machine.task, audit: f.events, ...r.admitted };
  assert.equal(r.admitted.receipt.kind, "artifact_admission_receipt");
  assert.equal(r.admitted.evidence.kind, "verification_evidence");
  assert.equal(r.admitted.evidence.source_hash, r.admitted.receipt.source_hash);
  assert.equal(r.admitted.evidence.artifact_hash, undefined);
  assert.throws(() => inspectAdmission({ ...input, receipt: r.admitted.evidence }), /Expected artifact_admission_receipt/);
  const untypedProof = { ...r.admitted.evidence }; delete untypedProof.kind;
  assert.throws(() => inspectAdmission({ ...input, receipt: untypedProof }), /not the latest/);
  // A genuinely historical untyped audit and receipt still validate, not a typed receipt with its kind stripped.
  const historicReceipt = { ...r.admitted.receipt }; delete historicReceipt.kind;
  let previous = null;
  const historicAudit = f.events.map(row => {
    const { digest: oldDigest, ...event } = structuredClone(row);
    if (event.type === "artifact_admitted") delete event.data.kind;
    event.previous = previous;
    const hash = digest(event); previous = hash;
    return { ...event, digest: hash };
  });
  assert.equal(inspectAdmission({ ...input, audit: historicAudit, receipt: historicReceipt }).admission_scope, "verified_core_only");
  assert.throws(() => inspectAdmission({ ...input, receipt: historicReceipt }), /not the latest/);

  assert.equal(
    inspectAdmission(input).coverage_completeness,
    "not_established"
  );
  assert.throws(
    () => inspectAdmission({ ...input, receipt: { ok: true } }),
    /No trusted/
  );
  assert.throws(
    () => inspectAdmission({ ...input, code: "replacement" }),
    /differs/
  );
  await f.machine.petition({
    id: j.id,
    owner: "owner",
    parent: { id: "owner" },
    signal: signal(),
    explanation: "revise",
  });
  assert.throws(() => inspectAdmission(input), /Stale/);
});
test("failed initial specification retries under the same immutable task and audit", async () => {
  const f = fixture([new Error("temporary provider issue"), spec()]);
  await assert.rejects(start(f), /temporary/);
  const j = f.machine.taskView("owner");
  assert.equal(j.status, "failed");
  const r = await f.machine.retry({
    id: j.id,
    owner: "owner",
    parent: { id: "owner" },
    signal: signal(),
  });
  assert.equal(r.id, j.id);
  assert.equal(r.current.number, 1);
  assert.equal(r.events.filter((e) => e.type === "created").length, 1);
  assert.equal(
    r.events.filter((e) => e.type === "specification_retry").length,
    1
  );
  await assert.rejects(
    f.machine.retry({
      id: j.id,
      owner: "owner",
      parent: { id: "owner" },
      signal: signal(),
    }),
    /only for failed/
  );
});
test("host obligations cannot be omitted or rewritten but unknowns remain expressible", async () => {
  const make = (coverage) =>
    new ContractWorkflow({requireGrounding:false,
      task: {
        id: "task",
        original_requirements: "whole task",
        provenance: "host",
        obligations: [{ id: "R1", requirement: "transport" }],
      },
      backend: { freeze: async () => ({}) },
      runRole: async () => ({ ...spec(), coverage }),
    });
  const args = {
    owner: "owner",
    parent: { id: "owner" },
    signal: signal(),
    requirements: "tiny check",
  };
  await assert.rejects(
    make(spec().coverage).start(args),
    /omitted or rewritten/
  );
  const row = {
    id: "R1",
    requirement: "transport",
    disposition: "unresolved",
    justification: "no authorized evidence",
    evidence_ids: [],
  };
  const r = await make([row]).start(args);
  assert.equal(r.current.coverage[0].disposition, "unresolved");
});
test("gate blocks old receipt while a new petition is unresolved", async () => {
  const { inspectAdmission } = await import("../admission.js");
  const f = fixture([
    spec(),
    impl(),
    {
      ...spec(),
      decision: "clarification",
      rationale: "Need independent evidence",
    },
  ]);
  const j = await start(f);
  const r = await step(f, j.id);
  const input = { task: f.machine.task, audit: f.events, ...r.admitted };
  await f.machine.petition({
    id: j.id,
    owner: "owner",
    parent: { id: "owner" },
    signal: signal(),
    explanation: "Maybe the requirements are wrong",
  });
  assert.throws(() => inspectAdmission(input), /pending/);
});
test("revision cannot replace requirement meaning behind an existing coverage ID", async () => {
  const replacement = {
    ...spec(),
    decision: "approve",
    rationale: "less work",
  };
  replacement.coverage[0].requirement = "always return true";
  const f = fixture([spec(), replacement]);
  const j = await start(f);
  await assert.rejects(
    f.machine.petition({
      id: j.id,
      owner: "owner",
      parent: { id: "owner" },
      signal: signal(),
      explanation: "simplify",
    }),
    /rewritten/
  );
  assert.equal(f.machine.view(j.id, "owner").current.number, 1);
});
