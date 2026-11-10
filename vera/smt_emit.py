"""Target 1: AST -> SMT-LIB 2.6 (QF_ABV) symbolic verification engine.

Implements the Section 6 translation:
  * table columns as functional SMT state (uninterpreted slot functions),
  * SSA statements as versioned constants with explicit overflow predicates,
  * table_set as an ite-chain over the pre-state column function,
  * guard/path accumulation,
  * footprint localization for forall invariants,
  * the three mandatory proof obligations (safety, panic, liveness).

The pre-state is a free uninterpreted function over slots; the post-state is an
explicit ite-chain of the pre-state plus the (bounded) set of writes.  This
avoids array store reasoning, which is the dominant cost for large capacities.

The encoder emits a concrete SMT-LIB 2.6 script and parses it back with z3 for
solving; model values are recovered by name.
"""

from dataclasses import dataclass, field
from typing import Dict, List, Optional, Set, Tuple
import time

import z3

from . import ast_nodes as A
from .errors import Diagnostic, DiagnosticBag, ERR_VACUOUS_IMPLEMENTATION
from .semantic import Program, TransitionInfo
from .types import BOOL, I64, ScalarType, id64, slot_bits_for


@dataclass
class SmtConfig:
    timeout_ms: int = 500
    max_unroll: int = 256
    ite_chain_limit: int = 256
    wide_extra_bits: int = 1
    # When False (default), the pre-state aggregate accumulator is modeled as a
    # free scalar constrained to the column domain, which is exactly the runtime
    # guarantee (the compiler reverts on accumulator overflow).  When True, the
    # accumulator is additionally tied to the exact O(C) column sum, which is
    # stronger but costs solver time proportional to capacity.
    exact_sum: bool = False


@dataclass
class ObligationResult:
    name: str
    status: str = "pending"          # unsat | sat | unknown | timeout | error
    elapsed_ms: float = 0.0
    model: Optional[z3.ModelRef] = None
    solver: Optional[z3.Solver] = None


@dataclass
class VerificationResult:
    transition: str
    safety: ObligationResult
    panic: ObligationResult
    liveness: ObligationResult
    script: str = ""
    diagnostics: List[Diagnostic] = field(default_factory=list)
    counterexample: Optional["Counterexample"] = None

    @property
    def ok(self) -> bool:
        return (self.safety.status == "unsat"
                and self.panic.status == "unsat"
                and self.liveness.status == "sat")

    def summary(self) -> str:
        lines = [
            f"transition {self.transition}:",
            f"  Obligation 1 (safety / partial correctness): {self.safety.status.upper()} "
            f"[{self.safety.elapsed_ms:.1f} ms]",
            f"  Obligation 2 (no unhandled arithmetic traps): {self.panic.status.upper()} "
            f"[{self.panic.elapsed_ms:.1f} ms]",
            f"  Obligation 3 (non-vacuity / liveness): {self.liveness.status.upper()} "
            f"[{self.liveness.elapsed_ms:.1f} ms]",
        ]
        if not self.ok:
            if self.liveness.status == "unsat":
                lines.append(f"  VERDICT: {ERR_VACUOUS_IMPLEMENTATION} "
                             f"(transition reverts on 100% of valid precondition states)")
            elif self.safety.status == "sat":
                lines.append("  VERDICT: INVARIANT OR POSTCONDITION VIOLATION")
            elif self.panic.status == "sat":
                lines.append("  VERDICT: UNHANDLED ARITHMETIC/TRAP PATH")
        else:
            lines.append("  VERDICT: VERIFIED")
        return "\n".join(lines)


@dataclass
class Counterexample:
    transition: str
    obligation: str
    law: str
    pre_state: Dict[str, List[Dict[str, object]]] = field(default_factory=dict)
    params: Dict[str, object] = field(default_factory=dict)
    trace: List[Dict[str, object]] = field(default_factory=list)
    violated: Dict[str, object] = field(default_factory=dict)
    directive: str = ""

    def render(self) -> str:
        out = [f"[VERIFICATION FAILED: {self.obligation}]",
               f"Transition:   {self.transition}",
               f"Violated Law: {self.law}", "", "MINIMAL COUNTEREXAMPLE WITNESS:"]
        out.append("1. Pre-State (Sigma_0):")
        if not self.pre_state:
            out.append("   (empty)")
        for table, rows in self.pre_state.items():
            for row in rows:
                vals = ", ".join(f"{k}: {v}" for k, v in row.items())
                out.append(f"   - {table}[id={row.get('id')}] = {{ {vals} }}")
        out.append("")
        out.append("2. Input Intent (iota):")
        for k, v in self.params.items():
            out.append(f"   - {k} = {v}")
        out.append("")
        out.append("3. SSA Execution Trace:")
        for step in self.trace:
            out.append(f"   - {step.get('text', '')}")
        out.append("")
        out.append("4. Violated Post-Condition / Invariant:")
        for k, v in self.violated.items():
            out.append(f"   - {k}: {v}")
        if self.directive:
            out.append("")
            out.append("REPAIR DIRECTIVE:")
            out.append(f"   {self.directive}")
        return "\n".join(out)


# ---------------------------------------------------------------------------
# SMT-LIB encoder
# ---------------------------------------------------------------------------

