# VERA contract-authority workflow for DSH

An **opt-in companion plugin** separating specification authority from implementation. It does not replace the ordinary VERA tools or silently change existing DSH sessions.

## What it does

1. `vera_workflow_start` starts a fresh specification model session with the supplied original requirements, not the owner's conversation history.
2. The compiler parses and type-checks its source, canonically freezes the complete specification AST, and identifies the permitted implementation bodies. Functions transitively referenced by specifications are protected.
3. `vera_workflow_step` starts a different, fresh executor session. Its capabilities are limited to language help, syntax checking and contract-bound proof/code generation. It cannot use filesystem, shell, arbitrary code execution or delegation tools.
4. The executor returns an implementation or a structured petition. A petition goes to another independent specification-authority session with the original requirements, frozen contract, recorded evidence and proposed change. It may approve an explicit revision, deny, or request clarification.
5. A subsequent step receives the previous attempt, counterexample or review feedback. Ordinary candidate rejection remains repairable. There is no silent contract weakening or unbounded internal retry loop.
6. An admitted Rust artifact has a receipt binding the requirements, contract revision, source, compiler binary, proof evidence and generated artifact digests.

`UNKNOWN`, timeout and failure to find an implementation are not proof that the requirements are impossible. A successful core proof is not proof of external behavior, independent acceptance tests, or specification adequacy.

## Scope and threat boundary

The isolated roles work on **VERA cores**, not unrestricted whole-project coding. The ordinary owner agent can integrate admitted Rust into a host application and run external acceptance tests. Those host operations do not acquire a VERA proof merely by using the artifact.

The authority store is private plugin-process memory. A private append-only JSONL audit records decisions, versions, child identities and evidence. Hash chaining is diagnostic integrity evidence, **not a cryptographic signature or protection from a hostile host owner**.

Fresh DSH sessions provide context separation; they are not OS process isolation. Real model-level capability isolation is enforced through native tool visibility and a monotonic dispatch guard. This requires a trusted DSH host, native plugins, compiler and provider implementation. The workflow fails closed if a fresh local structured-output provider or guard support is absent. It does not defend against a malicious native plugin, compromised compiler, OS administrator or owner agent modifying the host installation.

Retired or previously recorded child identities are denied after plugin restart. Historical workflows are audit-only: the plugin never restores authority from an executor-supplied/worktree artifact. Start a new workflow after restart. Cancellation revokes role capabilities before teardown, propagates to subprocess groups, and waits for cleanup. No default proof timeout is added by this companion.

The original VERA tools remain available to ordinary sessions. Their successful outputs are **not** contract-workflow admission receipts. Conversely, executor-role access to unconstrained `vera_verify`, `vera_codegen`, `vera_run`, shell, filesystem, PTC and further delegation is denied.

## Installation

Requires Linux, Node 24+, DSH `0.2.1-rc.5`, the ordinary native VERA plugin, the fresh `spawn` subagent provider, and a VERA compiler supporting `vera contract`.

Add this optional row to an isolated DSH profile or your chosen profile, after enabling the ordinary VERA plugin and subagent runtime:

```yaml
- id: vera-contract-workflow
  name: /absolute/path/to/dsh-contract-workflow/index.js
  config:
    binary: /absolute/path/to/vera
    stateDirectory: /absolute/private/path/to/vera-contract-state
    provider: spawn
```

The state directory must be private (mode `0700`) and not a symlink. Audit files are created with `0600`. The compiler binary is hashed at startup and rechecked before each invocation; replacing it mid-workflow fails closed.

This repository does not include model credentials, model configuration overrides or model-call budgets. The child sessions inherit the selected model route. Both `start` and `step` make model calls; `get` only reads the in-memory workflow.

## Tools

- `vera_workflow_start(requirements)`: freeze an independently authored contract.
- `vera_workflow_get(id)`: inspect owned current contract and audit history.
- `vera_workflow_step(id)`: one executor attempt and, if requested, one independent petition review. Continue after `amended`, `petition_denied` or `not_verified`. `needs_clarification` is not silently guessed away.
- `vera_contract_verify(source)` / `vera_contract_codegen(source)`: available only inside the isolated executor. Workflow/version are derived from trusted session identity, not caller arguments.

Authority decisions and recorded evidence are never automatically treated as human approval for external actions.

## Tests

Install/resolve the declared DSH peer packages from the same DSH installation used by the host. Run:

```sh
node --test test/*.test.js
```

Compiler and full native integration tests explicitly skip unless their dependencies are supplied:

```sh
VERA_CONTRACT_BINARY=/absolute/path/to/vera \
VERA_NATIVE_PLUGIN=/absolute/path/to/native-vera/index.js \
DSH_CLI=/absolute/path/to/@deepseek-ai/dsh/lib/bin.js \
node --test test/*.test.js
```

The native test uses a local fake HTTP provider, isolated DSH home and dummy key. It makes **zero external model calls**. It verifies distinct fresh sessions without owner-history leakage, denied tool bypass, protected-contract rejection, concrete counterexample/footprint feedback and final artifact admission. Other tests cover petitions, amendments, stale/forged evidence, owner isolation, cancellation, teardown failure, concurrent audit ordering and repair-feedback continuity.

Tests do not establish model quality, specification completeness, or convergence of real AI repair loops.
