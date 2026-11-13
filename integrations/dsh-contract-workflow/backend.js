import { spawn } from "node:child_process";
import { readFile, mkdtemp, writeFile, rm } from "node:fs/promises";
import { join, isAbsolute } from "node:path";
import { tmpdir } from "node:os";
import { digest } from "./workflow.js";
export async function compilerBackend(binary) {
  if (!isAbsolute(binary)) throw Error("Compiler path must be absolute");
  if (process.platform !== "linux")
    throw Error(
      "Contract workflow process cancellation currently requires Linux",
    );
  const identity = { path: binary, sha256: digest(await readFile(binary)) };
  async function run(args, source, signal) {
    signal.throwIfAborted();
    if (digest(await readFile(binary)) !== identity.sha256)
      throw Error(
        "Compiler changed; restart workflow with explicit new compiler identity",
      );
    const child = spawn(binary, args, {
      stdio: ["pipe", "pipe", "pipe"],
      shell: false,
      detached: true,
    });
    let stdout = "",
      stderr = "";
    const terminate = () => {
      try {
        process.kill(-child.pid, "SIGTERM");
      } catch {}
    };
    const kill = () => {
      try {
        process.kill(-child.pid, "SIGKILL");
      } catch {}
    };
    let killer;
    const cancel = () => {
      terminate();
      killer = setTimeout(kill, 1000);
    };
    signal.addEventListener("abort", cancel, { once: true });
    if (signal.aborted) cancel();
    child.stdin.on("error", () => {});
    child.stdin.end(source);
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (s) => (stdout += s));
    child.stderr.on("data", (s) => (stderr += s));
    try {
      const code = await new Promise((resolve, reject) => {
        child.once("error", reject);
        child.once("close", (code) => resolve(code));
      });
      signal.throwIfAborted();
      return { code, stdout, stderr };
    } finally {
      signal.removeEventListener("abort", cancel);
      clearTimeout(killer);
      kill();
    }
  }
  const parse = (r) => {
    try {
      return JSON.parse(r.stdout, (_key, value, context) =>
        typeof value === "number" &&
        context.source &&
        /^-?\d+$/.test(context.source) &&
        !Number.isSafeInteger(value)
          ? context.source
          : value,
      );
    } catch {
      throw Error("Compiler returned non-JSON result: " + r.stderr);
    }
  };
  return {
    identity,
    async freeze(source, signal) {
      const r = await run(["contract", "-", "--json"], source, signal);
      if (r.code !== 0)
        throw Error("Contract freeze failed: " + r.stderr + r.stdout);
      return parse(r);
    },
    async compare(frozen, source, signal) {
      const dir = await mkdtemp(join(tmpdir(), "vera-frozen-"));
      try {
        const path = join(dir, "contract.json");
        await writeFile(path, JSON.stringify(frozen), { mode: 0o600 });
        const r = await run(
          ["contract", "-", "--frozen", path, "--json"],
          source,
          signal,
        );
        if (r.code !== 0) {
          let detail;
          try {
            detail = parse(r);
          } catch {
            detail = { diagnostic: r.stderr };
          }
          return { ok: false, comparison: detail };
        }
        const value = parse(r);
        return { ok: value.matches === true, comparison: value };
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    },
    async verify(source, signal) {
      const r = await run(["verify", "-", "--json"], source, signal);
      const result = parse(r);
      if (result.results)
        for (const item of result.results) {
          delete item.smt_safety;
          delete item.smt_panic;
          delete item.smt_liveness;
        }
      return { ok: r.code === 0, result, diagnostic: r.stderr };
    },
    async codegen(source, signal) {
      const dir = await mkdtemp(join(tmpdir(), "vera-host-api-"));
      try {
        const apiPath = join(dir, "api.json");
        const r = await run(
          ["codegen", "rust", "-", "--no-main", "--api-out", apiPath],
          source,
          signal,
        );
        if (r.code !== 0)
          return { ok: false, code: null, diagnostic: r.stderr };
        const manifest = parse({
          stdout: await readFile(apiPath, "utf8"),
          stderr: "Missing or invalid generated Rust host API manifest",
        });
        if (manifest.proof_admitted !== true ||
            manifest.api?.schema_version !== 1 ||
            manifest.api?.target !== "rust" ||
            !Array.isArray(manifest.api?.functions))
          throw Error("Compiler did not return a proof-admitted Rust host API manifest");
        return { ok: true, code: r.stdout, host_api: manifest.api, diagnostic: r.stderr };
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    },
  };
}
