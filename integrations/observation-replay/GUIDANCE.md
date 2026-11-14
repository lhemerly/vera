# Optional plugin guidance: retain empirical behavior while repairing

Use this only when the host has supplied the observation-replay helper/config and
permitted a reference executable. Do not invent access, import hidden tests, or
run untrusted programs outside the authorized sandbox.

1. Preserve a useful permitted plain-CLI observation as a ledger case, with its
   exact argv, stdin and fixture files. Do not write the expected answer yourself.
2. Inspect `recorded`, `unstable`, `incomplete`, `rejected` or `unsupported` status.
   Two matching runs are sampled repeatability, not proof. Keep failed/gap records.
3. After changing/building the candidate, replay **the full existing corpus**, not
   just the new feature's cases. Read byte diffs and repair regressions before
   repeatedly adding more unrelated behavior. Historical green on an older
   artifact is not current green.
4. Associate cases with original obligation IDs for navigation, without claiming
   that sampled cases exhaust that obligation. Maintain separate proof, host-test,
   known mismatch and unobserved/unsupported categories.
5. Keep socket/stateful scenarios explicitly unsupported here. Use the task's
   already-authorized tools for those observations; do not manufacture a pass or
   silently reduce required behavior to the helper's supported subset.

The actor-owned ledger is a self-repair aid, not the protected contract authority.
Do not replace immutable specifications, admission checks, independent integration
review, or benchmark grading with its self-reported evidence. No score improvement
is promised. The goal is to stop losing previously observed behavior while fixing
new behavior.
