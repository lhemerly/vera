# VERA contract-authority workflow for DSH

An **opt-in companion plugin** separating specification authority from implementation. It does not replace the ordinary VERA tools or silently change existing DSH sessions.

## What it does

1. The trusted host registers the full original task in `taskManifest`. `vera_workflow_start` starts its single authority session. Caller-supplied requirements are an untrusted focus proposal, not a replacement original task.
2. The compiler parses and type-checks its source, canonically freezes the complete specification AST, and identifies the permitted implementation bodies. Functions transitively referenced by specifications are protected.
3. `vera_workflow_step` starts a different, fresh executor session. Its capabilities are limited to language help, syntax checking and contract-bound proof/code generation. It cannot use filesystem, shell, arbitrary code execution or delegation tools.
4. The executor returns an implementation or a structured petition. A petition goes to another independent specification-authority session with the original requirements, frozen contract, recorded evidence and proposed change. It may approve an explicit revision, deny, or request clarification.
5. A subsequent step receives the previous attempt, counterexample or review feedback. Ordinary candidate rejection remains repairable. There is no silent contract weakening or unbounded internal retry loop.
6. An admitted Rust artifact has a receipt binding the requirements, contract revision, source, compiler binary, proof evidence generated artifact and host API manifest digests. The receipt explicitly says `admission_scope: "verified_core_only"` and `host_integration: "not_checked"`.

`UNKNOWN`, timeout and failure to find an implementation are not proof that the requirements are impossible. A successful core proof is not proof of external behavior, independent acceptance tests, or specification adequacy.

## Calling an admitted core

`admitted.host_api` maps source names and types to stable public Rust functions,
for example `functions::call_shipping_fee`. Save `admitted.code` unchanged in a
Rust module and use that named API, not private `vera_function_N` helpers. The
manifest is interface metadata, not an independent proof; its digest is bound
into the core admission receipt together with the generated code digest.

The owner must still compile and test the host. At least two inputs should
require different externally observable results, and invalid inputs should have
an observable rejection path. Replace the returned value with a constant as a
negative integration test: if the tests still pass, they do not establish useful
consumption of the decision. These cases are evidence, not a universal host-code
proof. See [the executable shipping example](../../examples/shipping).

Core admission does not execute the authority's acceptance-test text and does
not automatically mark host integration complete.

## Scope and threat boundary

The isolated roles work on **VERA cores**, not unrestricted whole-project coding. The ordinary owner agent can integrate admitted Rust into a host application and run external acceptance tests. Those host operations do not acquire a VERA proof merely by using the artifact.

The authority store is private plugin-process memory. A private append-only JSONL audit records decisions, versions, child identities and evidence. Hash chaining is diagnostic integrity evidence, **not a cryptographic signature or protection from a hostile host owner**.

Fresh DSH sessions provide context separation; they are not OS process isolation. Real model-level capability isolation is enforced through native tool visibility and a monotonic dispatch guard. This requires a trusted DSH host, native plugins, compiler and provider implementation. The workflow fails closed if a fresh local structured-output provider or guard support is absent. It does not defend against a malicious native plugin, compromised compiler, OS administrator or owner agent modifying the host installation.

Retired or previously recorded child identities are denied after plugin restart. Historical workflows are audit-only: the plugin never restores authority from an executor-supplied/worktree artifact. A durable exclusive reservation blocks a fresh workflow after restart, failure, concurrent plugin launch or changed task ID in the same state directory. Recovery after process restart requires the trusted host to preserve/reconcile the existing lineage; automatic restoration is not implemented. Do not delete state or switch directories to bypass this protection. Cancellation revokes role capabilities before teardown, propagates to subprocess groups, and waits for cleanup. No default proof timeout is added by this companion.

The original VERA tools remain available to ordinary sessions. Their successful outputs are **not** contract-workflow admission receipts. Conversely, executor-role access to unconstrained `vera_verify`, `vera_codegen`, `vera_run`, shell, filesystem, PTC and further delegation is denied.

## Installation

Requires Linux, Node 24+, DSH `0.2.1-rc.5`, the ordinary native VERA plugin, the fresh `spawn` subagent provider, and a VERA compiler supporting `vera contract` and Rust codegen `--api-out`.

Add this optional row to an isolated DSH profile or your chosen profile, after enabling the ordinary VERA plugin and subagent runtime:

```yaml
- id: vera-contract-workflow
  name: /absolute/path/to/dsh-contract-workflow/index.js
  config:
    binary: /absolute/path/to/vera
    stateDirectory: /absolute/private/path/to/vera-contract-state
    taskManifest: /absolute/protected/path/to/task.json
    provider: spawn
```

