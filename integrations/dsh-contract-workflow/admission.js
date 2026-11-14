import { digest, taskManifest } from "./workflow.js";
/** Trusted-host gate, NOT callable by the model. Read audit from the protected state
 * directory, never an executor-supplied copy. No claim of whole-task correctness. */
export function inspectAdmission({ task, audit, receipt, code, host_api }) {
  const expected = taskManifest(task);
  const events = audit.filter(
    (row) => row.workflow_id === receipt?.workflow_id
  );
  if (!events.length) throw Error("No trusted task audit for receipt");
  let previous = null;
  for (let i = 0; i < events.length; i++) {
    const { digest: hash, ...event } = events[i];
    if (
      event.seq !== i ||
      event.previous !== previous ||
      digest(event) !== hash
    )
      throw Error("Invalid task audit chain");
    previous = hash;
  }
  const created = events[0];
  if (
    created.type !== "created" ||
    created.data.task_hash !== digest(expected) ||
    receipt.task_hash !== digest(expected)
  )
    throw Error("Wrong registered task");
  const versions = events.filter((e) => e.type === "contract_frozen");
  const current = versions.at(-1)?.data.version;
  if (
    !current ||
    receipt.version !== current.number ||
    receipt.contract_hash !== current.contract_hash ||
    receipt.coverage_hash !== digest(current.coverage) ||
    receipt.grounding_hash !== digest(current.grounding??null) ||
    receipt.core_scope_hash !== digest(current.core_scope??null) ||
    digest(receipt.grounding_policy)!==digest(expected.policy)
  )
    throw Error("Stale or unbound coverage/contract");
  const admitted = events.filter((e) => e.type === "artifact_admitted").at(-1);
  if (!admitted || digest(admitted.data) !== digest(receipt))
    throw Error("Receipt is not the latest admitted artifact");
  const disposition = events
    .filter((e) =>
      [
        "execution_finished",
        "petition",
        "contract_frozen",
        "failed",
        "candidate_rejected",
      ].includes(e.type)
    )
    .at(-1);
  if (
    disposition?.type !== "execution_finished" ||
    disposition.data.status !== "admitted" ||
    disposition.seq < admitted.seq
  )
    throw Error(
      "Task admission is pending, failed or requires re-execution after review"
    );
  if (
    receipt.artifact_hash !== digest(code) ||
    receipt.host_api_hash !== digest(host_api)
  )
    throw Error("Artifact differs from admitted artifact");
  return {
    admission_scope: "verified_core_only",
    host_integration: "not_checked",
    coverage_completeness: "not_established",
    task_id: expected.id,
    grounding:structuredClone(current.grounding??null),
    core_scope:structuredClone(current.core_scope??null),
    grounding_policy:expected.policy,
    coverage: structuredClone(current.coverage),
  };
}
