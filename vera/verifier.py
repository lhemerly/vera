"""Counterexample lifting: SMT model -> canonical Diagnostic Counterexample Packet.

Implements Section 10.2: when Z3 returns SAT on Obligation 1 or 2, the model is
projected onto the pre-state tables and input parameters, replayed through the
reference runtime to obtain an SSA execution trace, and lifted into the
token-minimal repair prompt consumed by the synthesis loop.
"""

from typing import Dict, List, Optional, Tuple

import z3

from . import ast_nodes as A
from .runtime import Machine, Revert, State, TableState, Transaction
from .semantic import Program
from .smt_emit import Counterexample, ObligationResult, SmtEncoder, VerificationResult
from .types import I64, ScalarType, slot_bits_for


def _sort_for(ctype: ScalarType):
    return z3.BoolSort() if ctype.is_bool else z3.BitVecSort(ctype.width)


def _const_for(name: str, kind: Tuple):
    if kind[0] == "bool":
        return z3.Bool(name)
    if kind[0] == "bv":
        return z3.BitVec(name, kind[1])
    if kind[0] == "fun":
        return z3.Function(name, z3.BitVecSort(kind[1]), _sort_for_kind(kind[2]))
    raise ValueError(kind)


def _sort_for_kind(kind):
    return z3.BoolSort() if kind == "bool" else z3.BitVecSort(kind)


def _to_python(value: z3.ExprRef, ctype: ScalarType):
    if ctype.is_bool:
        return z3.is_true(value)
    if ctype.kind == "enum":
        return value.as_long()
    return value.as_long()


