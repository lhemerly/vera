import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { apply } from "../index.js";
function fixture() {
  const tools = new Map();
  let calls = 0;
  return {
    tools,
    get calls() {
      return calls;
    },
    ctx: {
      tools: {
        guard() {},
        restrict() {},
        presentAs() {},
        register: (t) => tools.set(t.name, t),
      },
      agents: {},
      systemPrompt: { section() {} },
      on() {},
      effect() {},
      subagents: {
        getProvider: () => ({
          inheritsParentContext: false,
          prepareContinuable() {},
          capabilities: { outputSchema: true },
        }),
        startActivation: async () => {
          calls++;
          throw Error("fixture interruption");
        },
      },
    },
  };
}
const exec = () => ({
  agent: { id: "owner" },
  signal: new AbortController().signal,
});
test("two plugin instances cannot reserve the same task; failure/restart/changed task cannot reset", async () => {
  const dir = await mkdtemp(join(tmpdir(), "vera-registration-"));
  try {
    const path = join(dir, "task.json");
    await writeFile(
      path,
      JSON.stringify({
        id: "whole-task",
        original_requirements: "all requirements",
        provenance: "host",
      })
    );
    const config = {
      binary: "/bin/true",
      stateDirectory: dir,
      taskManifest: path,
      provider: "spawn",
    };
    const a = fixture(),
      b = fixture();
    await apply(a.ctx, config);
    await apply(b.ctx, config);
    await assert.rejects(
      a.tools
        .get("vera_workflow_start")
        .execute({ requirements: "tiny subset" }, exec()),
      /fixture interruption/
    );
    await assert.rejects(
      b.tools
        .get("vera_workflow_start")
        .execute({ requirements: "new subset" }, exec()),
      /reserved/
    );
    assert.equal(a.calls, 1);
    assert.equal(b.calls, 0);
    await writeFile(
      path,
      JSON.stringify({
        id: "replacement",
        original_requirements: "different",
        provenance: "host",
      })
    );
    const c = fixture();
    await apply(c.ctx, config);
    await assert.rejects(
      c.tools
        .get("vera_workflow_start")
        .execute({ requirements: "try again" }, exec()),
      /reserved/
    );
    assert.equal(c.calls, 0);
    const audit = await readFile(join(dir, "audit.jsonl"), "utf8");
    assert.match(audit, /whole-task/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
test("malformed audit fails before tools or model calls", async () => {
  const dir = await mkdtemp(join(tmpdir(), "vera-registration-"));
  try {
    const path = join(dir, "task.json");
    await writeFile(
      path,
      JSON.stringify({
        id: "t",
        original_requirements: "task",
        provenance: "host",
      })
    );
    await writeFile(join(dir, "audit.jsonl"), '{"truncated":', { mode: 0o600 });
    const f = fixture();
    await assert.rejects(
      apply(f.ctx, {
        binary: "/bin/true",
        stateDirectory: dir,
        taskManifest: path,
        provider: "spawn",
      })
    );
    assert.equal(f.calls, 0);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
