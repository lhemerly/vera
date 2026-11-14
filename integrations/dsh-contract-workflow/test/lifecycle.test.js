import test from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdtemp, rm, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { apply } from "../index.js";
const binary = process.env.VERA_CONTRACT_BINARY;
test("missing isolation capabilities fail closed", async () => {
  await assert.rejects(apply({}, {}), /isolation unavailable/);
});
test("context-inheriting provider rejected instead of silently forking", async () => {
  const ctx = {
    tools: { guard() {}, restrict() {}, presentAs() {} },
    on() {},
    subagents: {
      startActivation() {},
      getProvider: () => ({
        inheritsParentContext: true,
        prepareContinuable() {},
        capabilities: { outputSchema: true },
      }),
    },
  };
  await assert.rejects(apply(ctx, { provider: "fork" }), /fresh local/);
});
test(
  "failed child disposal revokes capability and journals closure",
  { skip: !binary },
  async () => {
    const dir = await mkdtemp(join(tmpdir(), "vera-role-test-"));
    try {
      let guard, onCreated, childId;
      const registered = new Map(),
        effects = [];
      const scoped = {
        tools: { presentAs() {}, restrict() {} },
        systemPrompt: { section() {} },
      };
      const ctx = {
        tools: {
          guard: (g) => (guard = g),
          restrict() {},
          presentAs() {},
          register: (t) => registered.set(t.name, t),
        },
        agents: {},
        systemPrompt: { section() {} },
        on: (event, f) => {
          if (event === "agent/created") onCreated = f;
        },
        effect: (f) => effects.push(f()),
        subagents: {
          getProvider: () => ({
            inheritsParentContext: false,
            prepareContinuable() {},
            capabilities: { outputSchema: true },
          }),
          startActivation: async (spec) => {
            childId = spec.childId;
            onCreated({ agent: { id: childId, ctx: scoped } });
            return {
              childId,
              result: Promise.resolve({
                stopReason: "completed",
                structured: {
                  source:
                    "module T; transition t() reads [] writes [] body { commit; }",
                  requirements_map: "test",
                  coverage: [
                    {
                      id: "R1",
                      requirement: "test",
                      disposition: "core_contract",
                      justification: "fixture",
                      evidence_ids: [],
                    },
                  ],
                  assumptions: "none",
                  acceptance_tests: "test",
                },
              }),
              dispose: async () => {
                throw Error("fixture disposal failed");
              },
            };
          },
        },
      };
      await writeFile(
        join(dir, "task.json"),
        JSON.stringify({
          id: "test",
          original_requirements: "test",
          provenance: "fixture",
        })
      );
      await apply(ctx, {
        binary,
        stateDirectory: dir,
        taskManifest: join(dir, "task.json"),
        provider: "spawn",
      });
      await assert.rejects(
        registered
          .get("vera_workflow_start")
          .execute(
            { requirements: "test" },
            { agent: { id: "parent" }, signal: new AbortController().signal }
          ),
        /disposal failed/
      );
      assert.match(
        guard({ agent: { id: childId }, name: "vera_help" }),
        /closed/
      );
      const rows = (await readFile(join(dir, "audit.jsonl"), "utf8"))
        .trim()
        .split("\n")
        .map(JSON.parse);
      assert.ok(rows.some((r) => r.type === "role_closed"));
      for (const effect of effects) await effect();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }
);