class ModelProjector:
    def __init__(self, prog: Program, encoder: SmtEncoder, model: z3.ModelRef):
        self.prog = prog
        self.encoder = encoder
        self.model = model

    def param_values(self) -> Dict[str, object]:
        out: Dict[str, object] = {}
        for p in self.encoder.tr.decl.params:
            name = f"p_{p.name}"
            kind = self.encoder.const_kinds.get(name)
            if kind is None:
                continue
            val = self.model.eval(_const_for(name, kind), model_completion=True)
            out[p.name] = _to_python(val, self.encoder.tr.param_types[p.name])
        return out

    def pre_state(self, relevant_only: bool = False) -> Dict[str, List[Dict[str, object]]]:
        out: Dict[str, List[Dict[str, object]]] = {}
        relevant = self._relevant_slots() if relevant_only else None
        for tname, tinfo in self.prog.tables.items():
            sbits = self.encoder.table_slot_bits[tname]
            gen_bits = min(64 - tinfo.layout.key_bits, 32)
            occ_fn = z3.Array(self.encoder.pre_occ[tname],
                              z3.BitVecSort(sbits), z3.BoolSort())
            gen_fn = z3.Function(self.encoder.pre_gen[tname],
                                 z3.BitVecSort(sbits), z3.BitVecSort(gen_bits))
            rows: List[Dict[str, object]] = []
            for slot in range(tinfo.layout.capacity):
                if relevant is not None and slot not in relevant.get(tname, set()):
                    continue
                occ = self.model.eval(z3.Select(occ_fn, z3.BitVecVal(slot, sbits)),
                                      model_completion=True)
                if not z3.is_true(occ):
                    continue
                gen = self.model.eval(gen_fn(z3.BitVecVal(slot, sbits)),
                                      model_completion=True).as_long()
                key = (gen << tinfo.layout.key_bits) | slot
                row: Dict[str, object] = {"id": key}
                for cname, cinfo in tinfo.columns.items():
                    fn = z3.Function(self.encoder.pre_col[tname][cname],
                                     z3.BitVecSort(sbits), _sort_for(cinfo.type))
                    val = self.model.eval(fn(z3.BitVecVal(slot, sbits)),
                                          model_completion=True)
                    row[cname] = _to_python(val, cinfo.type)
                rows.append(row)
            out[tname] = rows
        return out

    def pre_hashes(self) -> Dict[Tuple[str, str], List[Tuple[bool, int]]]:
        out = {}
        for (tname, cname), hkey_name in self.encoder.pre_hkey.items():
            hused_name = self.encoder.pre_hused[(tname, cname)]
            B = self.prog.tables[tname].layout.unique_hashes[cname]
            bb = max(1, B.bit_length() - 1)
            hkey = z3.Array(hkey_name, z3.BitVecSort(bb),
                            z3.BitVecSort(self.encoder.table_slot_bits[tname]))
            hused = z3.Array(hused_name, z3.BitVecSort(bb), z3.BoolSort())
            buckets = []
            for i in range(B):
                used = z3.is_true(self.model.eval(hused[z3.BitVecVal(i, bb)],
                                                  model_completion=True))
                key = self.model.eval(hkey[z3.BitVecVal(i, bb)],
                                      model_completion=True).as_long()
                buckets.append((used, key))
            out[(tname, cname)] = buckets
        return out

    def pre_aggregates(self) -> Dict[str, Dict[str, int]]:
        out: Dict[str, Dict[str, int]] = {}
        for (tname, cname), wide_w in self.encoder.agg_wide.items():
            name = f"{tname}_{cname}_agg_0"
            kind = self.encoder.const_kinds.get(name)
            if kind is None:
                continue
            val = self.model.eval(z3.BitVec(name, kind[1]), model_completion=True)
            raw = val.as_long()
            if raw >= (1 << (kind[1] - 1)):
                raw -= (1 << kind[1])
            out.setdefault(tname, {})[cname] = raw
        return out

    def _relevant_slots(self) -> Dict[str, set]:
        """Slots reachable from the transition's id64 input parameters."""
        slots: Dict[str, set] = {t: set() for t in self.prog.tables}
        params = self.param_values()
        frontier: List[Tuple[str, int]] = []
        for p in self.encoder.tr.decl.params:
            ptype = self.encoder.tr.param_types.get(p.name)
            if ptype is None or ptype.kind != "id64":
                continue
            tname = ptype.key_table
            tinfo = self.prog.tables[tname]
            slot = int(params.get(p.name, 0)) & (tinfo.layout.capacity - 1)
            if slot not in slots[tname]:
                slots[tname].add(slot)
                frontier.append((tname, slot))
        # follow id64 foreign-key references from included rows
        seen = set(frontier)
        while frontier:
            tname, slot = frontier.pop()
            tinfo = self.prog.tables[tname]
            for cname, cinfo in tinfo.columns.items():
                if cinfo.type.kind != "id64":
                    continue
                fn = z3.Function(self.encoder.pre_col[tname][cname],
                                 z3.BitVecSort(self.encoder.table_slot_bits[tname]),
                                 z3.BitVecSort(64))
                key = self.model.eval(fn(z3.BitVecVal(slot, self.encoder.table_slot_bits[tname])),
                                      model_completion=True).as_long()
                target = cinfo.type.key_table
                ttarget = self.prog.tables[target]
                tslot = key & (ttarget.layout.capacity - 1)
                if (target, tslot) not in seen:
                    seen.add((target, tslot))
                    slots[target].add(tslot)
                    frontier.append((target, tslot))
        return slots

    def ssa_values(self) -> Dict[str, object]:
        out: Dict[str, object] = {}
        for name, kind in self.encoder.const_kinds.items():
            if kind[0] not in ("bv", "bool"):
                continue
            if name.startswith("p_") or name.startswith("pi_") or name.startswith("trap_") \
                    or name.startswith("common_") or name.startswith("w_"):
                continue
            try:
                val = self.model.eval(_const_for(name, kind), model_completion=True)
            except Exception:
                continue
            out[name] = z3.is_true(val) if kind[0] == "bool" else val.as_long()
        return out


