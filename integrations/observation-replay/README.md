# Empirical observation/replay ledger

A small, standard-library Python helper for AI self-repair around a verified core.
It records permitted **plain CLI** reference observations and replays them against
candidate artifacts. It does not change VERA semantics or synthesize contracts.
No model/provider is invoked.

## Trust and execution boundary

This is an **actor-owned empirical aid**, not a protected authority audit, sandbox,
benchmark gate, or proof of provenance. Every record is labeled
`actor_reported_reference_observation_not_protected_authority`. Actors with write
access can alter files and configuration; hashes are self-consistency/artifact
binding, not adversarial authenticity. Never promote a matching replay to
whole-task correctness or obligation completeness.

Run it only inside the task's **existing authorized isolated environment** with
allowed executables. Do not run untrusted candidate programs on the user's host.
The caller supplies network/filesystem isolation and resource controls. This
helper does not grant new permission, use sudo, invoke a shell, choose an arbitrary
executable from a model case request, or inspect binaries. Subprocesses retain the
current user privileges. Reference access must already be allowed by the task.
Do not ingest hidden tests/solutions or execute unrelated external commands.

The host supplies a config, for example:

```json
{
  "directory": "/task/empirical-ledger",
  "process_timeout_seconds": 30,
  "reference": {"command": ["/reference/approved-cli"], "runtime_provenance": "allowed task reference version"},
  "candidate": {"command": ["/workspace/candidate"], "artifact_paths": ["/workspace/source.rs"], "runtime_provenance": "current task build"}
}
```

Commands are absolute executable argv arrays. Script arguments that are absolute
files are hashed too; additional declared `artifact_paths` bind source/dependency
files. These hashes do not cover the whole environment. The configured process
timeout classifies observations as incomplete; it is not a VERA validation limit.
The task may configure it appropriately. Fixed subprocess environment is PATH,
HOME, LC_ALL=C, TZ=UTC; arbitrary inherited credentials/loader environment are not
passed. Cases requiring other environment semantics are outside this version.

## Record, replay, inspect

```sh
printf '%s' '{"argv":["--flag"],"stdin_b64":"","files":[],"output_paths":[],"obligation_ids":["original-obligation"],"interaction":"plain_cli"}' \
  | python3 replay.py --config /task/replay-config.json observe
python3 replay.py --config /task/replay-config.json replay
python3 replay.py --config /task/replay-config.json replay CASE_ID
python3 replay.py --config /task/replay-config.json list
```

`replay` without IDs includes **all** recorded cases, including unsupported and
failed observations. There is no expected-output input. Fixture files use
`{path,data_b64}`; paths are nontraversing relative paths. Output paths select
regular-file byte observations; missing files are represented explicitly, while
symlinks/outside-root targets/directories are unsupported. Unselected filesystem
effects are not compared.

Each invocation gets fresh fixture contents at the same serialized working path.
The helper records two reference runs, preserving raw stdout/stderr bytes as
base64, exit status (negative codes retain signal termination), selected files,
and executable provenance. Nonzero exits are valid observed rejection behavior.
Matching repeats establish observed repeatability, not universal determinism.
Different repeats are `unstable`; timeouts/infrastructure/unsupported output are
`incomplete`. No whitespace/path/timestamp normalization silently hides differences.

A replay preserves every match, mismatch, incomplete run and raw field diff in
append-only-by-convention history. Current candidate and reference identities are
checked; changed artifacts cannot inherit an old green result. `list` reports
`current_match` separately from historical status. Source/artifact changes during
replay invalidate the apparent match. Historical failures are never silently
dropped. Obligation IDs are navigation annotations only, not coverage percentages.

## Deliberate limits

Socket peers, interactive sessions, background services, and cross-invocation
stateful sequences are **unsupported**, not simulated by guessed fidelity. Set
`interaction` accordingly to retain that gap without executing or passing it.
Plain-process outcomes may still hide unobserved external behavior; isolated task
runners must enforce their actual boundary. Descendant process-group cleanup is
best effort and is not a substitute for a container lifecycle. Large outputs need
the host's memory/disk controls. Never treat hash records as protected evidence.

## Zero-provider tests

```sh
python3 -m unittest -v test_replay.py
```

Tests cover exact bytes/nonzero exits/file effects; stale candidate and reference;
regression after a formerly passing case; attempted supplied expectations;
unsupported protocols; nondeterminism; partial timeout output; fresh fixtures;
path escapes/symlink output; changed candidate during replay; and literal argv
(no shell interpolation). No external reference, hidden tests, or paid inference.

For an execute-only oracle, the host may configure
`reference.artifact_metadata["/reference/approved-cli"]` as
`{"sha256":"64 lowercase hexadecimal digits","provenance":"host pinned artifact"}`.
The helper then **does not read or chmod the oracle**, and explicitly records
`provided_not_recomputed_by_actor`. The host must pin that executable read-only
and keep supplied metadata accurate. This is still not independently authenticated
provenance inside an actor-owned ledger. No permission relaxation is appropriate.
Candidate artifact absence is recorded as incomplete; reference cases may be
captured before the candidate exists.

Records are atomically saved before execution, after each individual observation,
and before/after each case replay. An interruption leaves `pending` work, never a
match; already completed cases remain available. A matching case is promoted only
after its post-execution artifact identity check. No whole-corpus completion is
inferred from a partial run. Atomic replacement is process-crash resilience, not a
power-loss/fsync durability guarantee or tamper protection.

## Capture a failure before repairing it

`check` accepts the same stdin request as `observe`, records the reference, and
immediately replays that exact case against the current candidate. Its response
is the observation record with the baseline in `replays`; the top-level `status`
remains the reference observation status. `recorded` is not a candidate pass.
A missing candidate produces an incomplete baseline; an unsupported/unstable
reference cannot produce a matching baseline. Existing `observe` behavior is
unchanged. Each phase retains the normal incremental journal, so interruption
leaves observations or a pending baseline rather than inventing a result.

Use `check` before editing a mismatch, then `replay CASE_ID` after rebuilding and
replay the full corpus after broader changes. Preserve the exact triggering
fixture, not a similar example. Historical failure evidence stays attached to
its artifact identity even after a later repair matches. This closes an evidence
retention gap; it is not evidence of improved benchmark scores or completeness.
The two phases use existing individual locks, not an atomic candidate snapshot;
concurrent external builds remain subject to the ordinary identity checks.
