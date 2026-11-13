import z from "@deepseek-ai/schemastery";
import { defineTool } from "@deepseek-ai/dsh-tools";
import { randomUUID } from "node:crypto";
import { mkdir, readFile, open, lstat } from "node:fs/promises";
import { constants } from "node:fs";
import { isAbsolute, join } from "node:path";
import { ContractWorkflow } from "./workflow.js";
import { compilerBackend } from "./backend.js";
export const name = "vera-contract-workflow";
export const inject = ["tools", "subagents", "agents", "systemPrompt"];
export const Config = z.object({
  binary: z.string().required(),
  stateDirectory: z.string().required(),
  provider: z.string().default("spawn"),
});
const output = {
  schema: { type: "object", additionalProperties: true },
  render: (_a, value) => [{ type: "text", text: JSON.stringify(value) }],
};
const string = (description) => ({
  type: "string",
  required: true,
  description,
});
const SPEC_TOOLS = ["vera_help", "vera_check", "vera_verify"];
const EXEC_TOOLS = [
  "vera_help",
  "vera_check",
  "vera_contract_verify",
  "vera_contract_codegen",
];
/** A real capability gate: executor children never receive shell/filesystem/eval/delegation tools. */
export function roleGuard(roles, known, exec) {
  const id = exec.agent?.id;
  if (!id || !known.has(id)) return;
  const role = roles.get(id);
  if (!role)
    return "Contract role is closed or belongs to an earlier plugin instance";
  const allowed = role.role === "specification" ? SPEC_TOOLS : EXEC_TOOLS;
  if (exec.name !== "structured_output" && !allowed.includes(exec.name))
    return "Tool unavailable in isolated contract role";
}
export async function apply(ctx, config) {
  if (
    !ctx.tools?.guard ||
    !ctx.tools?.restrict ||
    !ctx.tools?.presentAs ||
    !ctx.subagents?.startActivation ||
    !ctx.on
  )
    throw Error("Required DSH capability isolation unavailable");
  const provider = ctx.subagents.getProvider(config.provider);
  if (
    !provider ||
    provider.inheritsParentContext !== false ||
    !provider.prepareContinuable ||
    !provider.capabilities?.outputSchema
  )
    throw Error(
      "A fresh local structured-output spawn provider is required; no unsafe fallback",
    );
  if (!isAbsolute(config.stateDirectory))
    throw Error("stateDirectory must be absolute");
  await mkdir(config.stateDirectory, { recursive: true, mode: 0o700 });
  const stat = await lstat(config.stateDirectory);
  if (!stat.isDirectory() || stat.isSymbolicLink() || stat.mode & 0o077)
    throw Error("stateDirectory must be a private nonsymlink directory");
  const log = join(config.stateDirectory, "audit.jsonl");
  let old = "";
  try {
    const s = await lstat(log);
    if (s.isSymbolicLink() || !s.isFile() || s.mode & 0o077)
      throw Error("Unsafe audit file");
    old = await readFile(log, "utf8");
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
  }
  const known = new Set();
  for (const line of old.split("\n").filter(Boolean)) {
    const row = JSON.parse(line);
    if (row.type === "role_reserved") {
      if (typeof row.child_id !== "string") throw Error("Malformed role audit");
      known.add(row.child_id);
    }
  }
  let queue = Promise.resolve();
  const journal = (row) => {
    const next = queue.then(async () => {
      const file = await open(
        log,
        constants.O_WRONLY |
          constants.O_APPEND |
          constants.O_CREAT |
          constants.O_NOFOLLOW,
        0o600,
      );
      try {
        await file.writeFile(JSON.stringify(row) + "\n");
        await file.sync();
      } finally {
        await file.close();
      }
      const directory = await open(config.stateDirectory, "r");
      try {
        await directory.sync();
      } finally {
        await directory.close();
      }
    });
    queue = next;
    return next;
  };
  const backend = await compilerBackend(config.binary);
  const roles = new Map(),
    active = new Set();
  let disposed = false;
  ctx.tools.guard((exec) => roleGuard(roles, known, exec));
  ctx.on("agent/created", ({ agent }) => {
    if (!known.has(agent.id)) return;
    const role = roles.get(agent.id);
    if (!role)
      throw Error("Closed contract role cannot resume; start a new workflow");
    agent.ctx.tools.presentAs("native");
    agent.ctx.tools.restrict({
      allow: role.role === "specification" ? SPEC_TOOLS : EXEC_TOOLS,
    });
    agent.ctx.systemPrompt.section({
      name: "vera:isolated-contract-role",
      order: 9990,
      text: `You are the ${role.role} in a contract-separated VERA workflow. Use only the provided narrow tools. Filesystem, shell, arbitrary execution and delegation are unavailable. Do not claim unsupported external behavior has been proved.`,
      interpolate: false,
    });
    role.created = true;
  });
  const owner = (exec) => {
    if (!exec.agent?.id) throw Error("DSH Agent required");
    if (known.has(exec.agent.id))
      throw Error("Workflow lifecycle is controlled by its owner, not a child");
    if (disposed) throw Error("Plugin disposed");
    return exec.agent.id;
  };
  const runRole = async ({
    role,
    workflow,
    version,
    parent,
    signal,
    schema,
    prompt,
  }) => {
    if (disposed) throw Error("Plugin disposed");
    signal.throwIfAborted();
    const id = randomUUID();
    const meta = { role, workflow, version, owner: parent.id, created: false };
    known.add(id);
    await journal({
      type: "role_reserved",
      child_id: id,
      role,
      workflow,
      at: new Date().toISOString(),
    });
    roles.set(id, meta);
    let activation;
    const cancel = () => {
      roles.delete(id);
      if (activation) void activation.dispose().catch(() => {});
    };
    signal.addEventListener("abort", cancel, { once: true });
    try {
      activation = await ctx.subagents.startActivation({
        provider: config.provider,
        childId: id,
        label: `VERA ${role}`,
        delivery: "caller",
        signal,
        request: {
          parent,
          prompt: [{ type: "text", text: prompt }],
          outputSchema: schema,
        },
      });
      active.add(activation);
      if (!meta.created || activation.childId !== id)
        throw Error("Isolation was not established before child publication");
      if (signal.aborted) {
        await activation.dispose();
        signal.throwIfAborted();
      }
      const result = await activation.result;
      signal.throwIfAborted();
      if (result.stopReason !== "completed" || !result.structured)
        throw Error(
          `Contract ${role} ended without structured result: ${result.stopReason}`,
        );
      return result.structured;
    } finally {
      signal.removeEventListener("abort", cancel);
      roles.delete(id);
      try {
        if (activation) await activation.dispose();
      } finally {
        if (activation) active.delete(activation);
        await journal({ type: "role_closed", child_id: id });
      }
    }
  };
  const machine = new ContractWorkflow({ backend, runRole, journal });
  const register = (name, description, parameters, execute) =>
    ctx.tools.register(
      defineTool({ name, description, parameters, output, execute }),
    );
  register(
    "vera_workflow_start",
    "Create a fresh independent specification session, freeze its complete AST contract, and return a workflow ID. Makes model calls. Opt-in; does not affect ordinary VERA tools.",
    {
      requirements: string(
        "Original task and accepted requirements; preserve their meaning.",
      ),
    },
    async (args, exec) =>
      machine.start({
        requirements: args.requirements,
        owner: owner(exec),
        parent: exec.agent,
        signal: exec.signal,
      }),
  );
  register(
    "vera_workflow_get",
    "Read the owned frozen contract, revisions and audit trail.",
    { id: string("Workflow ID") },
    async (args, exec) => machine.view(args.id, owner(exec)),
  );
  register(
    "vera_workflow_step",
    "Run a fresh isolated executor against the frozen contract. If it petitions, request an independent authority review and record an explicit revision, denial or clarification. Makes model calls; call again after an amendment/denial to continue. Does not silently loop.",
    { id: string("Workflow ID") },
    async (args, exec) =>
      machine.step({
        id: args.id,
        owner: owner(exec),
        parent: exec.agent,
        signal: exec.signal,
      }),
  );
  for (const operation of ["verify", "codegen"])
    register(
      `vera_contract_${operation}`,
      `${operation === "verify" ? "Verify" : "Generate Rust for"} source against this executor session’s frozen contract. Rejects protected specification drift. Only available to isolated executor role; receipts bind source, compiler and contract.`,
      {
        source: string(
          "Complete VERA module with only permitted implementation bodies changed.",
        ),
      },
      async (args, exec) => {
        const role = roles.get(exec.agent?.id);
        if (disposed || role?.role !== "executor")
          throw Error(
            "Only an active isolated executor may use contract-bound admission",
          );
        return machine[operation]({
          id: role.workflow,
          owner: role.owner,
          version: role.version,
          source: args.source,
          signal: exec.signal,
        });
      },
    );
  ctx.systemPrompt.section({
    name: "vera:contract-workflow",
    order: 3070,
    text: "For independent specifications and implementation use the opt-in vera_workflow_start then vera_workflow_step. Contracts freeze before execution. Executor tools cannot modify them; petitions are independently reviewed and versioned. UNKNOWN/timeouts do not prove impossibility. An admitted VERA core does not prove external host behavior or requirements adequacy. Original six VERA tools remain ordinary unconstrained tools and do not carry workflow admission. After a restart historical workflows are audit-only; start a new one rather than trust replayed worktree claims.",
    interpolate: false,
  });
  ctx.effect(() => async () => {
    disposed = true;
    roles.clear();
    const results = await Promise.allSettled(
      [...active].map((a) => a.dispose()),
    );
    active.clear();
    await queue;
    const failed = results.find((r) => r.status === "rejected");
    if (failed) throw failed.reason;
  });
}