def _materialize_state(prog: Program, pre: Dict[str, List[Dict[str, object]]],
                       pre_agg: Optional[Dict[str, Dict[str, int]]] = None,
                       pre_hashes: Optional[Dict[Tuple[str, str], List[Tuple[bool, int]]]] = None
                       ) -> State:
    state = State()
    for tname, tinfo in prog.tables.items():
        cap = tinfo.layout.capacity
        t = TableState(
            name=tname, capacity=cap, key_bits=tinfo.layout.key_bits,
            occ=bytearray(cap), gen=[0] * cap,
            columns={c: [False if ci.type.is_bool else 0 for _ in range(cap)]
                     for c, ci in tinfo.columns.items()},
            agg={c: 0 for c in tinfo.layout.agg_offsets},
        )
        for row in pre.get(tname, []):
            key = int(row["id"])
            slot = t.slot_of(key)
            t.occ[slot] = 1
            t.gen[slot] = key >> tinfo.layout.key_bits
            for cname, cinfo in tinfo.columns.items():
                t.columns[cname][slot] = row.get(cname, 0)
        for cname in tinfo.layout.agg_offsets:
            if pre_agg is not None and tname in pre_agg:
                t.agg[cname] = pre_agg[tname][cname]
            else:
                t.agg[cname] = sum(t.columns[cname][i] for i in range(cap) if t.occ[i])
        for cname in tinfo.layout.unique_hashes:
            B = tinfo.layout.unique_hashes[cname]
            buckets = (pre_hashes or {}).get((tname, cname))
            if buckets is None:
                t.hash_keys[cname] = [0] * B
                t.hash_used[cname] = bytearray(B)
            else:
                t.hash_keys[cname] = [k for _, k in buckets]
                t.hash_used[cname] = bytearray(1 if u else 0 for u, _ in buckets)
        state.tables[tname] = t
    return state


def _eval_spec_expr(prog: Program, state: State, params: Dict[str, object],
                    env: Dict[str, object], expr: A.Expr,
                    pre_agg: Optional[Dict[str, Dict[str, int]]] = None) -> object:
    """Evaluate a specification-layer expression against a concrete state."""
    tx = Transaction(prog, state, "_eval", params)
    tx.env = dict(env)
    for k, v in env.items():
        tx.type_env[k] = I64
    if pre_agg is not None:
        tx.pre_agg = {t: dict(a) for t, a in pre_agg.items()}
    return tx.value_of(expr, "post")


def lift_counterexample(prog: Program, encoder: SmtEncoder,
                        result: VerificationResult) -> Optional[Counterexample]:
    """Build a Counterexample from a SAT safety or panic obligation."""
    if result.safety.status == "sat":
        obligation = "Obligation 1 - Invariant or Postcondition Violation"
        model = result.safety.model
    elif result.panic.status == "sat":
        obligation = "Obligation 2 - Unhandled Arithmetic/Trap Path"
        model = result.panic.model
    else:
        return None
    if model is None:
        return None

    projector = ModelProjector(prog, encoder, model)
    params = projector.param_values()
    pre = projector.pre_state(relevant_only=True)

    ce = Counterexample(transition=encoder.tr_name, obligation=obligation, law="")
    ce.params = params
    ce.pre_state = pre

    machine = Machine(prog)
    pre_agg = projector.pre_aggregates()
    pre_hashes = projector.pre_hashes()
    machine.state = _materialize_state(prog, pre, pre_agg, pre_hashes)
    pre_agg = {tname: dict(t.agg) for tname, t in machine.state.tables.items()}
    try:
        _, trace = machine.execute(encoder.tr_name, params, record_trace=True)
        ce.trace = [{"text": s.text, "kind": s.kind, "value": s.value} for s in trace]
    except Revert as exc:
        ce.trace = [{"text": f"REVERT({exc.code})", "kind": "revert", "value": exc.code}]
        ce.violated["trap"] = exc.code
        ce.law = _trap_law(exc.code)
        ce.directive = _trap_directive(exc.code)
        return ce

    # The transition committed: find the violated ensure or invariant.
    final_env = dict(getattr(machine, "last_tx", None).env or {}) \
        if getattr(machine, "last_tx", None) is not None else {}
    violated: Dict[str, object] = {}
    for name, expr in encoder.tr.decl.ensures:
        try:
            val = _eval_spec_expr(prog, machine.state, params, final_env, expr, pre_agg)
        except Revert:
            val = "<revert>"
        if val is not True:
            violated[f"ensure {name}"] = _fmt(expr, val)
    for inv in prog.invariants.values():
        if inv.quant_table is None:
            try:
                val = _eval_spec_expr(prog, machine.state, params, final_env, inv.expr)
            except Revert:
                val = "<revert>"
            if val is not True:
                violated[f"invariant {inv.name}"] = _fmt(inv.expr, val)
        else:
            t = machine.state.tables[inv.quant_table]
            for slot in range(t.capacity):
                if not t.occ[slot]:
                    continue
                key = (t.gen[slot] << t.key_bits) | slot
                env = dict(final_env)
                env[inv.quant_var] = key
                try:
                    val = _eval_spec_expr(prog, machine.state, params, env, inv.expr, pre_agg)
                except Revert:
                    val = "<revert>"
                if val is not True:
                    violated[f"invariant {inv.name}"] = \
                        f"{_fmt(inv.expr, val)} at {inv.quant_table}[id={key}]"
                    break
    ce.violated = violated
    if violated:
        first = next(iter(violated))
        ce.law = first
        ce.directive = _violation_directive(first, params, pre)
    return ce