The state directory must be private (mode `0700`) and not a symlink. Audit files are created with `0600`. The compiler binary is hashed at startup and rechecked before each invocation; replacing it mid-workflow fails closed.

This repository does not include model credentials, model configuration overrides or model-call budgets. The child sessions inherit the selected model route. Both `start` and `step` make model calls; `get` only reads the in-memory workflow.

## Tools

- `vera_workflow_start(requirements)`: start the host-registered task; `requirements` is a focus proposal only.
- `vera_task_get()`: recover the existing workflow ID and status.
- `vera_workflow_retry(id)`: retry failed initial specification without replacing task or history.
- `vera_workflow_petition(id, explanation)`: request a versioned independent revision, also after admission or clarification.
- `vera_authority_evidence(evidence_id?)`: authority-only snapshot catalog/read, no arbitrary file paths or commands.
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

## Conventional Rust shells

The [hybrid workflow](../../docs/HYBRID_RUST.md) shows how typed verified cores
fit normal Rust applications and how to test genuine result use. Its standalone
`vera-build` helper proves source contracts but does not enforce this workflow's
frozen contract revisions. Keep the admission gate here when using Invariant God;
do not replace it with a fresh proof of implementer-edited requirements.

## Task authority, coverage and deployment gate (0.2 migration)

The trusted harness, **not the implementing model**, writes this manifest before
launching the actor. Paths/configuration/state must be outside actor write access.
For example:

```json
{
  "id": "stable-task-identity",
  "original_requirements": "Full original user or benchmark task, verbatim",
  "provenance": "Trusted harness input capture; source and version",
  "obligations": [
    {"id": "R1", "requirement": "Observable requirement from the original task"}
  ],
  "evidence": [
    {"id": "reference-help", "provenance": "Host-captured authorized reference --help, version/hash/date", "content": "Reference output"}
  ]
}
```

Manifest data is copied and frozen at initialization. Host-supplied provenance
is not independent authentication that the user authored it. Never include
hidden tests, solutions, credentials or unauthorized private files as evidence.

The authority can discover/select evidence independently of the implementer's
summary with a narrow read-only tool. Each read is audited with content hash and
provenance. This is **snapshot access**, not autonomous binary exploration or a
web browser. The host must collect permitted observations in advance; missing
observations remain unknown. There is no shell, filesystem path, evaluator or
network escape in this capability. Snapshot content is untrusted data, never
instructions. Evidence cannot be invented by citing an unread ID.

The authority owns a stable-ID coverage ledger with `core_contract`,
`host_test_required` and `unresolved` dispositions. Host-listed obligations must
appear with exact ID/text; later versions cannot silently drop or rewrite coverage IDs/text. Correct an
inaccurate extracted row by retaining it as unresolved with an explanation and
adding the corrected row.
Authority-discovered requirements can be added. The host list and AI extraction
can still be incomplete. `coverage_completeness: not_established` is always
reported; neither row counts nor proof success are a completeness score.
One task's module may contain multiple cores. Add/change cores through petitions
on that module, not parallel unlinked workflows. Per-core independent workflow
slots are not implemented.

Approved revisions bind new coverage and contracts into receipts and supersede
prior admissions. Owner petitions are allegations, not independent observations.
They cannot mutate the original task, frozen contracts or evidence. A failed
initial specification may retry in the same running process and audit lineage.
A compiler timeout/UNKNOWN never establishes impossible requirements. No new
proof timeout or verification resource limit is added.

### Enforce acceptance outside the actor

A plugin cannot force an unrestricted outer coding agent to use it. The trusted
harness must import `inspectAdmission` from `admission.js`, provide the registered
manifest, full protected audit rows, receipt, exact generated code and host API,
and reject a missing/mismatched/stale receipt. It checks the current task/contract/
coverage binding and artifact hashes. **Do not accept an actor-supplied audit** or
truncate it to an older version. It returns verified-core-only status, never
whole-task acceptance. The harness must separately run host integration tests,
review unresolved obligations and grade observable behavior.

Direct `vera-build`, ordinary verify calls and skipped workflows do not pass
this admission gate. They remain valid ordinary language operations, but are not
substitutes for governed task acceptance. This repository supplies the gate;
external Codex/benchmark runners must wire it in and protect configuration/state.
It has not retroactively changed any prior benchmark or installed profile.
Existing 0.1 configurations must migrate to the explicit trusted manifest before
using this companion version. Ordinary six-tool VERA plugins are unaffected.
