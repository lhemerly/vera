#!/usr/bin/env python3
"""Compare the native parser with the Python oracle without executing test files.

Run after ``cargo build --locked -p vera-compiler --bin vera``::

    python tools/check_rust_frontend.py --binary target/debug/vera

Python is a development dependency for this migration check only. Every native
invocation has an empty PATH, so the compiler cannot fall back to Python.
"""

from __future__ import annotations

import argparse
import ast
from dataclasses import dataclass, fields, is_dataclass
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from vera.errors import VeraError  # noqa: E402
from vera.parser import parse_source  # noqa: E402

UNKNOWN = object()
SOURCE_START = re.compile(
    r"^\s*(?:module|table|invariant|effect|projection|transition)\s"
)


@dataclass
class Case:
    origin: str
    source: str


def canonical(value: Any) -> Any:
    """Keep exact source values and omit source locations from AST comparisons."""
    if is_dataclass(value):
        return {
            "node": type(value).__name__,
            **{
                field.name: canonical(getattr(value, field.name))
                for field in fields(value)
                if field.name not in ("line", "col")
            },
        }
    if isinstance(value, Decimal):
        sign, digits, exponent = value.as_tuple()
        return {
            "decimal": {
                "negative": bool(sign),
                "digits": "".join(map(str, digits)),
                "scale": -exponent,
            }
        }
    if type(value) is int:
        return {"integer": str(value)}
    if isinstance(value, (list, tuple)):
        return [canonical(item) for item in value]
    if value is None or type(value) in (str, bool):
        return value
    raise TypeError(f"unsupported AST value {type(value).__name__}")