def _trap_law(code: str) -> str:
    return {
        "ERR_ARITH_OVERFLOW": "checked arithmetic domain law (Section 4.1)",
        "ERR_MISSING_ROW": "total-access law: table_get requires a live row",
        "ERR_TABLE_FULL": "capacity law: table_alloc requires a free slot",
        "ERR_GENERATION_EXHAUSTED": "id64 generation law (Section 2.3)",
        "ERR_INDEX_SATURATED": "secondary index probe bound (Section 2.3)",
    }.get(code, f"domain law violated: {code}")


def _trap_directive(code: str) -> str:
    return {
        "ERR_ARITH_OVERFLOW": (
            "Add an explicit domain guard before the arithmetic operation, e.g. "
            "`guard(le(%rhs, sub(MAX, %lhs)), ERR_OVERFLOW);`, or widen the column type."),
        "ERR_MISSING_ROW": (
            "Add `require <key>_exists : table_exists(Table, <key>);` or guard the "
            "table_get with table_exists before reading."),
        "ERR_TABLE_FULL": (
            "Bound the table population with an invariant, or increase the static "
            "capacity and re-verify."),
        "ERR_GENERATION_EXHAUSTED": (
            "The id64 generation counter wrapped; increase the key width or compact "
            "the table."),
    }.get(code, "Guard the failing domain condition with an explicit guard().")


def _fmt(expr: A.Expr, val: object) -> str:
    return f"expected true, got {val!r}"


def _violation_directive(law: str, params: Dict[str, object],
                         pre: Dict[str, List[Dict[str, object]]]) -> str:
    if "ensure" in law:
        return ("The postcondition fails on a reachable input. Add a guard that "
                "excludes the counterexample input class, or re-read the mutated "
                "row after the first table_set so later reads observe the new value.")
    if "invariant" in law:
        return ("A global invariant is violated. Strengthen the transition's "
                "guards so the violating state is unreachable, or amend the "
                "invariant if it is not an inductive invariant.")
    return "Restrict the transition's domain with an explicit guard()."


def attach_counterexample(prog: Program, encoders, result: VerificationResult) -> VerificationResult:
    if isinstance(encoders, dict):
        if result.safety.status == "sat":
            encoder = encoders.get("safety")
        elif result.panic.status == "sat":
            encoder = encoders.get("panic")
        else:
            encoder = None
    else:
        encoder = encoders
    if encoder is not None:
        result.counterexample = lift_counterexample(prog, encoder, result)
    return result
