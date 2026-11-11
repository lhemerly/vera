"""Boundary architecture: projections and the State & Affordance Frame.

Implements Section 9.1: a projection evaluates ``allow_if`` and ``expose`` in
O(1) against the current state, evaluates each ``enable_intents`` guard, and
attaches exact parameter interval bounds derived from the transition's
``require`` clauses.  The result is a deterministic, strongly-typed frame that
any shell (native UI, CLI, or autonomous agent) can render without a single
line of application presentation code.
"""

from dataclasses import dataclass, field
from typing import Dict, List, Optional

from . import ast_nodes as A
from .errors import D_PROJECTION
from .runtime import Revert, State, Transaction
from .semantic import Program


def _json_value(value):
    if isinstance(value, bytes):
        return "0x" + value.hex()
    if isinstance(value, dict):
        return {k: _json_value(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_value(v) for v in value]
    return value


@dataclass
class Affordance:
    transition: str
    enabled: bool
    bounds: Dict[str, Dict[str, object]] = field(default_factory=dict)
    reason: str = ""

    def to_json(self) -> dict:
        return {
            "transition": self.transition,
            "enabled": self.enabled,
            "bounds": _json_value(self.bounds),
            "reason": self.reason,
        }


@dataclass
class AffordanceFrame:
    projection: str
    allowed: bool
    state_slice: Dict[str, object] = field(default_factory=dict)
    affordances: List[Affordance] = field(default_factory=list)
    reason: str = ""

    def to_json(self) -> dict:
        return {
            "projection": self.projection,
            "allowed": self.allowed,
            "reason": self.reason,
            "state_slice": _json_value(self.state_slice),
            "affordances": [a.to_json() for a in self.affordances],
        }


def _eval(
    prog: Program, state: State, params: Dict[str, object], expr: A.Expr
) -> object:
    tx = Transaction(prog, state, "_project", params)
    return tx.value_of(expr, "post")


def build_affordance_frame(
    prog: Program, state: State, projection_name: str, params: Dict[str, object]
) -> AffordanceFrame:
    proj = prog.projections.get(projection_name)
    if proj is None:
        raise Revert(D_PROJECTION)
    decl = proj.decl
    if set(params) != set(proj.param_types):
        raise Revert("ERR_INVALID_PARAMS")
    validator = Transaction(prog, state, "_project", {})
    try:
        params = {
            name: validator._coerce(ctype, params[name])
            for name, ctype in proj.param_types.items()
        }
    except Revert:
        raise Revert("ERR_INVALID_PARAMS") from None
    frame = AffordanceFrame(projection=projection_name, allowed=False)

    # allow_if
    if decl.allow_if is not None:
        try:
            allowed = _eval(prog, state, params, decl.allow_if)
        except Revert:
            allowed = False
        if not allowed:
            frame.reason = "allow_if evaluated to false"
            return frame
    frame.allowed = True

    # expose slices
    for sl in decl.expose:
        t = state.tables.get(sl.table)
        if t is None:
            continue
        if sl.row is None:
            values = []
            for slot in range(t.capacity):
                if t.occ[slot]:
                    key = (t.gen[slot] << t.key_bits) | slot
                    values.append({"id": key, sl.column: t.columns[sl.column][slot]})
            frame.state_slice[f"{sl.table}.{sl.column}"] = values
        else:
            row = _eval(prog, state, params, sl.row)
            slot = t.slot_of(int(row))
            key = (t.gen[slot] << t.key_bits) | slot
            live = t.valid(row)
            value = t.columns[sl.column][slot] if live else None
            frame.state_slice[f"{sl.table}.{sl.column}[{row}]"] = {
                "id": row,
                "value": value,
                "live": live,
            }

    # enable_intents + parameter bounds
    for ig in decl.enable_intents:
        try:
            enabled = _eval(prog, state, params, ig.condition)
        except Revert:
            enabled = False
        bounds: Dict[str, Dict[str, object]] = {}
        if enabled and ig.transition in prog.transitions:
            bounds = _derive_bounds(prog, state, params, ig.transition)
        frame.affordances.append(Affordance(ig.transition, bool(enabled), bounds))
    return frame


def _derive_bounds(
    prog: Program, state: State, params: Dict[str, object], transition: str
) -> Dict[str, Dict[str, object]]:
    ti = prog.transitions[transition]
    bound: Dict[str, Dict[str, object]] = {p.name: {} for p in ti.decl.params}

    def note(pname: str, key: str, value: object) -> None:
        if pname in bound:
            bound[pname][key] = value

    for _, expr in ti.decl.requires:
        _collect_bound(prog, state, params, expr, bound)
    return bound


def _collect_bound(
    prog: Program,
    state: State,
    params: Dict[str, object],
    expr: A.Expr,
    out: Dict[str, Dict[str, object]],
) -> None:
    """Extract simple parameter interval bounds from a require clause."""
    if not isinstance(expr, A.Binary):
        return
    op = expr.op
    if op == "and":
        _collect_bound(prog, state, params, expr.left, out)
        _collect_bound(prog, state, params, expr.right, out)
        return
    if op not in ("ge", "le", "gt", "lt", "eq"):
        return
    left, right = expr.left, expr.right
    # form: op(Var(param), expr)
    if isinstance(left, A.Var) and not _contains_var(right, left.name):
        pname = left.name
        if pname in out:
            try:
                val = _eval(prog, state, params, right)
            except Revert:
                return
            if op == "ge":
                out[pname]["min"] = max(out[pname].get("min", val), val)
            elif op == "gt":
                out[pname]["min"] = max(out[pname].get("min", val + 1), val + 1)
            elif op == "le":
                out[pname]["max"] = min(out[pname].get("max", val), val)
            elif op == "lt":
                out[pname]["max"] = min(out[pname].get("max", val - 1), val - 1)
            elif op == "eq":
                out[pname]["exact"] = val
        return
    # form: op(expr, Var(param))
    if isinstance(right, A.Var) and not _contains_var(left, right.name):
        pname = right.name
        if pname in out:
            try:
                val = _eval(prog, state, params, left)
            except Revert:
                return
            if op == "ge":
                out[pname]["max"] = min(out[pname].get("max", val), val)
            elif op == "gt":
                out[pname]["max"] = min(out[pname].get("max", val - 1), val - 1)
            elif op == "le":
                out[pname]["min"] = max(out[pname].get("min", val), val)
            elif op == "lt":
                out[pname]["min"] = max(out[pname].get("min", val + 1), val + 1)
            elif op == "eq":
                out[pname]["exact"] = val


def _contains_var(expr: A.Expr, name: str) -> bool:
    if isinstance(expr, A.Var):
        return expr.name == name
    if isinstance(expr, A.Unary):
        return _contains_var(expr.operand, name)
    if isinstance(expr, A.Binary):
        return _contains_var(expr.left, name) or _contains_var(expr.right, name)
    if isinstance(expr, A.Select):
        return (
            _contains_var(expr.cond, name)
            or _contains_var(expr.a, name)
            or _contains_var(expr.b, name)
        )
    if isinstance(expr, A.TableAccess):
        return _contains_var(expr.key, name)
    if isinstance(expr, A.TableGet):
        return _contains_var(expr.key, name)
    if isinstance(expr, A.KeyOf):
        return _contains_var(expr.expr, name)
    return False
