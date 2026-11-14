"""Trusted stopping governance, not a semantic-completeness or integration oracle.

The host supplies an immutable snapshot callback; proposals are untrusted claims.
A ready result only permits independent build/consumer audit, never grading directly.
"""
import hashlib
import json
import threading
import time
from pathlib import Path


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


class Reviewer:
    def __init__(self, *, run_id, task_hash, deadline_ms, directory, snapshot, clock=None, cancelled=None):
        if not isinstance(deadline_ms, int) or deadline_ms <= 0:
            raise ValueError("Original absolute deadline required")
        self.run_id, self.task_hash, self.deadline_ms = run_id, task_hash, deadline_ms
        self.directory = Path(directory)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.snapshot = snapshot
        self.clock = clock or (lambda: int(time.time() * 1000))
        self.cancelled = cancelled or (lambda: False)
        self.lock = threading.Lock()
        self.index = 0

    def review(self, request):
        if not self.lock.acquire(blocking=False):
            raise RuntimeError("Completion review already in progress")
        try:
            if not isinstance(request, dict):
                raise ValueError("Review request must be an object")
            # Host configuration is authoritative; never adopt identity/deadline from request.
            # Stop before capture if the original clock/cancellation already ended.
            stop_before_capture = self.cancelled() or self.clock() >= self.deadline_ms
            snap = (dict(files={}, artifact_digest=hashlib.sha256(b'').hexdigest(),
                         snapshot_unavailable_reason='Deadline/cancellation prevented a fresh snapshot')
                    if stop_before_capture else self.snapshot())
            if not isinstance(snap, dict) or not isinstance(snap.get("files"), dict):
                raise RuntimeError("Trusted artifact snapshot unavailable")
            artifact = snap.get("artifact_digest")
            if not isinstance(artifact, str) or len(artifact) != 64:
                raise RuntimeError("Trusted artifact digest unavailable")
            evidence = ([{"id": "snapshot-unavailable", "provenance": snap['snapshot_unavailable_reason']}]
                        if snap.get('snapshot_unavailable_reason') else
                        [{"id": "committed-artifact", "provenance": "Host-captured immutable HEAD archive sha256=" + artifact}])
            if snap.get("audit_digest"):
                evidence.append({"id": "protected-authority-audit", "provenance": "Host-captured protected audit sha256=" + snap["audit_digest"]})
            out = dict(run_id=self.run_id, task_hash=self.task_hash, deadline_ms=self.deadline_ms,
                       artifact_digest=artifact, completion_scope="accepted_stop_not_task_correctness",
                       decision="continue", summary="Remaining work requires continuation.", missing=[], evidence=evidence)
            if self.cancelled():
                out.update(decision="cancelled", summary="Host cancellation observed; artifacts preserved.")
            elif self.clock() >= self.deadline_ms:
                out.update(decision="time_expired", summary="Original authorized deadline elapsed; incomplete work remains unproved.")
            else:
                self._assess(request.get("proposal"), snap, out)
            self.index += 1
            record = dict(request_claims=request, trusted_snapshot=snap, response=out, reviewed_at_ms=self.clock())
            target = self.directory / ("review-%04d.json" % self.index)
            # Fail closed if durable review cannot be saved; never return an unrecorded acceptance.
            with target.open("x") as f:
                json.dump(record, f, indent=2)
                f.write("\n")
            return out
        finally:
            self.lock.release()

    def _assess(self, proposal, snap, out):
        def gap(id, requirement, reason, action):
            out["missing"].append(dict(id=id, requirement=requirement, reason=reason, next_action=action))
        if not isinstance(proposal, dict):
            gap("completion-report", "Report remaining original task and required treatment obligations", "No structured proposal was supplied.", "Use vera_completion_propose honestly, then continue unresolved work or identify a concrete blocker.")
            return
        status = proposal.get("status")
        if status not in ("ready", "partial", "blocked"):
            raise ValueError("Invalid completion proposal status")
        remaining = proposal.get("remaining_obligation_ids")
        if not isinstance(remaining, list) or not all(isinstance(x, str) and x for x in remaining):
            raise ValueError("Remaining obligation IDs must be strings")
        blocker = proposal.get("blocker")
        if not isinstance(blocker, dict) or blocker.get("kind") not in ("none", "inaccessible", "unsafe", "stalled") or not isinstance(blocker.get("detail"), str):
            raise ValueError("Invalid declared blocker")
        if status == "blocked" and blocker["kind"] != "none" and blocker["detail"].strip():
            out.update(decision="blocked", summary="Stopped on an actor-declared blocker; its truth and task correctness are NOT verified.", blocker=blocker)
            for id in remaining:
                gap(id, "Original obligation remains unresolved", "Actor explicitly reports unresolved work.", "Retain unknowns for independent review; do not manufacture behavior or a core.")
            return
        admission = snap.get("admission", {})
        if not admission.get("ok") or not admission.get("source_present"):
            gap("verified-core", "Use the current independently admitted core in committed source", admission.get("error", "Current protected admission/source match missing."), "Obtain or repair the admitted core through the frozen-contract workflow; integrate it on a meaningful path.")
        else:
            out["evidence"].append({"id": "core-admission", "provenance": "Protected admission and committed contract hash match; host consumption still requires independent audit."})
        for field, id, requirement in (("core_artifact_paths", "core-artifacts", "Identify committed core integration artifacts"), ("adapter_artifact_paths", "adapter", "Integrate a compatible adapter on a real format boundary")):
            paths = proposal.get(field)
            if not isinstance(paths, list) or not all(isinstance(x, str) for x in paths):
                raise ValueError("Invalid artifact path claims")
            missing = [x for x in paths if x not in snap["files"]]
            if not paths or missing:
                gap(id, requirement, "No committed artifact paths supplied." if not paths else "Declared paths absent from immutable archive: " + ", ".join(missing), "Implement and commit the required real integration, report exact archive-relative paths, and test that outputs are consumed.")
        tests = proposal.get("test_evidence")
        if not isinstance(tests, list) or not all(isinstance(t, dict) and isinstance(t.get("command"), str) and isinstance(t.get("result"), str) for t in tests):
            raise ValueError("Invalid test evidence claims")
        if not tests or any(not t["command"].strip() or not t["result"].strip() for t in tests):
            gap("host-tests", "Supply reproducible host/core/adapter integration checks", "No usable test commands/results were declared.", "Run meaningful positive/negative and consumption checks, report commands and actual observed results; these claims will be independently checked.")
        for id in remaining:
            gap(id, "Original task obligation remains unresolved", "Actor explicitly reports unresolved work.", "Continue this obligation within the existing deadline, or honestly report the concrete inaccessible/unsafe/stalled blocker.")
        if status != "ready":
            gap("partial-task", "Finish feasible remaining task and treatment work", "Actor proposal is partial, not ready for independent audit.", "Continue the same task/session; do not stop merely because one verified slice works.")
        if not out["missing"]:
            out.update(decision="ready_for_audit", summary="Candidate artifacts accepted for independent audit only. Declared adapter use, tests, host behavior and completeness remain unverified; do not grade yet.")
