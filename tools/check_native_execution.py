#!/usr/bin/env python3
"""Differential native execution against the development Python reference.

Run after ``cargo build --locked -p vera-compiler --bin vera``::

    python tools/check_native_execution.py --binary target/debug/vera

The native CLI always runs with an empty PATH and --unverified, exercising its
own frontend and interpreter without invoking Python or the proof gate. Python
is used only by this development/CI oracle. Cases are never skipped on failure.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from vera.parser import parse_source
from vera.projection import build_affordance_frame
from vera.runtime import Machine, Revert
from vera.semantic import Program, analyze
from vera.types import primitive, symbol_id

NUMERIC_TYPES = ("u8", "u16", "u32", "u64", "i64", "dec64", "dec128")
ARITHMETIC = ("add", "sub", "mul", "div", "rem", "min", "max")
COMPARISONS = ("eq", "ne", "lt", "le", "gt", "ge")


@dataclass
class Source:
    name: str
    text: str
    path: Path
    program: Program


@dataclass
class Case:
    label: str
    source: Source
    transition: str
    params: dict[str, object]
    cli_params: dict[str, str] = field(default_factory=dict)
    setup: dict | None = None
    projection: bool = False


def source(folder: Path, name: str, text: str) -> Source:
    program = analyze(parse_source(text))
    if program.diags.has_errors():
        raise AssertionError(
            f"oracle fixture {name} is not valid:\n{program.diags.render()}\n{text}"
        )
    path = folder / f"{name}.vera"
    path.write_text(text, encoding="utf-8")
    return Source(name, text, path, program)


def arithmetic_pairs(type_name: str) -> list[tuple[int, int]]:
    ty = primitive(type_name)
    low, high = ty.min_value, ty.max_value
    pairs = [
        (0, 0),
        (1, 0),
        (0, 1),
        (1, 1),
        (2, 1),
        (high, 1),
        (high - 1, 1),
        (high, high),
        (0, high),
        (high, 2),
        (high // 2 + 1, 2),
        (1, 2),
    ]
    if ty.is_signed:
        pairs += [
            (0, -1),
            (-1, 1),
            (1, -1),
            (low, -1),
            (low, 1),
            (low, low),
            (high, -1),
            (-7, 3),
            (7, -3),
            (-7, -3),
        ]
    if ty.is_decimal:
        scale = 10**ty.scale
        pairs += [
            (0, scale),
            (scale, scale),
            (scale, 2 * scale),
            (scale, 3 * scale),
            (scale + 1, 2 * scale),
            (-scale, scale),
            (low, scale),
            (high, scale),
            (-scale, 3 * scale),
            (scale + 1, scale + 1),
            (-scale - 1, 2 * scale),
            (scale, scale - 1),
            (low, -scale),
            (high, -scale),
            (-1, 2),
        ]
    # Every generated input is inside its declared type. Fail rather than skip
    # if an extension accidentally introduces an inadmissible parameter.
    assert all(low <= a <= high and low <= b <= high for a, b in pairs)
    return list(dict.fromkeys(pairs))


def numeric_cases(folder: Path) -> list[Case]:
    cases = []
    for type_name in NUMERIC_TYPES:
        ty = primitive(type_name)
        declarations = []
        for op in ARITHMETIC + COMPARISONS:
            effect = "Predicate" if op in COMPARISONS else "Result"
            declarations.append(f"""transition calc_{op}(a:{type_name},b:{type_name})
reads [] writes [] body {{emit {effect}({op}(a,b));commit;}}""")
        if ty.is_signed:
            declarations.append(f"""transition calc_neg(a:{type_name})
reads [] writes [] body {{emit Result(neg(a));commit;}}""")
        if type_name == "dec128":
            declarations.append("""transition exact_negative() reads [] writes []
body {emit Result(-10000000000.000000000000000001);commit;}""")
        program = source(
            folder,
            f"scalar_{type_name}",
            f"""module Arithmetic;
