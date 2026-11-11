#!/usr/bin/env python3
"""Compare native semantic admission/layout/costs with the Python oracle.

The corpus is extracted statically by check_rust_frontend; test modules never
execute. Native invocations have an empty PATH, excluding a Python fallback.
Run after `cargo build --locked -p vera-compiler --bin vera`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

from check_rust_frontend import ROOT, Case, canonical, collect

from vera.errors import VeraError
from vera.parser import parse_source
from vera.semantic import analyze

# These three normalized programs read state while computing a fold's returned
# old-state key. Python's legacy def_kind loses that dependency across the fold;
# native lowering conservatively retains it. Pin exact AST identities, rather
# than accepting arbitrary tier differences or silently excluding these cases.
EXPECTED_TIER_UPGRADES = {
    "ae632127c32a26e0bec6dd4e36b66c5699f6b8f557a2c1442e260fcf8c3140b9": "change",
    "67ba3d28ada12cff43d39d3a9a89a2a5208f8bc742b9503cfd72cf3a577614af": "go",
    "ae440bcb74c9b50887c75279a1e1777f893815af2162d111c0f345b3e93b204a": "go",
}


def program_identity(module) -> str:
    normalized = json.dumps(canonical(module), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(normalized.encode()).hexdigest()


def transition_metadata(program) -> list[dict]:
    return [
        {
            "name": name,
            "tier": transition.tier,
            "wcet_bound": transition.wcet_bound,
            "fold_depth": transition.fold_depth,
            "max_fold_product": transition.max_fold_product,
            # Occupancy-only exists probes are implicit in the native footprint
            # representation; compare explicit column-access counts.
            "reads": sum(read.column is not None for read in transition.reads),
            "writes": len(transition.writes),
        }
        for name, transition in program.transitions.items()
    ]


def table_metadata(program) -> list[dict]:
    return [
        {
            "name": name,
            "capacity": table.layout.capacity,
            "bytes": table.layout.size,
            "columns": [
                {
                    "name": column.name,
                    "type": str(column.type),
                    "tracked_sum": column.tracked_sum,
                    "unique": column.unique,
                }
                for column in table.columns.values()
            ],
        }
        for name, table in program.tables.items()
    ]


def check_case(
    binary: Path, case: Case, timeout: float
) -> tuple[str, str | None, set[str]]:
    module = None
    program = None
    try:
        module = parse_source(case.source)
    except VeraError:
        outcome = "syntax_rejected"
    else:
        try:
            program = analyze(module)
            outcome = "semantic_rejected" if program.diags.has_errors() else "accepted"
        except VeraError:
            outcome = "semantic_rejected"
    try:
        process = subprocess.run(
            [str(binary), "check", "-", "--json"],
            input=case.source,
            check=False,
            text=True,
            capture_output=True,
            timeout=timeout,
            env={**os.environ, "PATH": ""},
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return outcome, f"could not run native analyzer: {error}", set()
    if outcome != "accepted":
        if process.returncode == 0:
            return outcome, "native analyzer admitted source rejected by Python", set()
        if process.stdout.strip():
            return outcome, "rejected source produced stdout", set()
        if not re.search(r"\bD_[A-Z_]+\b", process.stderr):
            return (
                outcome,
                "native rejection lacks a diagnostic: " + process.stderr.strip(),
                set(),
            )
        return outcome, None, set()
    if process.returncode != 0:
        return outcome, "native rejected valid source: " + process.stderr.strip(), set()
    if process.stderr.strip():
        return (
            outcome,
            "native accepted source produced stderr: " + process.stderr.strip(),
            set(),
        )
    try:
        actual = json.loads(process.stdout)
    except json.JSONDecodeError as error:
        return outcome, f"native stdout is not JSON: {error}", set()
    if actual.get("ok") is not True:
        return outcome, "native success JSON does not report ok=true", set()
    expected_tables = table_metadata(program)
    actual_tables = actual.get("tables")
    if not isinstance(actual_tables, list):
        return outcome, "native success JSON lacks table metadata", set()
    # IR handle types use dense table identities; translate for human schema
    # comparison only after comparing the declaration order and names.
    names = [table.get("name") for table in actual_tables]
    if names != [table["name"] for table in expected_tables]:
        return outcome, f"table identities/order differ: {names!r}", set()
    for table in actual_tables:
        for column in table.get("columns", []):
            ty = column.get("type", "")
            match = re.fullmatch(r"id64<table#(\d+)>", ty)
            if match:
                index = int(match[1])
                if index >= len(names):
                    return outcome, f"invalid resolved table identity {ty}", set()
                column["type"] = f"id64<{names[index]}>"
    if actual_tables != expected_tables:
        return (
            outcome,
            f"table layouts/types differ: expected {expected_tables!r}, got {actual_tables!r}",
            set(),
        )
    for field, expected in (
        ("invariants", len(program.invariants)),
        ("projections", len(program.projections)),
    ):
        if actual.get(field) != expected:
            return outcome, f"{field} count differs", set()
    actual_transitions = actual.get("transitions")
    expected_transitions = transition_metadata(program)
    if not isinstance(actual_transitions, list) or len(actual_transitions) != len(
        expected_transitions
    ):
        return outcome, "transition metadata count differs", set()
    identity = program_identity(module)
    observed_upgrades = set()
    for expected, native in zip(expected_transitions, actual_transitions):
        if native != expected:
            upgraded = {**expected, "tier": 2}
            if (
                expected["tier"] == 1
                and EXPECTED_TIER_UPGRADES.get(identity) == expected["name"]
                and native == upgraded
            ):
                observed_upgrades.add(identity)
                continue
            return (
                outcome,
                f"transition metadata differs: expected {expected!r}, got {native!r}",
                observed_upgrades,
            )
    return outcome, None, observed_upgrades


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/vera")
    parser.add_argument("--timeout", type=float, default=10)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"native compiler not found: {binary}")
    cases = collect()
    if len(cases) < 148:
        parser.error(f"corpus shrank below its 148-source baseline: {len(cases)}")
    counts = Counter()
    upgrades = set()
    for case in cases:
        outcome, mismatch, observed = check_case(binary, case, args.timeout)
        counts[outcome] += 1
        upgrades.update(observed)
        if mismatch:
            counts["mismatched"] += 1
            print(f"MISMATCH {case.origin}: {mismatch}", file=sys.stderr)
    missing = EXPECTED_TIER_UPGRADES.keys() - upgrades
    if missing:
        counts["mismatched"] += len(missing)
        print(
            f"MISMATCH expected conservative tier upgrades not observed: {sorted(missing)}",
            file=sys.stderr,
        )
    print(
        f"Rust/Python semantic conformance: {len(cases)} sources; "
        f"accepted={counts['accepted']}, semantic_rejected={counts['semantic_rejected']}, "
        f"syntax_rejected={counts['syntax_rejected']}, "
        f"documented_tier_upgrades={len(upgrades)}, mismatched={counts['mismatched']} "
        "(native PATH is empty; no cases skipped)"
    )
    return 1 if counts["mismatched"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