class StaticCorpus:
    """A deliberately small AST evaluator, never eval(), exec(), or imports.

    Only primitive data, string builders, finite literal loops/parametrizations,
    and helpers consisting of one return expression are interpreted. Unknown
    constructs remain unknown; imports, fixtures and test functions never run.
    """

    def __init__(self):
        self.cases: list[Case] = []
        self.seen: set[str] = set()
        self.helpers: dict[str, ast.FunctionDef] = {}

    def add(self, source: str, origin: str) -> None:
        digest = hashlib.sha256(source.encode()).hexdigest()
        if digest not in self.seen:
            self.seen.add(digest)
            self.cases.append(Case(origin, source))

    @staticmethod
    def bind(target: ast.AST, value: Any, env: dict[str, Any]) -> None:
        if isinstance(target, ast.Name):
            if value is UNKNOWN:
                env.pop(target.id, None)
            else:
                env[target.id] = value
        elif isinstance(target, (ast.Tuple, ast.List)) and isinstance(
            value, (tuple, list)
        ):
            if len(target.elts) == len(value):
                for name, item in zip(target.elts, value):
                    StaticCorpus.bind(name, item, env)

    def value(self, node: ast.AST | None, env: dict[str, Any], depth: int = 0) -> Any:
        if node is None or depth > 24:
            return UNKNOWN
        resolve = lambda child, scope=env: self.value(child, scope, depth + 1)
        try:
            if isinstance(node, ast.Constant):
                return (
                    node.value
                    if type(node.value) in (str, int, bool, type(None))
                    else UNKNOWN
                )
            if isinstance(node, ast.Name):
                return env.get(node.id, UNKNOWN)
            if isinstance(node, (ast.Tuple, ast.List)):
                result = [resolve(item) for item in node.elts]
                return UNKNOWN if any(item is UNKNOWN for item in result) else result
            if isinstance(node, ast.UnaryOp):
                item = resolve(node.operand)
                if type(item) is int and isinstance(node.op, ast.USub):
                    return -item
            if isinstance(node, ast.Compare) and len(node.ops) == 1:
                left, right = resolve(node.left), resolve(node.comparators[0])
                if left is not UNKNOWN and right is not UNKNOWN:
                    if isinstance(node.ops[0], ast.Eq):
                        return left == right
                    if isinstance(node.ops[0], ast.NotEq):
                        return left != right
            if isinstance(node, ast.IfExp):
                condition = resolve(node.test)
                if type(condition) is bool:
                    return resolve(node.body if condition else node.orelse)
            if isinstance(node, ast.BinOp):
                left, right = resolve(node.left), resolve(node.right)
                if isinstance(node.op, ast.Add):
                    if type(left) is type(right) and type(left) in (str, int, list):
                        return left + right
                if (
                    isinstance(node.op, ast.Mult)
                    and type(right) is int
                    and 0 <= right <= 4096
                ):
                    if type(left) in (str, int):
                        return left * right
                if (
                    isinstance(node.op, ast.Pow)
                    and type(left) is int
                    and type(right) is int
                ):
                    if abs(left) <= 256 and 0 <= right <= 1024:
                        return left**right
            if isinstance(node, ast.JoinedStr):
                parts = []
                for item in node.values:
                    if isinstance(item, ast.Constant):
                        parts.append(item.value)
                        continue
                    value = resolve(item.value)
                    if type(value) not in (str, int, bool):
                        return UNKNOWN
                    if item.conversion == 114:
                        value = repr(value)
                    elif item.conversion == 115:
                        value = str(value)
                    elif item.conversion not in (-1, 97):
                        return UNKNOWN
                    elif item.conversion == 97:
                        value = ascii(value)
                    spec = resolve(item.format_spec) if item.format_spec else ""
                    if not isinstance(spec, str) or len(spec) > 64:
                        return UNKNOWN
                    parts.append(format(value, spec))
                return "".join(parts)
            if (
                isinstance(node, (ast.ListComp, ast.GeneratorExp))
                and len(node.generators) == 1
            ):
                gen = node.generators[0]
                values = resolve(gen.iter)
                if (
                    gen.ifs
                    or gen.is_async
                    or not isinstance(values, (list, tuple))
                    or len(values) > 256
                ):
                    return UNKNOWN
                result = []
                for item in values:
                    scope = dict(env)
                    self.bind(gen.target, item, scope)
                    result.append(resolve(node.elt, scope))
                return UNKNOWN if any(item is UNKNOWN for item in result) else result
            if isinstance(node, ast.Call):
                args = [resolve(arg) for arg in node.args]
                if any(arg is UNKNOWN for arg in args) or node.keywords:
                    return UNKNOWN
                if isinstance(node.func, ast.Attribute):
                    owner = resolve(node.func.value)
                    if (
                        isinstance(owner, str)
                        and node.func.attr == "replace"
                        and 2 <= len(args) <= 3
                    ):
                        return owner.replace(*args)
                    if (
                        isinstance(owner, str)
                        and node.func.attr == "join"
                        and len(args) == 1
                    ):
                        return owner.join(args[0])
                if isinstance(node.func, ast.Name) and node.func.id in self.helpers:
                    helper = self.helpers[node.func.id]
                    body = helper.body
                    if (
                        body
                        and isinstance(body[0], ast.Expr)
                        and isinstance(body[0].value, ast.Constant)
                    ):
                        body = body[1:]
                    names = helper.args.posonlyargs + helper.args.args
                    if (
                        len(body) == 1
                        and isinstance(body[0], ast.Return)
                        and len(names) == len(args)
                    ):
                        scope = dict(env)
                        scope.update((name.arg, arg) for name, arg in zip(names, args))
                        return resolve(body[0].value, scope)
        except (TypeError, ValueError, OverflowError):
            return UNKNOWN
        return UNKNOWN

    def expression(self, node: ast.AST, env: dict[str, Any], origin: str) -> None:
        value = self.value(node, env)
        if isinstance(value, str) and SOURCE_START.match(value):
            self.add(value, f"{origin}:{getattr(node, 'lineno', 1)}")
            return
        # Literal chunks inside an unresolved builder are not complete fixture
        # inputs. For example, f"module M;table T capacity {unknown}..." must
        # not introduce the artificial rejected source "module M;table T capacity ".
        if isinstance(node, ast.JoinedStr) or (
            isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add)
        ):
            return
        for child in ast.iter_child_nodes(node):
            self.expression(child, env, origin)

    def parameterizations(
        self, node: ast.FunctionDef, env: dict[str, Any]
    ) -> list[dict[str, Any]]:
        scopes = [dict(env)]
        for decorator in node.decorator_list:
            if not (
                isinstance(decorator, ast.Call)
                and isinstance(decorator.func, ast.Attribute)
            ):
                continue
            if decorator.func.attr != "parametrize" or len(decorator.args) < 2:
                continue
            names = self.value(decorator.args[0], env)
            values = self.value(decorator.args[1], env)
            if not isinstance(names, str) or not isinstance(values, (tuple, list)):
                continue
            names = [name.strip() for name in names.split(",")]
            expanded = []
            for scope in scopes:
                for row in values:
                    items = [row] if len(names) == 1 else row
                    if isinstance(items, (tuple, list)) and len(items) == len(names):
                        expanded.append({**scope, **dict(zip(names, items))})
            if expanded and len(expanded) <= 256:
                scopes = expanded
        return scopes

    def block(
        self, statements: list[ast.stmt], env: dict[str, Any], origin: str
    ) -> None:
        env = dict(env)
        for statement in statements:
            if isinstance(statement, ast.FunctionDef):
                for scope in self.parameterizations(statement, env):
                    self.block(statement.body, scope, origin)
            elif isinstance(statement, (ast.Assign, ast.AnnAssign)):
                value = self.value(statement.value, env)
                if statement.value is not None:
                    self.expression(statement.value, env, origin)
                targets = (
                    statement.targets
                    if isinstance(statement, ast.Assign)
                    else [statement.target]
                )
                for target in targets:
                    self.bind(target, value, env)
            elif isinstance(statement, ast.For):
                values = self.value(statement.iter, env)
                if isinstance(values, (list, tuple)) and len(values) <= 256:
                    for value in values:
                        scope = dict(env)
                        self.bind(statement.target, value, scope)
                        self.block(statement.body, scope, origin)
                else:
                    self.block(statement.body, env, origin)
                self.block(statement.orelse, env, origin)
            elif isinstance(statement, ast.If):
                self.expression(statement.test, env, origin)
                self.block(statement.body, env, origin)
                self.block(statement.orelse, env, origin)
            elif isinstance(statement, (ast.With, ast.While)):
                self.block(statement.body, env, origin)
            elif isinstance(statement, ast.Try):
                for body in [statement.body, statement.orelse, statement.finalbody] + [
                    h.body for h in statement.handlers
                ]:
                    self.block(body, env, origin)
            else:
                self.expression(statement, env, origin)

    def from_python(self, path: Path) -> None:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        self.helpers = {
            node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)
        }
        self.block(tree.body, {}, str(path.relative_to(ROOT)))


