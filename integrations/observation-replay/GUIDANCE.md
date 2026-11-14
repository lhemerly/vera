# Optional plugin guidance: retain empirical behavior while repairing

Use this only when the host has supplied the observation-replay helper/config and
permitted a reference executable. Do not invent access, import hidden tests, or
run untrusted programs outside the authorized sandbox.

1. Before editing a failing behavior, use `check` to capture the permitted
   plain-CLI reference case and immediately baseline the current candidate. Keep
   the exact triggering argv, stdin and fixture files, not a simplified substitute.
   Inspect the returned `replays` separately from reference observation status;
   `recorded` does not mean the candidate matched. A missing candidate is incomplete,
   not a pass. `observe` remains reference-only when that is what you need.
   Do not write the expected answer yourself.
2. Inspect `recorded`, `unstable`, `incomplete`, `rejected` or `unsupported` status.
   Two matching runs are sampled repeatability, not proof. Keep failed/gap records.
3. After changing/building the candidate, replay **the full existing corpus**, not
   just the new feature's cases. Read byte diffs and repair regressions before
   repeatedly adding more unrelated behavior. Historical green on an older
   artifact is not current green.
4. Where meaningful and available, choose an ordinary successful input-to-output
   workflow from the original requirements, including its real dependency boundary,
   then investigate rejection/boundary scenarios. Help/version or mock-only success
   does not establish that workflow. Record missing dependencies and unsupported
   scenarios honestly; do not bypass them or invent
   success, add quotas, or mistake repeated easy cases for broad coverage.
   Associate cases with original obligation IDs for navigation, without claiming
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