table Dummy capacity 1 {{key id:id64<Dummy>;col marker:u8;}}
effect Result(value:{type_name});effect Predicate(value:bool);
"""
            + "\n".join(declarations),
        )
        for op in ARITHMETIC + COMPARISONS:
            for a, b in arithmetic_pairs(type_name):
                cases.append(
                    Case(
                        f"{type_name}/{op}/{a}/{b}",
                        program,
                        f"calc_{op}",
                        {"a": a, "b": b},
                    )
                )
        if ty.is_signed:
            scale = 10**ty.scale if ty.is_decimal else 1
            for value in dict.fromkeys(
                [ty.min_value, -1, 0, 1, ty.max_value, -scale, scale]
            ):
                cases.append(
                    Case(f"{type_name}/neg/{value}", program, "calc_neg", {"a": value})
                )
        if type_name == "dec128":
            cases.append(
                Case("dec128/exact-negative-source", program, "exact_negative", {})
            )
    return cases


def boolean_cases(folder: Path) -> list[Case]:
    declarations = [
        f"transition calc_{op}(a:bool,b:bool) reads [] writes [] body {{emit Result({op}(a,b));commit;}}"
        for op in ("and", "or", "xor", "eq", "ne")
    ]
    declarations.append(
        "transition calc_not(a:bool) reads [] writes [] body {emit Result(not(a));commit;}"
    )
    for op, first in (("and", "false"), ("or", "true"), ("xor", "false")):
        declarations.append(
            f"transition eager_{op}() reads [] writes [] body {{emit Result({op}({first},eq(div(1,0),0)));commit;}}"
        )
    declarations += [
        "transition lazy_yes() reads [] writes [] body {emit Number(select(true,7,div(1,0)));commit;}",
        "transition lazy_no() reads [] writes [] body {emit Number(select(false,div(1,0),9));commit;}",
        "transition selected_trap() reads [] writes [] body {emit Number(select(true,div(1,0),9));commit;}",
        "transition reject() reads [] writes [] body {guard(false,ERR_REJECT);commit;}",
        "transition precondition() reads [] writes [] require denied:false;body {commit;}",
    ]
    program = source(
        folder,
        "booleans",
        """module Logic;
table Dummy capacity 1 {key id:id64<Dummy>;col marker:u8;}
effect Result(value:bool);effect Number(value:i64);
"""
        + "\n".join(declarations),
    )
    cases = [
        Case(f"bool/{op}/{a}/{b}", program, f"calc_{op}", {"a": a, "b": b})
        for op in ("and", "or", "xor", "eq", "ne")
        for a in (False, True)
        for b in (False, True)
    ]
    cases += [
        Case(f"bool/not/{a}", program, "calc_not", {"a": a}) for a in (False, True)
    ]
    cases += [
        Case(f"bool/{name}", program, name, {})
        for name in (
            "eager_and",
            "eager_or",
            "eager_xor",
            "lazy_yes",
            "lazy_no",
            "selected_trap",
            "reject",
            "precondition",
        )
    ]
    return cases


def boundary_cases(folder: Path) -> list[Case]:
    program = source(
        folder,
        "boundary",
        """module Boundary;
table Dummy capacity 1 {key id:id64<Dummy>;col marker:u8;}
effect Result(code:bytes32,symbol:sym32,tag:enum{First,Second});
effect Handle(k:id64<Dummy>);
transition identity(code:bytes32,symbol:sym32,tag:enum{First,Second}) reads [] writes []
body {emit Result(code,symbol,tag);commit;}
transition literal() reads [] writes [] body {
emit Result(0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff,"Clamaí 中文",Second);commit;}
transition handle(k:id64<Dummy>) reads [] writes [] body {emit Handle(k);commit;}
transition handle_literal() reads [] writes [] body {emit Handle(id64<Dummy>(-1));commit;}
""",
    )
    values = [
        bytes(32),
        bytes([255]) * 32,
        bytes(range(32)),
        (1).to_bytes(32, "little"),
        ((1 << 255) + 1).to_bytes(32, "little"),
    ]
    symbols = ["Clamai", "Clamaí 中文🦀", "123", "zero", "the same low 64 bits"]
    cases = []
    for index, (code, symbol) in enumerate(zip(values, symbols)):
        cases.append(
            Case(
                f"boundary/identity/{index}",
                program,
                "identity",
                {"code": code, "symbol": symbol_id(symbol), "tag": index % 2},
                {"symbol": symbol, "tag": "Second" if index % 2 else "First"},
            )
        )
    cases.append(Case("boundary/source-literals", program, "literal", {}))
    for key in (0, 1, 1 << 32, (1 << 64) - 1):
        cases.append(Case(f"boundary/handle/{key}", program, "handle", {"k": key}))
    cases.append(Case("boundary/source-handle-cast", program, "handle_literal", {}))
    return cases


def application_cases(folder: Path) -> list[Case]:
    bank = source(
        folder, "bank", (ROOT / "examples/bank.vera").read_text(encoding="utf-8")
    )
    setup = json.loads((ROOT / "examples/bank_setup.json").read_text())
    cases = []
    for amount in (0, 1, 100000000, 500000000, 500000001, 700000000, 700000001, -1):
        cases.append(
            Case(
                f"bank/transfer/{amount}",
                bank,
                "transfer",
                {"from": 32, "to": 33, "amount": amount},
                setup=setup,
            )
        )
    cases += [
        Case(
            "bank/self-transfer",
            bank,
            "transfer",
            {"from": 32, "to": 32, "amount": 1},
            setup=setup,
        ),
        Case(
            "bank/stale-transfer",
            bank,
            "transfer",
            {"from": 96, "to": 33, "amount": 1},
            setup=setup,
        ),
    ]
    cases += [
        Case(f"bank/freeze/{key}", bank, "freeze", {"acct": key}, setup=setup)
        for key in (32, 33, 96)
    ]
    cases += [
        Case(
            f"bank/order/{amount}",
            bank,
            "place_order",
            {"buyer": 32, "amount": amount},
            setup=setup,
        )
        for amount in (1, 500000000, 500000001)
    ]
    cases += [
        Case(
            f"bank/project/{key}",
            bank,
            "AccountView",
            {"from": key},
            setup=setup,
            projection=True,
        )
        for key in (32, 33, 96)
    ]
    counter = source(
        folder, "counter", (ROOT / "examples/counter.vera").read_text(encoding="utf-8")
    )
    setup = json.loads((ROOT / "examples/counter_setup.json").read_text())
    cases += [
        Case(
            f"counter/increase/{delta}",
            counter,
            "increase",
            {"counter": 4, "delta": delta},
            setup=setup,
        )
        for delta in (0, 1, 999999993, 999999994, (1 << 64) - 1)
    ]
    cases += [
        Case(
            f"counter/create/{initial}",
            counter,
            "create",
            {"initial": initial},
            setup=setup,
        )
        for initial in (0, 1, 1000000000, 1000000001, (1 << 64) - 1)
    ]
    cases += [
        Case(
            f"counter/project/{key}",
            counter,
            "CounterView",
            {"counter": key},
            setup=setup,
            projection=True,
        )
        for key in (4, 5, 8, (1 << 64) - 1)
    ]
    return cases


def collision_cases(folder: Path) -> list[Case]:
    program = source(
        folder,
        "id_column",
        """module IdColumn;