def collect() -> list[Case]:
    corpus = StaticCorpus()
    for folder in (ROOT / "examples", ROOT / "tests" / "rust_frontend_fixtures"):
        for path in sorted(folder.rglob("*.vera")):
            corpus.add(path.read_text(encoding="utf-8"), str(path.relative_to(ROOT)))
    for path in sorted((ROOT / "tests").glob("test_*.py")):
        corpus.from_python(path)
    return corpus.cases


def first_difference(expected: Any, actual: Any, path: str = "$") -> str | None:
    if type(expected) is not type(actual):
        return (
            f"{path}: expected {type(expected).__name__}, got {type(actual).__name__}"
        )
    if isinstance(expected, dict):
        if expected.keys() != actual.keys():
            return f"{path}: expected keys {sorted(expected)}, got {sorted(actual)}"
        for key in expected:
            mismatch = first_difference(expected[key], actual[key], f"{path}.{key}")
            if mismatch:
                return mismatch
    elif isinstance(expected, list):
        if len(expected) != len(actual):
            return f"{path}: expected {len(expected)} items, got {len(actual)}"
        for index, (left, right) in enumerate(zip(expected, actual)):
            mismatch = first_difference(left, right, f"{path}[{index}]")
            if mismatch:
                return mismatch
    elif expected != actual:
        return f"{path}: expected {expected!r}, got {actual!r}"
    return None


def check_case(binary: Path, case: Case) -> tuple[str, str | None]:
    expected = None
    try:
        expected = canonical(parse_source(case.source))
        outcome = "accepted"
    except VeraError as exc:
        if exc.diag.code != "D_PARSE":
            raise
        outcome = "rejected"
    try:
        process = subprocess.run(
            [str(binary), "parse", "-", "--json"],
            input=case.source,
            text=True,
            capture_output=True,
            timeout=10,
            env={**os.environ, "PATH": ""},
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return outcome, f"could not run native parser: {exc}"
    if outcome == "rejected":
        if process.returncode == 0:
            return outcome, "native parser accepted a source rejected by Python"
        if process.stdout.strip():
            return outcome, "native parser emitted stdout for rejected input"
        location = re.search(r"D_PARSE:(\d+):(\d+)", process.stderr)
        if location is None:
            return (
                outcome,
                "native rejection lacks D_PARSE and source position: "
                + process.stderr.strip(),
            )
        line, column = map(int, location.groups())
        lines = case.source.split("\n")
        if not 1 <= line <= len(lines) or not 1 <= column <= len(lines[line - 1]) + 1:
            return (
                outcome,
                f"native rejection has invalid source position {line}:{column}",
            )
        return outcome, None
    if process.returncode != 0:
        return (
            outcome,
            "native parser rejected accepted source: " + process.stderr.strip(),
        )
    if process.stderr.strip():
        return (
            outcome,
            "native parser emitted stderr for accepted input: "
            + process.stderr.strip(),
        )
    try:
        actual = json.loads(process.stdout)
    except json.JSONDecodeError as exc:
        return outcome, f"native stdout is not one JSON document: {exc}"
    return outcome, first_difference(expected, actual)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary", type=Path, default=ROOT / "target" / "debug" / "vera"
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list the statically extracted corpus without running a compiler",
    )
    args = parser.parse_args()
    cases = collect()
    if args.list:
        for case in cases:
            print(case.origin)
        print(f"{len(cases)} unique sources")
        return 0
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(
            f"native compiler not found: {binary}; build with cargo build -p vera-compiler --bin vera"
        )
    counts = {"accepted": 0, "rejected": 0, "mismatched": 0}
    for case in cases:
        outcome, mismatch = check_case(binary, case)
        counts[outcome] += 1
        if mismatch:
            counts["mismatched"] += 1
            print(f"MISMATCH {case.origin}: {mismatch}", file=sys.stderr)
    print(
        f"Rust/Python frontend conformance: {len(cases)} unique sources; "
        f"accepted={counts['accepted']}, rejected={counts['rejected']}, "
        f"mismatched={counts['mismatched']} (native PATH is empty)"
    )
    return 1 if counts["mismatched"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