class SmtEncoder:
    """Lowers one transition plus the module invariants to SMT-LIB 2.6."""

    def __init__(self, prog: Program, transition_name: str, config: SmtConfig = None):
        self.prog = prog
        self.tr_name = transition_name
        self.tr = prog.transitions[transition_name]
        self.config = config or SmtConfig()
        self.diags = DiagnosticBag()

        self.decls: List[str] = []
        self.common_asserts: List[str] = []
        self.obligation_asserts: Dict[str, str] = {}
        self.const_kinds: Dict[str, Tuple] = {}
        self._declared: Set[str] = set()
        self._version: Dict[str, int] = {}

        # pre-state function names
        self.pre_occ: Dict[str, str] = {}
        self.pre_gen: Dict[str, str] = {}
        self.pre_col: Dict[str, Dict[str, str]] = {}
        self.pre_agg: Dict[str, Dict[str, str]] = {}
        self.cur_agg: Dict[str, Dict[str, str]] = {}
        self.agg_wide: Dict[Tuple[str, str], int] = {}
        self.agg_signed: Dict[Tuple[str, str], bool] = {}

        # writes / allocations
        self.writes: Dict[Tuple[str, str], List[Tuple[str, str]]] = {}
        self.alloc_records: Dict[str, List[Tuple[str, str]]] = {}
        self.final_values: Dict[Tuple[str, str], Dict[str, str]] = {}
        self.slot_pre_value: Dict[Tuple[str, str], Dict[str, str]] = {}

        self.params: Dict[str, str] = {}
        self.env: Dict[str, str] = {}
        self.type_env: Dict[str, ScalarType] = dict(self.tr.param_types)
        self.ssa_types: Dict[str, ScalarType] = {}

        self.pi = "pi_0"
        self.trap = "trap_0"
        self.modified_slots: Dict[str, List[str]] = {t: [] for t in prog.tables}
        self.witness_slots: Dict[str, str] = {}
        self.touched_tables: Set[str] = set()
        self._relevant: Dict[str, List[str]] = {}

        self._pi_counter = 0
        self._trap_counter = 0
        self._step_index = 0
        self.silent = False
        # marker-edit
        self.final_pi = "pi_0"
        self.final_trap = "trap_0"
        self.post_condition = "true"
        self.precondition = "true"

    # --- SMT-LIB primitives -------------------------------------------------
    def declare(self, name: str, decl: str, kind: Tuple) -> str:
        if name in self._declared:
            return name
        self._declared.add(name)
        self.decls.append(decl)
        self.const_kinds[name] = kind
        return name

    def assert_common(self, expr: str) -> None:
        self.common_asserts.append(f"(assert {expr})")

    def _bv(self, value: int, width: int) -> str:
        value = int(value) & ((1 << width) - 1)
        if width % 4 == 0:
            return "#x" + format(value, "0{}x".format(width // 4))
        return "#b" + format(value, "0{}b".format(width))

    def _fresh(self, base: str, decl: str, kind: Tuple) -> str:
        idx = self._version.get(base, 0)
        self._version[base] = idx + 1
        name = f"{base}_{idx}"
        self.declare(name, decl, kind)
        return name

    def _sort_of(self, ctype: ScalarType) -> str:
        return "Bool" if ctype.is_bool else f"(_ BitVec {ctype.width})"

    def _kind_of(self, ctype: ScalarType) -> Tuple:
        return ("bool",) if ctype.is_bool else ("bv", ctype.width)

    def _zero(self, ctype: ScalarType) -> str:
        return "false" if ctype.is_bool else self._bv(0, ctype.width)

    def _apply(self, fn: str, idx: str) -> str:
        return f"({fn} {idx})"

    def _ite(self, cond: str, a: str, b: str) -> str:
        return f"(ite {cond} {a} {b})"

    def _and(self, *terms: str) -> str:
        terms = [t for t in terms if t is not None]
        if not terms:
            return "true"
        if len(terms) == 1:
            return terms[0]
        return "(and " + " ".join(terms) + ")"

    def _or(self, *terms: str) -> str:
        terms = [t for t in terms if t is not None]
        if not terms:
            return "false"
        if len(terms) == 1:
            return terms[0]
        return "(or " + " ".join(terms) + ")"

    def _not(self, term: str) -> str:
        return f"(not {term})"

    def _cmp(self, op: str, a: str, b: str, signed: bool) -> str:
        if op == "eq":
            return f"(= {a} {b})"
        if op == "ne":
            return f"(not (= {a} {b}))"
        fn = {"lt": "bvult" if not signed else "bvslt",
              "le": "bvule" if not signed else "bvsle",
              "gt": "bvugt" if not signed else "bvsgt",
              "ge": "bvuge" if not signed else "bvsge"}[op]
        return f"({fn} {a} {b})"

    def _extend(self, expr: str, width: int, target: int, signed: bool) -> str:
        if target == width:
            return expr
        op = "sign_extend" if signed else "zero_extend"
        return f"((_ {op} {target - width}) {expr})"

    def _truncate(self, expr: str, width: int, target: int) -> str:
        if target == width:
            return expr
        return f"((_ extract {target - 1} 0) {expr})"

    # --- layout ------------------------------------------------------------
    def _slot_of_key(self, key_expr: str, table: str) -> str:
        sbits = self.table_slot_bits[table]
        if sbits == 64:
            return key_expr
        return f"((_ extract {sbits - 1} 0) {key_expr})"

    # --- entry -------------------------------------------------------------
    def build(self) -> None:
        self.table_slot_bits = {name: slot_bits_for(t.layout.capacity)
                                for name, t in self.prog.tables.items()}
        self.slot_bits = max(self.table_slot_bits.values())
        self._build_state()
        self._build_well_formedness()
        self._encode_body()
        self._relevant = self._relevant_slots()
        self._build_pre_invariants()
        self._build_post_condition()
        self._build_obligations()

    def _build_state(self) -> None:
        for p in self.tr.decl.params:
            ctype = self.tr.param_types[p.name]
            name = f"p_{p.name}"
            self.declare(name, f"(declare-const {name} {self._sort_of(ctype)})",
                         self._kind_of(ctype))
            self.params[p.name] = name
        for tname, tinfo in self.prog.tables.items():
            cap = tinfo.layout.capacity
            sbits = self.table_slot_bits[tname]
            gen_bits = min(64 - tinfo.layout.key_bits, 32)
            occ = f"{tname}_occ"
            self.declare(occ, f"(declare-fun {occ} ((_ BitVec {sbits})) Bool)",
                         ("fun", sbits, "bool"))
            gen = f"{tname}_gen"
            self.declare(gen, f"(declare-fun {gen} ((_ BitVec {sbits})) (_ BitVec {gen_bits}))",
                         ("fun", sbits, gen_bits))
            self.pre_occ[tname] = occ
            self.pre_gen[tname] = gen
            self.pre_col[tname] = {}
            for cname, cinfo in tinfo.columns.items():
                col = f"{tname}_{cname}"
                self.declare(col, f"(declare-fun {col} ((_ BitVec {sbits})) {self._sort_of(cinfo.type)})",
                             ("fun", sbits, "bool" if cinfo.type.is_bool else cinfo.type.width))
                self.pre_col[tname][cname] = col
            if tinfo.layout.agg_offsets:
                self.pre_agg[tname] = {}
                self.cur_agg[tname] = {}
                wide_w = self._wide_width(tinfo)
                for cname in tinfo.layout.agg_offsets:
                    ctype = tinfo.columns[cname].type
                    self.agg_wide[(tname, cname)] = wide_w
                    self.agg_signed[(tname, cname)] = ctype.is_signed
                    agg = self._fresh(f"{tname}_{cname}_agg",
                                      f"(declare-const {tname}_{cname}_agg_0 (_ BitVec {wide_w}))",
                                      ("bv", wide_w))
                    if self.config.exact_sum:
                        exact = self._exact_sum(tname, self.pre_col[tname][cname], ctype,
                                                cap, wide_w)
                        self.assert_common(f"(= {agg} {exact})")
                    else:
                        self.assert_common(self._range_check(agg, ctype, wide_w))
                    self.pre_agg[tname][cname] = agg
                    self.cur_agg[tname][cname] = agg

    def _wide_width(self, tinfo) -> int:
        max_w = max(c.type.width for c in tinfo.columns.values())
        return max_w + (tinfo.layout.capacity - 1).bit_length() + self.config.wide_extra_bits

    def _exact_sum(self, tname: str, fn: str, ctype: ScalarType, capacity: int,
                   wide_w: int) -> str:
        sbits = self.table_slot_bits[tname]
        terms: List[str] = []
        offset = 1 << (ctype.width - 1) if ctype.is_signed else 0
        for i in range(capacity):
            v = self._apply(fn, self._bv(i, sbits))
            if offset:
                v = f"(bvadd {v} {self._bv(offset, ctype.width)})"
            terms.append(self._extend(v, ctype.width, wide_w, False))

        def tree(terms: List[str]) -> str:
            if len(terms) == 1:
                return terms[0]
            mid = len(terms) // 2
            return f"(bvadd {tree(terms[:mid])} {tree(terms[mid:])})"

        if not terms:
            return self._bv(0, wide_w)
        acc = tree(terms)
        if offset:
            corr = self._bv((capacity * offset) & ((1 << wide_w) - 1), wide_w)
            acc = f"(bvsub {acc} {corr})"
        return acc

    # --- state access ------------------------------------------------------
    def _current_occ(self, table: str, slot: str) -> str:
        terms = [self._apply(self.pre_occ[table], slot)]
        for alloc_slot, _ in self.alloc_records.get(table, []):
            terms.append(f"(= {slot} {alloc_slot})")
        return self._or(*terms)

    def _current_gen(self, table: str, slot: str) -> str:
        expr = self._apply(self.pre_gen[table], slot)
        for alloc_slot, new_gen in reversed(self.alloc_records.get(table, [])):
            expr = self._ite(f"(= {slot} {alloc_slot})", new_gen, expr)
        return expr

    def _current_col(self, table: str, cname: str, key_expr: str,
                     ctype: ScalarType) -> str:
        slot = self._slot_of_key(key_expr, table)
        expr = self._apply(self.pre_col[table][cname], slot)
        for wslot, wval in reversed(self.writes.get((table, cname), [])):
            expr = self._ite(f"(= {slot} {wslot})", wval, expr)
        return expr

    def _current_valid(self, key_expr: str, table: str) -> str:
        slot = self._slot_of_key(key_expr, table)
        occ = self._current_occ(table, slot)
        gen = self._current_gen(table, slot)
        shift = self.prog.tables[table].layout.key_bits
        gen_bits = min(64 - shift, 32)
        if shift == 0:
            expected = key_expr
        else:
            expected = f"((_ extract {shift + gen_bits - 1} {shift}) {key_expr})"
        high_bits = 64 - (shift + gen_bits)
        if high_bits > 0:
            high = f"((_ extract 63 {shift + gen_bits}) {key_expr})"
            return self._and(occ, f"(= {gen} {expected})",
                             f"(= {high} {self._bv(0, high_bits)})")
        return self._and(occ, f"(= {gen} {expected})")

    def _pre_valid(self, key_expr: str, table: str) -> str:
        slot = self._slot_of_key(key_expr, table)
        occ = self._apply(self.pre_occ[table], slot)
        gen = self._apply(self.pre_gen[table], slot)
        shift = self.prog.tables[table].layout.key_bits
        gen_bits = min(64 - shift, 32)
        if shift == 0:
            expected = key_expr
        else:
            expected = f"((_ extract {shift + gen_bits - 1} {shift}) {key_expr})"
        high_bits = 64 - (shift + gen_bits)
        if high_bits > 0:
            high = f"((_ extract 63 {shift + gen_bits}) {key_expr})"
            return self._and(occ, f"(= {gen} {expected})",
                             f"(= {high} {self._bv(0, high_bits)})")
        return self._and(occ, f"(= {gen} {expected})")

    # --- well-formedness ---------------------------------------------------
    def _build_well_formedness(self) -> None:
        for tname, tinfo in self.prog.tables.items():
            cap = tinfo.layout.capacity
            sbits = self.table_slot_bits[tname]
            for i in range(cap):
                slot = self._bv(i, sbits)
                occ = self._apply(self.pre_occ[tname], slot)
                for cname, cinfo in tinfo.columns.items():
                    val = self._apply(self.pre_col[tname][cname], slot)
                    self.assert_common(
                        f"(=> {self._not(occ)} (= {val} {self._zero(cinfo.type)}))")

    # --- pre-invariants ----------------------------------------------------
    def _build_pre_invariants(self) -> None:
        for inv in self.prog.invariants.values():
            if inv.quant_table is None:
                expr = self._encode_expr(inv.expr, {}, "pre", BOOL)
                self.assert_common(expr)
                continue
            tname = inv.quant_table
            tinfo = self.prog.tables[tname]
            if self.config.exact_sum:
                # The exact sum ranges over every slot, so the inductive
                # pre-invariant must hold at every slot for soundness.
                sbits = self.table_slot_bits[tname]
                slots = [self._bv(i, sbits) for i in range(tinfo.layout.capacity)]
            else:
                slots = self._relevant.get(tname, [])
            for slot in slots:
                occ = self._apply(self.pre_occ[tname], slot)
                body = self._encode_forall_body(inv.expr, tname, inv.quant_var,
                                                slot, "pre")
                self.assert_common(f"(=> {occ} {body})")

    def _encode_forall_body(self, expr: A.Expr, table: str, var: str,
                            slot: str, state: str) -> str:
        env = {var: self._key_expr(table, slot, state)}
        return self._encode_expr(expr, env, state, BOOL)

    def _key_expr(self, table: str, slot: str, state: str = "post") -> str:
        tinfo = self.prog.tables[table]
        gen_fn = self.pre_gen[table]
        gen = self._apply(gen_fn, slot)
        if tinfo.layout.key_bits == 0:
            return gen
        return f"(concat {gen} {slot})"

    # --- relevant slots ----------------------------------------------------
    def _relevant_slots(self) -> Dict[str, List[str]]:
        out: Dict[str, List[str]] = {t: [] for t in self.prog.tables}
        seen: Dict[str, Set[str]] = {t: set() for t in self.prog.tables}

        def add(table: str, slot: Optional[str]) -> None:
            if slot is None:
                return
            self.touched_tables.add(table)
            if slot not in seen[table]:
                seen[table].add(slot)
                out[table].append(slot)

        def walk(expr: A.Expr, env: Dict[str, str]) -> None:
            if isinstance(expr, A.TableAccess):
                self.touched_tables.add(expr.table)
                add(expr.table, self._simple_slot(expr.key, env, expr.table))
                walk(expr.key, env)
            elif isinstance(expr, A.Old):
                self.touched_tables.add(expr.access.table)
                add(expr.access.table, self._simple_slot(expr.access.key, env,
                                                         expr.access.table))
                walk(expr.access.key, env)
            elif isinstance(expr, A.TableGet):
                self.touched_tables.add(expr.table)
                add(expr.table, self._simple_slot(expr.key, env, expr.table))
                walk(expr.key, env)
            elif isinstance(expr, A.TableExists):
                self.touched_tables.add(expr.table)
                add(expr.table, self._simple_slot(expr.key, env, expr.table))
                walk(expr.key, env)
            elif isinstance(expr, A.SumAgg):
                self.touched_tables.add(expr.table)
            elif isinstance(expr, (A.Var, A.Literal)):
                return
            elif isinstance(expr, A.Unary):
                walk(expr.operand, env)
            elif isinstance(expr, A.Binary):
                walk(expr.left, env)
                walk(expr.right, env)
            elif isinstance(expr, A.Select):
                walk(expr.cond, env)
                walk(expr.a, env)
                walk(expr.b, env)
            elif isinstance(expr, A.KeyOf):
                walk(expr.expr, env)
            elif isinstance(expr, A.Fold):
                walk(expr.init, env)
                walk(expr.lo, env)
                walk(expr.hi, env)
                for s in expr.body:
                    walk_stmt(s, env)
                walk(expr.yield_expr, env)

        def walk_stmt(stmt: A.Stmt, env: Dict[str, str]) -> None:
            if isinstance(stmt, A.Let):
                walk(stmt.value, env)
            elif isinstance(stmt, A.Guard):
                walk(stmt.cond, env)
            elif isinstance(stmt, A.TableSet):
                self.touched_tables.add(stmt.table)
                add(stmt.table, self._simple_slot(stmt.key, env, stmt.table))
                walk(stmt.key, env)
                walk(stmt.value, env)
            elif isinstance(stmt, A.TableAlloc):
                self.touched_tables.add(stmt.table)
                for _, fexpr in stmt.fields:
                    walk(fexpr, env)
            elif isinstance(stmt, A.Emit):
                for a in stmt.args:
                    walk(a, env)

        for _, rexpr in self.tr.decl.requires:
            walk(rexpr, self.params)
        for _, eexpr in self.tr.decl.ensures:
            walk(eexpr, self.env)
        for stmt in self.tr.decl.body:
            walk_stmt(stmt, self.env)
        for inv in self.prog.invariants.values():
            if inv.quant_table is None:
                continue
            tname = inv.quant_table
            self.touched_tables.add(tname)
            w = f"w_{inv.name}"
            sbits = self.table_slot_bits[tname]
            self.declare(w, f"(declare-const {w} (_ BitVec {sbits}))", ("bv", sbits))
            self.witness_slots.setdefault(tname, w)
            add(tname, w)
        return out

    def _encode_key_simple(self, expr: A.Expr, env: Dict[str, str]) -> Optional[str]:
        if isinstance(expr, A.Var):
            if expr.name in env:
                return self._as64(env[expr.name], self.type_env.get(expr.name))
            if expr.name in self.params:
                return self._as64(self.params[expr.name], self.tr.param_types.get(expr.name))
            return None
        if isinstance(expr, A.Literal):
            if isinstance(expr.value, bool) or isinstance(expr.value, str):
                return None
            return self._bv(int(expr.value) & ((1 << 64) - 1), 64)
        if isinstance(expr, A.Binary):
            a = self._encode_key_simple(expr.left, env)
            b = self._encode_key_simple(expr.right, env)
            if a is None or b is None:
                return None
            names = {"add": "bvadd", "sub": "bvsub", "mul": "bvmul",
                     "div": "bvudiv", "rem": "bvurem"}
            if expr.op in names:
                return f"({names[expr.op]} {a} {b})"
            if expr.op == "and":
                return f"(bvand {a} {b})"
            if expr.op == "or":
                return f"(bvor {a} {b})"
            if expr.op == "xor":
                return f"(bvxor {a} {b})"
            if expr.op == "min":
                return self._ite(self._cmp("ult", a, b, False), a, b)
            if expr.op == "max":
                return self._ite(self._cmp("ugt", a, b, False), a, b)
            return None
        if isinstance(expr, A.Unary):
            a = self._encode_key_simple(expr.operand, env)
            if a is None:
                return None
            if expr.op == "neg":
                return f"(bvneg {a})"
            return None
        if isinstance(expr, A.Select):
            c = self._encode_key_simple(expr.cond, env)
            a = self._encode_key_simple(expr.a, env)
            b = self._encode_key_simple(expr.b, env)
            if c is None or a is None or b is None:
                return None
            return self._ite(c, a, b)
        if isinstance(expr, A.KeyOf):
            return self._encode_key_simple(expr.expr, env)
        return None

    def _as64(self, expr: str, ctype: Optional[ScalarType]) -> str:
        if ctype is None or ctype.width == 64:
            return expr
        return self._extend(expr, ctype.width, 64, ctype.is_signed)

    def _simple_slot(self, expr: Optional[A.Expr], env: Dict[str, str],
                     table: str) -> Optional[str]:
        if expr is None:
            return None
        key = self._encode_key_simple(expr, env)
        if key is None:
            return None
        return self._slot_of_key(key, table)

    # --- body --------------------------------------------------------------
    def _encode_body(self) -> None:
        self._pi_counter = 0
        self._trap_counter = 0
        self._step_index = 0
        self.declare("pi_0", "(declare-const pi_0 Bool)", ("bool",))
        self.declare("trap_0", "(declare-const trap_0 Bool)", ("bool",))
        self.assert_common("(= pi_0 true)")
        self.assert_common("(= trap_0 false)")
        for stmt in self.tr.decl.body:
            self._encode_stmt(stmt, self.env)
        self.final_pi = self.pi
        self.final_trap = self.trap

    def _pi_update(self, cond: str) -> None:
        if self.silent:
            return
        self._pi_counter += 1
        name = f"pi_{self._pi_counter}"
        self.declare(name, f"(declare-const {name} Bool)", ("bool",))
        self.assert_common(f"(= {name} (and {self.pi} {cond}))")
        self.pi = name

    def _trap_update(self, trap_cond: str) -> None:
        if self.silent:
            return
        self._trap_counter += 1
        name = f"trap_{self._trap_counter}"
        self.declare(name, f"(declare-const {name} Bool)", ("bool",))
        self.assert_common(f"(= {name} (or {self.trap} (and {self.pi} {trap_cond})))")
        self.trap = name

    def _encode_stmt(self, stmt: A.Stmt, env: Dict[str, str]) -> None:
        self._step_index += 1
        if isinstance(stmt, A.Let):
            if isinstance(stmt.value, A.Fold):
                val = self._encode_fold(stmt.value, env)
            else:
                val = self._encode_expr(stmt.value, env, "post")
            env[stmt.name] = val
            self.env[stmt.name] = val
            self.type_env[stmt.name] = self.tr.ssa_types.get(stmt.name, I64)
        elif isinstance(stmt, A.Guard):
            cond = self._encode_expr(stmt.cond, env, "post", BOOL)
            self._pi_update(cond)
        elif isinstance(stmt, A.TableSet):
            self._encode_table_set(stmt, env)
        elif isinstance(stmt, A.TableAlloc):
            self._encode_table_alloc(stmt, env)
            self.type_env[stmt.name] = id64(stmt.table)
        elif isinstance(stmt, A.Emit):
            for arg in stmt.args:
                self._encode_expr(arg, env, "post")
        elif isinstance(stmt, A.Yield):
            self._encode_expr(stmt.value, env, "post")
        elif isinstance(stmt, A.Commit):
            pass

    def _encode_table_set(self, stmt: A.TableSet, env: Dict[str, str]) -> None:
        tname = stmt.table
        cinfo = self.prog.tables[tname].columns[stmt.column]
        ctype = cinfo.type
        key = self._encode_expr(stmt.key, env, "post")
        slot = self._slot_of_key(key, tname)
        valid = self._current_valid(key, tname)
        self._trap_update(self._not(valid))
        self._pi_update(valid)
        val = self._encode_expr(stmt.value, env, "post")
        v_old = self._current_col(tname, stmt.column, key, ctype)
        self.writes.setdefault((tname, stmt.column), []).append((slot, val))
        self.modified_slots[tname].append(slot)
        self._record_final(tname, stmt.column, slot, val, None, False)
        if cinfo.tracked_sum:
            self._agg_update(tname, stmt.column, v_old, val, ctype)

    def _record_final(self, tname: str, cname: str, slot: str, value: str,
                      active: Optional[str], is_alloc: bool) -> None:
        key = (tname, cname)
        fv = self.final_values.setdefault(key, {})
        pv = self.slot_pre_value.setdefault(key, {})
        if slot not in pv:
            ctype = self.prog.tables[tname].columns[cname].type
            pv[slot] = self._zero(ctype) if is_alloc else \
                self._apply(self.pre_col[tname][cname], slot)
        prev = fv.get(slot, pv[slot])
        fv[slot] = value if active is None else self._ite(active, value, prev)

    def _agg_update(self, tname: str, cname: str, v_old: str, v_new: str,
                    ctype: ScalarType) -> None:
        wide_w = self.agg_wide[(tname, cname)]
        signed = self.agg_signed[(tname, cname)]
        cur = self.cur_agg[tname][cname]
        w_old = self._extend(v_old, ctype.width, wide_w, signed)
        w_new = self._extend(v_new, ctype.width, wide_w, signed)
        new_val = f"(bvadd (bvsub {cur} {w_old}) {w_new})"
        in_range = self._range_check(new_val, ctype, wide_w)
        self._trap_update(self._not(in_range))
        self._pi_update(in_range)
        new_agg = self._fresh(f"{tname}_{cname}_agg",
                              f"(declare-const {tname}_{cname}_agg_{self._version[f'{tname}_{cname}_agg']} (_ BitVec {wide_w}))",
                              ("bv", wide_w))
        self.assert_common(f"(= {new_agg} {new_val})")
        self.cur_agg[tname][cname] = new_agg

    def _range_check(self, expr: str, ctype: ScalarType, wide_w: int) -> str:
        mn = ctype.min_value & ((1 << wide_w) - 1)
        mx = ctype.max_value & ((1 << wide_w) - 1)
        if ctype.is_signed:
            return self._and(f"(bvsge {expr} {self._bv(mn, wide_w)})",
                             f"(bvsle {expr} {self._bv(mx, wide_w)})")
        return self._and(f"(bvuge {expr} {self._bv(0, wide_w)})",
                         f"(bvule {expr} {self._bv(mx, wide_w)})")

    def _encode_table_alloc(self, stmt: A.TableAlloc, env: Dict[str, str],
                            active: Optional[str] = None) -> None:
        tname = stmt.table
        tinfo = self.prog.tables[tname]
        cap = tinfo.layout.capacity
        sbits = self.table_slot_bits[tname]
        gen_bits = min(64 - tinfo.layout.key_bits, 32)
        if cap <= self.config.ite_chain_limit:
            slot = self._lowest_free_ite(tname, cap, sbits)
        else:
            slot = self._symbolic_free_slot(tname, cap, sbits)
        free_exists = self._not(self._current_occ(tname, slot))
        gen = self._current_gen(tname, slot)
        gen_max = self._bv((1 << gen_bits) - 1, gen_bits)
        gen_ok = self._not(f"(= {gen} {gen_max})")
        alloc_ok = self._and(free_exists, gen_ok)
        if active is None:
            # Allocation saturation is a *handled* deterministic revert
            # (ERR_TABLE_FULL / ERR_GENERATION_EXHAUSTED), not an unhandled panic.
            self._pi_update(alloc_ok)
        else:
            self._pi_update(f"(=> {active} {alloc_ok})")
        new_gen = f"(bvadd {gen} {self._bv(1, gen_bits)})"
        if tinfo.layout.key_bits == 0:
            new_id = new_gen
        else:
            new_id = f"(concat {new_gen} {slot})"
        if active is not None:
            new_id = self._ite(active, new_id, self._bv(0, 64))
        env[stmt.name] = new_id
        self.env[stmt.name] = new_id
        self.alloc_records.setdefault(tname, []).append((slot, new_gen))
        self.modified_slots[tname].append(slot)
        for cname, cinfo in tinfo.columns.items():
            fexpr = next((e for n, e in stmt.fields if n == cname), None)
            init = self._zero(cinfo.type) if fexpr is None \
                else self._encode_expr(fexpr, env, "post", cinfo.type)
            self.writes.setdefault((tname, cname), []).append((slot, init))
            self._record_final(tname, cname, slot, init, active, True)
            if cinfo.tracked_sum:
                self._agg_add(tname, cname, init, cinfo.type, active)

    def _agg_add(self, tname: str, cname: str, v_new: str, ctype: ScalarType,
                 active: Optional[str] = None) -> None:
        wide_w = self.agg_wide[(tname, cname)]
        signed = self.agg_signed[(tname, cname)]
        cur = self.cur_agg[tname][cname]
        w_new = self._extend(v_new, ctype.width, wide_w, signed)
        delta = w_new if active is None else self._ite(active, w_new, self._bv(0, wide_w))
        new_val = f"(bvadd {cur} {delta})"
        in_range = self._range_check(new_val, ctype, wide_w)
        if active is None:
            self._trap_update(self._not(in_range))
            self._pi_update(in_range)
        else:
            self._trap_update(self._and(active, self._not(in_range)))
            self._pi_update(f"(=> {active} {in_range})")
        new_agg = self._fresh(f"{tname}_{cname}_agg",
                              f"(declare-const {tname}_{cname}_agg_{self._version[f'{tname}_{cname}_agg']} (_ BitVec {wide_w}))",
                              ("bv", wide_w))
        self.assert_common(f"(= {new_agg} {new_val})")
        self.cur_agg[tname][cname] = new_agg

    def _lowest_free_ite(self, table: str, cap: int, sbits: int) -> str:
        expr = self._bv(cap - 1, sbits)
        for i in range(cap - 1, -1, -1):
            slot = self._bv(i, sbits)
            free = self._not(self._current_occ(table, slot))
            expr = self._ite(free, slot, expr)
        return expr

    def _symbolic_free_slot(self, table: str, cap: int, sbits: int) -> str:
        name = self._fresh(f"{table}_alloc_slot",
                           f"(declare-const {table}_alloc_slot_{self._version[f'{table}_alloc_slot']} (_ BitVec {sbits}))",
                           ("bv", sbits))
        self.assert_common(self._not(self._current_occ(table, name)))
        return name

    # --- folds -------------------------------------------------------------
    def _encode_fold(self, fold: A.Fold, env: Dict[str, str]) -> str:
        if fold.n > self.config.max_unroll:
            self.diags.error("D_FOLD_BOUND",
                             f"fold_bounded<{fold.n}> exceeds the verifier unroll "
                             f"limit {self.config.max_unroll}", fold.line, fold.col)
            return self._zero(self._resolve_fold_type(fold))
        lo = self._encode_expr(fold.lo, env, "post")
        hi = self._encode_expr(fold.hi, env, "post")
        idx_type = self._fold_index_type(fold)
        acc = self._encode_expr(fold.init, env, "post")
        for j in range(fold.n):
            idx_val = f"(bvadd {lo} {self._bv(j, idx_type.width)})"
            active = self._cmp("le", idx_val, hi, idx_type.is_signed)
            inner_env = dict(env)
            inner_env[fold.index_name] = idx_val
            inner_env[fold.acc_name] = acc
            for stmt in fold.body:
                self._encode_fold_stmt(stmt, inner_env, active)
            yield_val = self._encode_expr(fold.yield_expr, inner_env, "post")
            acc = self._ite(active, yield_val, acc)
        env[fold.acc_name] = acc
        return acc

    def _encode_fold_stmt(self, stmt: A.Stmt, env: Dict[str, str], active: str) -> None:
        if isinstance(stmt, A.Let):
            if isinstance(stmt.value, A.Fold):
                val = self._encode_fold(stmt.value, env)
            else:
                val = self._encode_expr(stmt.value, env, "post")
            env[stmt.name] = val
            from .semantic import _resolve_type_ref
            ft = _resolve_type_ref(self.prog, stmt.type, "fold let")
            if ft is not None:
                self.type_env[stmt.name] = ft
        elif isinstance(stmt, A.Guard):
            cond = self._encode_expr(stmt.cond, env, "post", BOOL)
            self._pi_update(f"(=> {active} {cond})")
        elif isinstance(stmt, A.TableSet):
            tname = stmt.table
            cinfo = self.prog.tables[tname].columns[stmt.column]
            ctype = cinfo.type
            key = self._encode_expr(stmt.key, env, "post")
            slot = self._slot_of_key(key, tname)
            valid = self._current_valid(key, tname)
            self._trap_update(self._and(active, self._not(valid)))
            self._pi_update(f"(=> {active} {valid})")
            val = self._encode_expr(stmt.value, env, "post")
            v_old = self._current_col(tname, stmt.column, key, ctype)
            self.writes.setdefault((tname, stmt.column), []).append(
                (slot, self._ite(active, val, v_old)))
            self.modified_slots[tname].append(slot)
            self._record_final(tname, stmt.column, slot, val, active, False)
            if cinfo.tracked_sum:
                wide_w = self.agg_wide[(tname, stmt.column)]
                signed = self.agg_signed[(tname, stmt.column)]
                cur = self.cur_agg[tname][stmt.column]
                w_old = self._extend(v_old, ctype.width, wide_w, signed)
                w_new = self._extend(val, ctype.width, wide_w, signed)
                delta = self._ite(active, f"(bvsub {w_new} {w_old})", self._bv(0, wide_w))
                new_val = f"(bvadd {cur} {delta})"
                in_range = self._range_check(new_val, ctype, wide_w)
                self._trap_update(self._and(active, self._not(in_range)))
                self._pi_update(f"(=> {active} {in_range})")
                new_agg = self._fresh(f"{tname}_{stmt.column}_agg",
                                      f"(declare-const {tname}_{stmt.column}_agg_{self._version[f'{tname}_{stmt.column}_agg']} (_ BitVec {wide_w}))",
                                      ("bv", wide_w))
                self.assert_common(f"(= {new_agg} {new_val})")
                self.cur_agg[tname][stmt.column] = new_agg
        elif isinstance(stmt, A.TableAlloc):
            self._encode_table_alloc(stmt, env, active)
        elif isinstance(stmt, A.Emit):
            for arg in stmt.args:
                self._encode_expr(arg, env, "post")
        elif isinstance(stmt, A.Yield):
            self._encode_expr(stmt.value, env, "post")

    def _resolve_fold_type(self, fold: A.Fold) -> ScalarType:
        from .semantic import _resolve_type_ref
        t = _resolve_type_ref(self.prog, fold.acc_type, "fold")
        return t or I64

    def _fold_index_type(self, fold: A.Fold) -> ScalarType:
        from .semantic import _TypeChecker
        tc = _TypeChecker(self.prog, dict(self.type_env))
        return tc.type_of(fold.lo)

    # --- expressions -------------------------------------------------------
    def _encode_expr(self, expr: A.Expr, env: Dict[str, str], state: str,
                     expected: Optional[ScalarType] = None) -> str:
        if isinstance(expr, A.Literal):
            return self._encode_literal(expr, expected)
        if isinstance(expr, A.Var):
            if expr.name in env:
                return env[expr.name]
            if expr.name in self.params:
                return self.params[expr.name]
            if expected is not None and expected.kind == "enum" \
                    and expr.name in expected.enum_tags:
                return self._bv(expected.enum_tags.index(expr.name), expected.width)
            self.diags.error("D_UNKNOWN_PARAM",
                             f"unknown variable {expr.name!r}", expr.line, expr.col)
            return self._bv(0, expected.width if expected is not None else 64)
        if isinstance(expr, A.TableGet):
            cinfo = self.prog.tables[expr.table].columns[expr.column]
            ctype = cinfo.type
            key = self._encode_expr(expr.key, env, state)
            if state == "pre":
                slot = self._slot_of_key(key, expr.table)
                return self._apply(self.pre_col[expr.table][expr.column], slot)
            valid = self._current_valid(key, expr.table)
            val = self._current_col(expr.table, expr.column, key, ctype)
            self._trap_update(self._not(valid))
            self._pi_update(valid)
            return self._ite(valid, val, self._zero(ctype))
        if isinstance(expr, A.TableExists):
            key = self._encode_expr(expr.key, env, state)
            if state == "pre":
                return self._pre_valid(key, expr.table)
            return self._current_valid(key, expr.table)
        if isinstance(expr, A.TableAccess):
            cinfo = self.prog.tables[expr.table].columns[expr.column]
            key = self._encode_expr(expr.key, env, state)
            if state == "pre":
                slot = self._slot_of_key(key, expr.table)
                return self._apply(self.pre_col[expr.table][expr.column], slot)
            return self._current_col(expr.table, expr.column, key, cinfo.type)
        if isinstance(expr, A.Old):
            acc = expr.access
            cinfo = self.prog.tables[acc.table].columns[acc.column]
            key = self._encode_expr(acc.key, env, "pre")
            slot = self._slot_of_key(key, acc.table)
            return self._apply(self.pre_col[acc.table][acc.column], slot)
        if isinstance(expr, A.SumAgg):
            if expr.old or state == "pre":
                return self.pre_agg[expr.table][expr.column]
            return self.cur_agg[expr.table][expr.column]
        if isinstance(expr, A.KeyOf):
            return self._encode_expr(expr.expr, env, state)
        if isinstance(expr, A.Select):
            cond = self._encode_expr(expr.cond, env, state, BOOL)
            a = self._encode_expr(expr.a, env, state, expected)
            b = self._encode_expr(expr.b, env, state, expected)
            return self._ite(cond, a, b)
        if isinstance(expr, A.Unary):
            return self._encode_unary(expr, env, state, expected)
        if isinstance(expr, A.Binary):
            return self._encode_binary(expr, env, state, expected)
        if isinstance(expr, A.Fold):
            return self._encode_fold(expr, env)
        self.diags.error("D_TYPE_MISMATCH",
                         f"cannot encode expression {type(expr).__name__}",
                         getattr(expr, "line", 0), getattr(expr, "col", 0))
        return self._bv(0, expected.width if expected is not None else 64)

    def _encode_literal(self, expr: A.Literal,
                        expected: Optional[ScalarType]) -> str:
        val = expr.value
        if isinstance(val, bool):
            return "true" if val else "false"
        if isinstance(val, str):
            return self._bv(_intern_symbol(val), 32)
        ctype = expected or I64
        if isinstance(val, float):
            scale = 10 ** ctype.scale if ctype.is_decimal else 1
            raw = int(round(val * scale))
            return self._bv(raw & ((1 << ctype.width) - 1), ctype.width)
        return self._bv(int(val) & ((1 << ctype.width) - 1), ctype.width)

    def _encode_unary(self, expr: A.Unary, env: Dict[str, str], state: str,
                      expected: Optional[ScalarType]) -> str:
        if expr.op == "not":
            operand = self._encode_expr(expr.operand, env, state, BOOL)
            return self._not(operand)
        if expr.op == "neg":
            ctype = expected or self._infer_type(expr.operand, env)
            operand = self._encode_expr(expr.operand, env, state, ctype)
            if ctype.is_signed:
                w = ctype.width
                wide = w + 1
                op_w = self._extend(operand, w, wide, True)
                res_w = f"(bvneg {op_w})"
                mn = self._bv(ctype.min_value & ((1 << wide) - 1), wide)
                mx = self._bv(ctype.max_value & ((1 << wide) - 1), wide)
                ok = self._and(f"(bvsge {res_w} {mn})", f"(bvsle {res_w} {mx})")
                self._trap_update(self._not(ok))
                self._pi_update(ok)
                return self._truncate(res_w, wide, w)
            self.diags.error("D_BAD_OPERAND",
                             "neg() on an unsigned type always overflows",
                             expr.line, expr.col)
            return self._bv(0, ctype.width)
        self.diags.error("D_BAD_OPERAND", f"unknown unary operator {expr.op!r}",
                         expr.line, expr.col)
        return self._bv(0, 8)

    def _encode_binary(self, expr: A.Binary, env: Dict[str, str], state: str,
                       expected: Optional[ScalarType]) -> str:
        op = expr.op
        if op in ("and", "or", "xor"):
            a = self._encode_expr(expr.left, env, state, BOOL)
            b = self._encode_expr(expr.right, env, state, BOOL)
            if op == "and":
                return self._and(a, b)
            if op == "or":
                return self._or(a, b)
            return f"(xor {a} {b})"
        if op in ("add", "sub", "mul", "div", "rem", "min", "max"):
            if expected is not None and not expected.is_numeric:
                expected = None
            ctype = self._infer_arith_type(expr, env, expected)
            a = self._encode_expr(expr.left, env, state, ctype)
            b = self._encode_expr(expr.right, env, state, ctype)
            return self._encode_arith(op, a, b, ctype, expr)
        if op in ("eq", "ne", "lt", "le", "gt", "ge"):
            ctype = self._infer_cmp_type(expr, env, None)
            left_is_agg = isinstance(expr.left, A.SumAgg)
            right_is_agg = isinstance(expr.right, A.SumAgg)
            if left_is_agg or right_is_agg:
                widths = []
                if left_is_agg:
                    widths.append(self.agg_wide[(expr.left.table, expr.left.column)])
                if right_is_agg:
                    widths.append(self.agg_wide[(expr.right.table, expr.right.column)])
                wide_w = max(widths)
                a = self._encode_expr(expr.left, env, state, ctype)
                b = self._encode_expr(expr.right, env, state, ctype)
                a_width = self.agg_wide[(expr.left.table, expr.left.column)] \
                    if left_is_agg else ctype.width
                b_width = self.agg_wide[(expr.right.table, expr.right.column)] \
                    if right_is_agg else ctype.width
                a = self._extend(a, a_width, wide_w, ctype.is_signed)
                b = self._extend(b, b_width, wide_w, ctype.is_signed)
            else:
                a = self._encode_expr(expr.left, env, state, ctype)
                b = self._encode_expr(expr.right, env, state, ctype)
            if op in ("eq", "ne"):
                cmp = f"(= {a} {b})"
                return cmp if op == "eq" else self._not(cmp)
            return self._cmp(op, a, b, ctype.is_signed)
        self.diags.error("D_BAD_OPERAND", f"unknown binary operator {op!r}",
                         expr.line, expr.col)
        return self._bv(0, 8)

    def _encode_arith(self, op: str, a: str, b: str, ctype: ScalarType,
                      expr: A.Expr) -> str:
        w = ctype.width
        signed = ctype.is_signed
        if op == "min":
            return self._ite(self._cmp("lt", a, b, signed), a, b)
        if op == "max":
            return self._ite(self._cmp("gt", a, b, signed), a, b)
        if op == "add":
            res = f"(bvadd {a} {b})"
            ovf = self._signed_add_overflow(a, b, w) if signed else f"(bvult {res} {a})"
            self._trap_update(ovf)
            self._pi_update(self._not(ovf))
            return res
        if op == "sub":
            res = f"(bvsub {a} {b})"
            ovf = self._signed_sub_overflow(a, b, w) if signed else f"(bvult {a} {b})"
            self._trap_update(ovf)
            self._pi_update(self._not(ovf))
            return res
        if op == "mul":
            if ctype.is_decimal:
                return self._decimal_mul(a, b, ctype, expr)
            if signed:
                prod = f"(bvmul {self._extend(a, w, 2 * w, True)} " \
                       f"{self._extend(b, w, 2 * w, True)})"
                low = self._truncate(prod, 2 * w, w)
                ovf = self._not(f"(= {prod} {self._extend(low, w, 2 * w, True)})")
                self._trap_update(ovf)
                self._pi_update(self._not(ovf))
                return low
            prod = f"(bvmul {self._extend(a, w, 2 * w, False)} " \
                   f"{self._extend(b, w, 2 * w, False)})"
            mx = self._bv((1 << w) - 1, 2 * w)
            ovf = f"(bvugt {prod} {mx})"
            self._trap_update(ovf)
            self._pi_update(self._not(ovf))
            return self._truncate(prod, 2 * w, w)
        if op == "div":
            if ctype.is_decimal:
                return self._decimal_div(a, b, ctype, expr)
            zero = self._bv(0, w)
            is_zero = f"(= {b} {zero})"
            self._trap_update(is_zero)
            self._pi_update(self._not(is_zero))
            if signed:
                mn = self._bv(ctype.min_value & ((1 << w) - 1), w)
                neg1 = self._bv((1 << w) - 1, w)
                ovf = self._and(f"(= {a} {mn})", f"(= {b} {neg1})")
                self._trap_update(ovf)
                self._pi_update(self._not(ovf))
                return f"(bvsdiv {a} {b})"
            return f"(bvudiv {a} {b})"
        if op == "rem":
            if ctype.is_decimal:
                self.diags.error("D_BAD_OPERAND",
                                 "rem() is not defined for fixed-point decimals",
                                 expr.line, expr.col)
                return self._bv(0, w)
            zero = self._bv(0, w)
            is_zero = f"(= {b} {zero})"
            self._trap_update(is_zero)
            self._pi_update(self._not(is_zero))
            if signed:
                mn = self._bv(ctype.min_value & ((1 << w) - 1), w)
                neg1 = self._bv((1 << w) - 1, w)
                ovf = self._and(f"(= {a} {mn})", f"(= {b} {neg1})")
                self._trap_update(ovf)
                self._pi_update(self._not(ovf))
                return f"(bvsrem {a} {b})"
            return f"(bvurem {a} {b})"
        return self._bv(0, w)

    def _signed_add_overflow(self, a: str, b: str, w: int) -> str:
        wide = w + 1
        aw = self._extend(a, w, wide, True)
        bw = self._extend(b, w, wide, True)
        res = f"(bvadd {aw} {bw})"
        mn = self._bv(-(1 << (w - 1)) & ((1 << wide) - 1), wide)
        mx = self._bv((1 << (w - 1)) - 1, wide)
        return self._or(f"(bvslt {res} {mn})", f"(bvsgt {res} {mx})")

    def _signed_sub_overflow(self, a: str, b: str, w: int) -> str:
        wide = w + 1
        aw = self._extend(a, w, wide, True)
        bw = self._extend(b, w, wide, True)
        res = f"(bvsub {aw} {bw})"
        mn = self._bv(-(1 << (w - 1)) & ((1 << wide) - 1), wide)
        mx = self._bv((1 << (w - 1)) - 1, wide)
        return self._or(f"(bvslt {res} {mn})", f"(bvsgt {res} {mx})")

    def _decimal_mul(self, a: str, b: str, ctype: ScalarType, expr: A.Expr) -> str:
        w = ctype.width
        scale = 10 ** ctype.scale
        prod = f"(bvmul {self._extend(a, w, 2 * w, True)} " \
               f"{self._extend(b, w, 2 * w, True)})"
        s = self._bv(scale, 2 * w)
        quot = f"(bvsdiv {prod} {s})"
        rem = f"(bvsrem {prod} {s})"
        exact = f"(= {rem} {self._bv(0, 2 * w)})"
        mn = self._bv(ctype.min_value & ((1 << 2 * w) - 1), 2 * w)
        mx = self._bv(ctype.max_value & ((1 << 2 * w) - 1), 2 * w)
        fit = self._and(f"(bvsge {quot} {mn})", f"(bvsle {quot} {mx})")
        ok = self._and(exact, fit)
        self._trap_update(self._not(ok))
        self._pi_update(ok)
        return self._truncate(quot, 2 * w, w)

    def _decimal_div(self, a: str, b: str, ctype: ScalarType, expr: A.Expr) -> str:
        w = ctype.width
        scale = 10 ** ctype.scale
        zero = self._bv(0, w)
        is_zero = f"(= {b} {zero})"
        self._trap_update(is_zero)
        self._pi_update(self._not(is_zero))
        num = f"(bvmul {self._extend(a, w, 2 * w, True)} {self._bv(scale, 2 * w)})"
        quot = f"(bvsdiv {num} {self._extend(b, w, 2 * w, True)})"
        rem = f"(bvsrem {num} {self._extend(b, w, 2 * w, True)})"
        exact = f"(= {rem} {self._bv(0, 2 * w)})"
        mn = self._bv(ctype.min_value & ((1 << 2 * w) - 1), 2 * w)
        mx = self._bv(ctype.max_value & ((1 << 2 * w) - 1), 2 * w)
        fit = self._and(f"(bvsge {quot} {mn})", f"(bvsle {quot} {mx})")
        ok = self._and(exact, fit)
        self._trap_update(self._not(ok))
        self._pi_update(ok)
        return self._truncate(quot, 2 * w, w)

    def _infer_type(self, expr: A.Expr, env: Dict[str, str]) -> ScalarType:
        from .semantic import _TypeChecker
        return _TypeChecker(self.prog, dict(self.type_env)).type_of(expr)

    def _infer_arith_type(self, expr: A.Binary, env: Dict[str, str],
                          expected: Optional[ScalarType]) -> ScalarType:
        if expected is not None:
            return expected
        from .semantic import _TypeChecker
        tc = _TypeChecker(self.prog, dict(self.type_env))
        candidates = []
        for operand in (expr.left, expr.right):
            ct = tc._concrete_type(operand)
            if ct is not None and ct.is_numeric:
                candidates.append(ct)
        if candidates and all(c == candidates[0] for c in candidates):
            return candidates[0]
        return I64

    def _infer_cmp_type(self, expr: A.Binary, env: Dict[str, str],
                        expected: Optional[ScalarType]) -> ScalarType:
        if expected is not None:
            return expected
        from .semantic import _TypeChecker
        tc = _TypeChecker(self.prog, dict(self.type_env))
        candidates = []
        for operand in (expr.left, expr.right):
            ct = tc._concrete_type(operand)
            if ct is not None:
                candidates.append(ct)
        if candidates and all(c == candidates[0] for c in candidates):
            return candidates[0]
        return I64

    # --- postcondition -----------------------------------------------------
    def _build_post_condition(self) -> None:
        self.silent = True
        parts: List[str] = []
        for name, expr in self.tr.decl.ensures:
            parts.append(self._encode_expr(expr, self.env, "post", BOOL))
        for inv in self.prog.invariants.values():
            tname = inv.quant_table
            if tname is None:
                parts.append(self._encode_expr(inv.expr, {}, "post", BOOL))
                continue
            if tname not in self.touched_tables:
                continue
            for slot in self.modified_slots.get(tname, []):
                occ = self._current_occ(tname, slot)
                body = self._encode_forall_body(inv.expr, tname, inv.quant_var,
                                                slot, "post")
                parts.append(f"(=> {occ} {body})")
            w = self.witness_slots.get(tname)
            if w is not None:
                occ = self._current_occ(tname, w)
                body = self._encode_forall_body(inv.expr, tname, inv.quant_var,
                                                w, "post")
                parts.append(f"(=> {occ} {body})")
        # aggregate consistency: exact post sum rebuilt from pre-agg + deltas
        for (tname, cname), wide_w in self.agg_wide.items():
            if not self.modified_slots.get(tname):
                continue
            ctype = self.prog.tables[tname].columns[cname].type
            signed = ctype.is_signed
            fv = self.final_values.get((tname, cname), {})
            pv = self.slot_pre_value.get((tname, cname), {})
            deltas: List[str] = []
            for slot, final in fv.items():
                w_final = self._extend(final, ctype.width, wide_w, signed)
                w_pre = self._extend(pv[slot], ctype.width, wide_w, signed)
                deltas.append(f"(bvsub {w_final} {w_pre})")
            if deltas:
                total = deltas[0]
                for d in deltas[1:]:
                    total = f"(bvadd {total} {d})"
                exact = f"(bvadd {self.pre_agg[tname][cname]} {total})"
            else:
                exact = self.pre_agg[tname][cname]
            parts.append(f"(= {self.cur_agg[tname][cname]} {exact})")
        self.post_condition = self._and(*parts) if parts else "true"
        self.silent = False

    def _build_obligations(self) -> None:
        pre_parts = [a[len("(assert "):-1] for a in self.common_asserts]
        req_parts = [self._encode_expr(e, self.params, "pre", BOOL)
                     for _, e in self.tr.decl.requires]
        self.precondition = self._and(*req_parts) if req_parts else "true"
        common_body = self._and(*(pre_parts + [self.precondition]))
        self.declare("common_0", "(declare-const common_0 Bool)", ("bool",))
        self.assert_common(f"(= common_0 {common_body})")
        common = "common_0"
        self.obligation_asserts["safety"] = \
            f"(and {common} {self.final_pi} (not {self.post_condition}))"
        self.obligation_asserts["panic"] = \
            f"(and {common} {self.final_trap})"
        self.obligation_asserts["liveness"] = \
            f"(and {common} {self.final_pi})"

    # --- script ------------------------------------------------------------
    def script_for(self, obligation: Optional[str] = None) -> str:
        lines = ["(set-logic QF_ABV)"]
        lines.extend(self.decls)
        lines.extend(self.common_asserts)
        if obligation is None:
            for name in ("safety", "panic", "liveness"):
                lines.append(f"(assert {self.obligation_asserts[name]})")
        else:
            lines.append(f"(assert {self.obligation_asserts[obligation]})")
        return "\n".join(lines) + "\n"

    def script_full(self) -> str:
        lines = ["(set-logic QF_ABV)"]
        lines.extend(self.decls)
        lines.extend(self.common_asserts)
        for name, label in (("safety", "Obligation 1: safety"),
                            ("panic", "Obligation 2: panic-free"),
                            ("liveness", "Obligation 3: liveness")):
            lines.append(f"; ---- {label} ----")
            lines.append("(push)")
            lines.append(f"(assert {self.obligation_asserts[name]})")
            lines.append("(check-sat)")
            lines.append("(pop)")
        return "\n".join(lines) + "\n"

    # --- solving -----------------------------------------------------------
    def solve(self) -> VerificationResult:
        result = VerificationResult(transition=self.tr_name,
                                    safety=ObligationResult("safety"),
                                    panic=ObligationResult("panic"),
                                    liveness=ObligationResult("liveness"),
                                    script=self.script_full())
        for name in ("safety", "panic", "liveness"):
            setattr(result, name, self._solve_one(name))
        result.diagnostics = list(self.diags.items)
        return result

    def _solve_one(self, name: str) -> ObligationResult:
        res = ObligationResult(name)
        start = time.perf_counter()
        try:
            text = self.script_for(name)
            assertions = z3.parse_smt2_string(text)
            solver = z3.Solver()
            solver.set("timeout", self.config.timeout_ms)
            solver.add(assertions)
            chk = solver.check()
            res.elapsed_ms = (time.perf_counter() - start) * 1000.0
            if chk == z3.unsat:
                res.status = "unsat"
            elif chk == z3.sat:
                res.status = "sat"
                res.model = solver.model()
                res.solver = solver
            elif chk == z3.unknown:
                res.status = "unknown"
                res.solver = solver
            else:
                res.status = "error"
        except z3.Z3Exception as exc:
            res.elapsed_ms = (time.perf_counter() - start) * 1000.0
            res.status = "error"
            self.diags.error("D_SOLVER", f"z3 error in {name}: {exc}")
        return res


_SYMBOL_INTERNER: Dict[str, int] = {}


def _intern_symbol(text: str) -> int:
    if text not in _SYMBOL_INTERNER:
        _SYMBOL_INTERNER[text] = len(_SYMBOL_INTERNER)
    return _SYMBOL_INTERNER[text]


def verify_transition(prog: Program, transition_name: str,
                      config: SmtConfig = None) -> VerificationResult:
    encoder = SmtEncoder(prog, transition_name, config)
    encoder.build()
    return encoder.solve()


def verify_transition_dual(prog: Program, transition_name: str,
                           config: SmtConfig = None) -> VerificationResult:
    """Two-mode verification.

    Obligation 1 (safety) and Obligation 3 (liveness) are checked with the
    bounded-free-accumulator abstraction, which is exact for relative ensures
    and postcondition reasoning.  Obligation 2 (panic freedom) is checked with
    the exact O(C) column sum, because the intermediate tracked-sum range check
    genuinely depends on the whole table.
    """
    from dataclasses import replace
    config = config or SmtConfig()
    free_cfg = replace(config, exact_sum=False)
    exact_cfg = replace(config, exact_sum=True)
    free = SmtEncoder(prog, transition_name, free_cfg)
    free.build()
    exact = SmtEncoder(prog, transition_name, exact_cfg)
    exact.build()
    result = VerificationResult(transition=transition_name,
                                safety=ObligationResult("safety"),
                                panic=ObligationResult("panic"),
                                liveness=ObligationResult("liveness"))
    result.safety = free._solve_one("safety")
    result.liveness = free._solve_one("liveness")
    result.panic = exact._solve_one("panic")
    result.script = (free.script_full()
                     + "\n; ---- Obligation 2 with exact column sums ----\n"
                     + exact.script_for("panic"))
    result.diagnostics = list(free.diags.items) + list(exact.diags.items)
    result.encoders = {"safety": free, "panic": exact, "liveness": free}
    return result


def verify_module(prog: Program, config: SmtConfig = None,
                  dual: bool = True) -> List[VerificationResult]:
    fn = verify_transition_dual if dual else verify_transition
    return [fn(prog, name, config) for name in prog.transitions]
