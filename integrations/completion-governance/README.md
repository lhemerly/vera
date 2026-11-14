# Completion governance

This host-side gate prevents a known-incomplete or missing-treatment actor final
from automatically ending a research attempt. Pair `Reviewer` with the DSH
`governed-headless.js` driver. The host fixes run ID, task-manifest SHA256 and the
original absolute deadline. Continuations use the same actor session and clock.

`DockerSnapshot` captures immutable Git HEAD and the private authority audit using
fixed host-configured paths. It never executes actor code or extracts the archive.
A changed protected task is an infrastructure error. Source presence and protected
proof admission are checked; adapter paths and reported tests remain **claims**.
The host persists every proposal, snapshot and decision before responding.

`ready_for_audit` is only permission to stop for independent inspection. It is not
proof of meaningful adapter use, host correctness, original-task completeness or
benchmark success. Before grading, independently compare final artifact hashes,
validate admission, rebuild, test real core/adapter consumers and review remaining
obligations. Never treat a source filename, import or self-reported passing test as
integration proof. A declared blocker can stop without proving its truth; preserve
unknowns and do not score an incomplete treatment. This prevents forced invention
and unnecessary spending when investigation stalls.

The fixed host `/completion/review` endpoint accepts the governed driver's proposal
and returns a response bound to run/task/deadline/artifact. Review failures are
infrastructure failures, not accepted completion. No automatic model plans execute,
no new verifier restrictions are added, and no budget is extended. Capture skips expired/cancelled work; every subprocess is bounded by the remaining
original deadline and timeout is infrastructure failure, not a failed proof. Git runs
as the actor UID. Keep outer deadline teardown to stop any container-side exec after
its Docker client is reaped. The operational
container and host endpoint must be protected independently of actor-writable files.

Run tests: `python3 -m unittest -v test_reviewer` in this directory. Native same-agent
continuation tests live in `../dsh-contract-workflow/test/`.