table T capacity 2 {key handle:id64<T>;col id:u64;col value:u64;}
effect Result(id:u64,value:u64);
transition echo(k:id64<T>) reads [T[k].id,T[k].value] writes []
require live:table_exists(T,k);
body {let %id:u64=table_get(T,k,id);let %value:u64=table_get(T,k,value);emit Result(%id,%value);commit;}
projection View() {allow_if:true;expose:[T.id,T.value];enable_intents:[];}
""",
    )
    setup = {"tables": {"T": [{"id": 2, "columns": {"id": 99, "value": 7}}]}}
    return [
        Case("id-column/execution", program, "echo", {"k": 2}, setup=setup),
        Case("id-column/projection", program, "View", {}, setup=setup, projection=True),
    ]


def json_value(value: Any) -> Any:
    return "0x" + value.hex() if isinstance(value, bytes) else value


def model(case: Case) -> Machine:
    machine = Machine(case.source.program)
    for table_name, rows in (case.setup or {}).get("tables", {}).items():
        table = machine.state.tables[table_name]
        for row in rows:
            key = row["id"]
            slot = table.slot_of(key)
            table.occ[slot] = 1
            table.gen[slot] = key >> table.key_bits
            values = row.get("columns", row)
            for column in table.columns:
                if column in values:
                    table.columns[column][slot] = values[column]
    machine.recompute_aggregates()
    machine.rebuild_indexes()
    machine.validate_state()
    return machine


def state_json(machine: Machine) -> dict:
    tables = {}
    for name, table in machine.state.tables.items():
        rows = []
        for slot in range(table.capacity):
            if table.occ[slot]:
                columns = {
                    name: json_value(values[slot])
                    for name, values in table.columns.items()
                }
                # Intentional boundary bug fix: the historical Python flat
                # formatter overwrote a row handle when a legal column was
                # named id. Compare actual reference state using the native
                # collision-safe shape, preserving both independent values.
                rows.append(
                    {
                        "id": (table.gen[slot] << table.key_bits) | slot,
                        **({"columns": columns} if "id" in columns else columns),
                    }
                )
        tables[name] = rows
    outbox = []
    for effect, values in machine.state.outbox:
        params = machine.prog.effects[effect].params
        outbox.append(
            {
                "effect": effect,
                "args": {
                    param.name: json_value(value)
                    for param, value in zip(params, values)
                },
            }
        )
    return {"tables": tables, "outbox": outbox}


def expected(case: Case) -> dict:
    machine = model(case)
    if case.projection:
        frame = build_affordance_frame(
            case.source.program, machine.state, case.transition, case.params
        ).to_json()
        # The same deliberate fix applies only to wildcard slices of an id
        # column; ordinary reference frame fields retain their existing shape.
        for slice in case.source.program.projections[case.transition].decl.expose:
            label = f"{slice.table}.{slice.column}"
            if (
                slice.column == "id"
                and slice.row is None
                and label in frame["state_slice"]
            ):
                table = machine.state.tables[slice.table]
                frame["state_slice"][label] = [
                    {
                        "id": (table.gen[slot] << table.key_bits) | slot,
                        "columns": {"id": json_value(table.columns["id"][slot])},
                    }
                    for slot in range(table.capacity)
                    if table.occ[slot]
                ]
        return frame
    before = machine.state.clone()
    try:
        machine.call(case.transition, case.params)
        outcome = {"ok": True, "error": None}
    except Revert as error:
        assert machine.state == before, (
            f"Python oracle failed to roll back {case.label}"
        )
        outcome = {"ok": False, "error": {"code": error.code, "kind": error.kind}}
    return {**outcome, "state": state_json(machine)}


def cli_text(value: object) -> str:
    if type(value) is bool:
        return str(value).lower()
    if isinstance(value, bytes):
        return "0x" + value.hex()
    return str(value)


def difference(expected_value: Any, actual: Any, path: str = "$") -> str | None:
    if type(expected_value) is not type(actual):
        return f"{path}: expected {type(expected_value).__name__} {expected_value!r}, got {type(actual).__name__} {actual!r}"
    if isinstance(expected_value, dict):
        if expected_value.keys() != actual.keys():
            return (
                f"{path}: expected keys {sorted(expected_value)}, got {sorted(actual)}"
            )
        for key in expected_value:
            result = difference(expected_value[key], actual[key], f"{path}.{key}")
            if result:
                return result
    elif isinstance(expected_value, list):
        if len(expected_value) != len(actual):
            return f"{path}: expected {len(expected_value)} entries, got {len(actual)}"
        for index, (left, right) in enumerate(zip(expected_value, actual)):
            result = difference(left, right, f"{path}[{index}]")
            if result:
                return result
    elif expected_value != actual:
        return f"{path}: expected {expected_value!r}, got {actual!r}"
    return None


def check(binary: Path, case: Case, folder: Path) -> tuple[str, str | None]:
    oracle = expected(case)
    command = [
        str(binary),
        "project" if case.projection else "run",
        str(case.source.path),
        "--json",
    ]
    if case.projection:
        command += ["--projection", case.transition]
    else:
        command += ["--unverified", "--transition", case.transition]
    for name, value in case.params.items():
        command += ["--param", f"{name}={case.cli_params.get(name, cli_text(value))}"]
    if case.setup is not None:
        setup = folder / "setup.json"
        setup.write_text(json.dumps(case.setup), encoding="utf-8")
        command += ["--setup", str(setup)]
    try:
        process = subprocess.run(
            command,
            check=False,
            text=True,
            capture_output=True,
            timeout=15,
            env={**os.environ, "PATH": ""},
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return "failed", str(error)
    if process.stderr.strip():
        return "failed", f"native stderr: {process.stderr.strip()}"
    try:
        native = json.loads(process.stdout)
    except json.JSONDecodeError as error:
        return (
            "failed",
            f"native output is not JSON (status {process.returncode}): {error}; {process.stdout[:300]!r}",
        )
    if case.projection:
        outcome = "projection"
        actual = native
        status = 0
    else:
        outcome = "committed" if oracle["ok"] else "reverted"
        error = native.get("error")
        actual = {
            "ok": native.get("ok"),
            "error": None
            if error is None
            else {"code": error.get("code"), "kind": str(error.get("kind")).lower()},
            "state": native.get("state"),
        }
        status = 0 if oracle["ok"] else 1
    if process.returncode != status:
        return outcome, f"expected process status {status}, got {process.returncode}"
    return outcome, difference(oracle, actual)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/vera")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(
            f"native CLI not found at {binary}; run cargo build -p vera-compiler --bin vera"
        )
    counts = Counter()
    with tempfile.TemporaryDirectory(prefix="vera-native-execution-") as temporary:
        folder = Path(temporary)
        cases = (
            numeric_cases(folder)
            + boolean_cases(folder)
            + boundary_cases(folder)
            + application_cases(folder)
            + collision_cases(folder)
        )
        assert 0 < len(cases) < 2000, len(cases)
        for case in cases:
            outcome, mismatch = check(binary, case, folder)
            counts[outcome] += 1
            if mismatch:
                counts["mismatched"] += 1
                print(
                    f"MISMATCH {case.label}: {mismatch}\n  params={case.params!r}",
                    file=sys.stderr,
                )
    print(
        f"Native/Python execution: {len(cases)} cases; committed={counts['committed']}, reverted={counts['reverted']}, projections={counts['projection']}, mismatched={counts['mismatched']} (native PATH is empty)"
    )
    return 1 if counts["mismatched"] or counts["failed"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
