"""Target 1: AST -> SMT-LIB 2.6 (QF_AUFBV) symbolic verification engine.

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

from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Set, Tuple
import time

import z3

from . import ast_nodes as A
from .errors import Diagnostic, DiagnosticBag, ERR_VACUOUS_IMPLEMENTATION
from .semantic import Program, TransitionInfo
from .types import BOOL, I64, MAX_PROBES, ScalarType, id64, slot_bits_for


@dataclass
class SmtConfig:
    timeout_ms: int = 500
    max_unroll: int = 1024
    ite_chain_limit: int = 256
    wide_extra_bits: int = 1
    # When False (default), the pre-state aggregate accumulator is modeled as a
    # free scalar constrained to the column domain, which is exactly the runtime
    # guarantee (the compiler reverts on accumulator overflow).  When True, the
    # accumulator is additionally tied to the exact O(C) column sum, which is
    # stronger but costs solver time proportional to capacity.
    exact_sum: bool = False
    # When True (default), tracked_sum pre-aggregates use the frame-decomposed
    # symbolic sum (sigma_local over the touched footprint + a bounded symbolic
    # rest frame), which is O(|K_rw|) in capacity instead of O(C).  Falls back
    # to the free/exact modes when a table has wildcard or Tier-2 accesses.
    frame_sum: bool = True


@dataclass
class ObligationResult:
    name: str
    status: str = "pending"  # unsat | sat | unknown | timeout | error
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
        return (
            not any(d.severity == "error" for d in self.diagnostics)
            and self.safety.status == "unsat"
            and self.panic.status == "unsat"
            and self.liveness.status == "sat"
        )

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
            if any(d.severity == "error" for d in self.diagnostics):
                lines.append("  VERDICT: ENCODING OR SOLVER ERROR")
                lines.extend(
                    "  " + d.render() for d in self.diagnostics if d.severity == "error"
                )
            elif self.liveness.status == "unsat":
                lines.append(
                    f"  VERDICT: {ERR_VACUOUS_IMPLEMENTATION} "
                    f"(transition reverts on 100% of valid precondition states)"
                )
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
    pre_generations: Dict[str, List[int]] = field(default_factory=dict)
    trace: List[Dict[str, object]] = field(default_factory=list)
    violated: Dict[str, object] = field(default_factory=dict)
    directive: str = ""

    def render(self) -> str:
        out = [
            f"[VERIFICATION FAILED: {self.obligation}]",
            f"Transition:   {self.transition}",
            f"Violated Law: {self.law}",
            "",
            "MINIMAL COUNTEREXAMPLE WITNESS:",
        ]
        out.append("1. Pre-State (Sigma_0):")
        if not self.pre_state:
            out.append("   (empty)")
        for table, rows in self.pre_state.items():
            for row in rows:
                vals = ", ".join(f"{k}: {v}" for k, v in row.items())
                out.append(f"   - {table}[id={row.get('id')}] = {{ {vals} }}")
        for table, generations in self.pre_generations.items():
            live_slots = {
                int(row["id"]) & (len(generations) - 1)
                for row in self.pre_state.get(table, [])
            }
            nonzero_free = {
                i: g for i, g in enumerate(generations) if g and i not in live_slots
            }
            if nonzero_free:
                out.append(f"   - {table} unused-slot generations: {nonzero_free}")
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
        if self.config.exact_sum and self.config.frame_sum:
            from dataclasses import replace

            self.config = replace(self.config, frame_sum=False)
        self.diags = DiagnosticBag()

        self.decls: List[str] = []
        self.common_asserts: List[str] = []
        self.obligation_asserts: Dict[str, str] = {}
        self.const_kinds: Dict[str, Tuple] = {}
        self._declared: Set[str] = set()
        self._version: Dict[str, int] = {}
        # Source names are separate namespaces. Concatenating them would alias
        # A.b_c with A_b.c, or a column named "occ" with the occupancy array.
        # Stable ordinal bases keep all state symbols independent of spelling.
        self._table_symbols = {name: f"t{i}" for i, name in enumerate(prog.tables)}
        self._column_symbols = {
            (name, column): f"{self._table_symbols[name]}_c{i}"
            for name, info in prog.tables.items()
            for i, column in enumerate(info.columns)
        }

        # pre-state function names
        self.pre_occ: Dict[str, str] = {}
        self.cur_occ: Dict[str, str] = {}
        self.pre_gen: Dict[str, str] = {}
        self.pre_col: Dict[str, Dict[str, str]] = {}
        self.pre_hkey: Dict[Tuple[str, str], str] = {}
        self.cur_hkey: Dict[Tuple[str, str], str] = {}
        self.pre_hused: Dict[Tuple[str, str], str] = {}
        self.cur_hused: Dict[Tuple[str, str], str] = {}
        self.pre_agg: Dict[str, Dict[str, str]] = {}
        self.cur_agg: Dict[str, Dict[str, str]] = {}
        self.agg_wide: Dict[Tuple[str, str], int] = {}
        self.agg_signed: Dict[Tuple[str, str], bool] = {}
        # frame-decomposed aggregate state
        self.frame_ok: Dict[Tuple[str, str], bool] = {}
        self.frame_rest: Dict[Tuple[str, str], str] = {}
        self.frame_nrest: Dict[Tuple[str, str], str] = {}
        self.frame_nbits: Dict[Tuple[str, str], int] = {}
        self.frame_local_pre: Dict[Tuple[str, str], str] = {}
        self.frame_local_post: Dict[Tuple[str, str], str] = {}
        self.frame_keys: Dict[Tuple[str, str], List[str]] = {}
        self.frame_m: Dict[Tuple[str, str], str] = {}
        self.agg_bounds: Dict[Tuple[str, str], Tuple[int, int]] = {}

        # writes / allocations
        self.writes: Dict[Tuple[str, str], List[Tuple[str, str]]] = {}
        self.cur_widx: Dict[Tuple[str, str], str] = {}
        self.cur_wval: Dict[Tuple[str, str], str] = {}
        self.write_counter: Dict[Tuple[str, str], int] = {}
        self.alloc_slots: Dict[str, str] = {}
        self.idx_bits: Dict[Tuple[str, str], int] = {}
        self.alloc_records: Dict[str, List[Tuple[str, str]]] = {}
        self.known_unique = {}
        self.lookup_results = {}
        self._hash_lookups: Dict[Tuple[str, ...], Tuple[str, str]] = {}
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
        self.active = "true"
        self.spec_checks = None
        # marker-edit
        self.final_pi = "pi_0"
        self.final_trap = "trap_0"
        self.post_condition = "true"
        self.precondition = "true"

    # --- SMT-LIB primitives -------------------------------------------------
    def _table_symbol(self, table: str) -> str:
        return self._table_symbols[table]

    def _column_symbol(self, table: str, column: str) -> str:
        return self._column_symbols[(table, column)]

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

    def _select_arr(self, arr: str, idx: str) -> str:
        return f"(select {arr} {idx})"

    def _store_arr(self, arr: str, idx: str, val: str) -> str:
        return f"(store {arr} {idx} {val})"

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
        fn = {
            "lt": "bvult" if not signed else "bvslt",
            "le": "bvule" if not signed else "bvsle",
            "gt": "bvugt" if not signed else "bvsgt",
            "ge": "bvuge" if not signed else "bvsge",
        }[op]
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
        if self.prog.tables[table].layout.capacity == 1:
            return self._bv(0, sbits)
        if sbits == 64:
            return key_expr
        return f"((_ extract {sbits - 1} 0) {key_expr})"

    # --- entry -------------------------------------------------------------
    def build(self) -> None:
        self.table_slot_bits = {
            name: slot_bits_for(t.layout.capacity)
            for name, t in self.prog.tables.items()
        }
        self.slot_bits = max(self.table_slot_bits.values(), default=1)
        self._build_state()
        self._build_well_formedness()
        self._encode_body()
        self._build_frame()
        self._relevant = self._relevant_slots()
        self._build_pre_invariants()
        self._build_post_condition()
        self._build_obligations()

    def _build_state(self) -> None:
        for p in self.tr.decl.params:
            ctype = self.tr.param_types[p.name]
            name = f"p_{p.name}"
            self.declare(
                name,
                f"(declare-const {name} {self._sort_of(ctype)})",
                self._kind_of(ctype),
            )
            self.params[p.name] = name
            if ctype.kind == "enum":
                self.assert_common(
                    f"(bvule {name} {self._bv(ctype.max_value, ctype.width)})"
                )
        for tname, tinfo in self.prog.tables.items():
            cap = tinfo.layout.capacity
            sbits = self.table_slot_bits[tname]
            gen_bits = min(64 - tinfo.layout.key_bits, 32)
            occ = f"{self._table_symbol(tname)}_occ"
            self.declare(
                occ,
                f"(declare-const {occ} (Array (_ BitVec {sbits}) Bool))",
                ("array", sbits, "bool"),
            )
            gen = f"{self._table_symbol(tname)}_gen"
            self.declare(
                gen,
                f"(declare-fun {gen} ((_ BitVec {sbits})) (_ BitVec {gen_bits}))",
                ("fun", sbits, gen_bits),
            )
            self.pre_occ[tname] = occ
            self.cur_occ[tname] = occ
            self.pre_gen[tname] = gen
            self.pre_col[tname] = {}
            for cname, cinfo in tinfo.columns.items():
                col = self._column_symbol(tname, cname)
                self.declare(
                    col,
                    f"(declare-fun {col} ((_ BitVec {sbits})) {self._sort_of(cinfo.type)})",
                    ("fun", sbits, "bool" if cinfo.type.is_bool else cinfo.type.width),
                )
                self.pre_col[tname][cname] = col
                n_writes = max(1, self.tr.wcet_bound)
                ib = max(1, n_writes.bit_length())
                self.idx_bits[(tname, cname)] = ib
                widx = f"{self._column_symbol(tname, cname)}_widx"
                self.declare(
                    widx,
                    f"(declare-const {widx} (Array (_ BitVec {sbits}) (_ BitVec {ib})))",
                    ("array", sbits, ib),
                )
                wval = f"{self._column_symbol(tname, cname)}_wval"
                self.declare(
                    wval,
                    f"(declare-const {wval} (Array (_ BitVec {ib}) {self._sort_of(cinfo.type)}))",
                    ("array", ib, "bool" if cinfo.type.is_bool else cinfo.type.width),
                )
                self.cur_widx[(tname, cname)] = widx
                self.cur_wval[(tname, cname)] = wval
                self.write_counter[(tname, cname)] = 0
                # The pre-state write maps are empty: widx is the constant zero
                # array and wval is never read at index zero.
                self.assert_common(
                    f"(= {widx} ((as const (Array (_ BitVec {sbits}) (_ BitVec {ib}))) "
                    f"{self._bv(0, ib)}))"
                )
                self.assert_common(
                    f"(= {wval} ((as const (Array (_ BitVec {ib}) "
                    f"{self._sort_of(cinfo.type)})) {self._zero(cinfo.type)}))"
                )
            for cname, cinfo in tinfo.columns.items():
                if cinfo.unique:
                    B = tinfo.layout.unique_hashes[cname]
                    bb = max(1, B.bit_length() - 1)
                    hkey = f"{self._column_symbol(tname, cname)}_hkey"
                    self.declare(
                        hkey,
                        f"(declare-const {hkey} (Array (_ BitVec {bb}) (_ BitVec {sbits})))",
                        ("array", bb, sbits),
                    )
                    hused = f"{self._column_symbol(tname, cname)}_hused"
                    self.declare(
                        hused,
                        f"(declare-const {hused} (Array (_ BitVec {bb}) Bool))",
                        ("array", bb, "bool"),
                    )
                    self.pre_hkey[(tname, cname)] = hkey
                    self.cur_hkey[(tname, cname)] = hkey
                    self.pre_hused[(tname, cname)] = hused
                    self.cur_hused[(tname, cname)] = hused
            if tinfo.layout.agg_offsets:
                self._compute_agg_bounds()
                self.pre_agg[tname] = {}
                self.cur_agg[tname] = {}
                from .types import accumulator_type

                for cname in tinfo.layout.agg_offsets:
                    ctype = tinfo.columns[cname].type
                    wide_w = accumulator_type(ctype, cap).width
                    self.agg_wide[(tname, cname)] = wide_w
                    self.agg_signed[(tname, cname)] = ctype.is_signed
                    agg = self._fresh(
                        f"{self._column_symbol(tname, cname)}_agg",
                        f"(declare-const {self._column_symbol(tname, cname)}_agg_0 (_ BitVec {wide_w}))",
                        ("bv", wide_w),
                    )
                    self.pre_agg[tname][cname] = agg
                    self.cur_agg[tname][cname] = agg
                    n_bits = max(1, cap.bit_length())
                    use_frame = self.config.frame_sum and self._frame_possible(tname)
                    self.frame_ok[(tname, cname)] = use_frame
                    if use_frame:
                        rest = self._fresh(
                            f"{self._column_symbol(tname, cname)}_rest",
                            f"(declare-const {self._column_symbol(tname, cname)}_rest_0 (_ BitVec {wide_w}))",
                            ("bv", wide_w),
                        )
                        nrest = self._fresh(
                            f"{self._column_symbol(tname, cname)}_nrest",
                            f"(declare-const {self._column_symbol(tname, cname)}_nrest_0 (_ BitVec {n_bits}))",
                            ("bv", n_bits),
                        )
                        self.frame_rest[(tname, cname)] = rest
                        self.frame_nrest[(tname, cname)] = nrest
                        self.frame_nbits[(tname, cname)] = n_bits
                    elif self.config.exact_sum:
                        exact = self._exact_sum(
                            tname, self.pre_col[tname][cname], ctype, cap, wide_w
                        )
                        self.assert_common(f"(= {agg} {exact})")
                    else:
                        self.assert_common(
                            self._agg_domain_check(agg, wide_w, cap, tname, cname)
                        )

    def _pre_occ(self, key: str, tname: str) -> str:
        return self._select_arr(self.pre_occ[tname], self._slot_of_key(key, tname))

    def _add_all(self, terms: List[str], width: int) -> str:
        if not terms:
            return self._bv(0, width)
        acc = terms[0]
        for t in terms[1:]:
            acc = f"(bvadd {acc} {t})"
        return acc

    def _bound_mul(self, nrest: str, const: int, wide_w: int, n_bits: int) -> str:
        """n_rest * const as a signed wide product (n_rest is non-negative)."""
        ext = self._extend(nrest, n_bits, wide_w, False)
        lit = self._bv(const & ((1 << wide_w) - 1), wide_w)
        return f"(bvmul {ext} {lit})"

    def _agg_domain_check(
        self, expr: str, wide_w: int, cap: int, tname: str, cname: str
    ) -> str:
        """The tracked_sum accumulator's domain is [C*L_c, C*U_c], the range of
        possible sums under the invariant-derived per-row interval."""
        ctype = self.prog.tables[tname].columns[cname].type
        lo_b, hi_b = ctype.min_value, ctype.max_value
        mn = (lo_b * cap) & ((1 << wide_w) - 1)
        mx = (hi_b * cap) & ((1 << wide_w) - 1)
        if ctype.is_signed:
            return self._and(
                f"(bvsge {expr} {self._bv(mn, wide_w)})",
                f"(bvsle {expr} {self._bv(mx, wide_w)})",
            )
        return self._and(
            f"(bvuge {expr} {self._bv(0, wide_w)})",
            f"(bvule {expr} {self._bv(mx, wide_w)})",
        )

    def _is_col_access(self, expr: A.Expr, qvar: str, tname: str, cname: str) -> bool:
        return (
            isinstance(expr, A.TableAccess)
            and expr.table == tname
            and expr.column == cname
            and isinstance(expr.key, A.Var)
            and expr.key.name == qvar
        )

    def _const_eval(self, expr: A.Expr):
        """Use the language's checked, typed arithmetic for interval bounds.

        Host integer arithmetic is not interchangeable here: signed division
        truncates toward zero and decimal operations preserve their scale.
        An incorrect bound can exclude valid states from a universal proof.
        """
        from .runtime import Revert, State, Transaction

        def constant(node):
            if isinstance(node, A.Literal):
                return True
            if isinstance(node, A.Unary):
                return constant(node.operand)
            if isinstance(node, A.Binary):
                return constant(node.left) and constant(node.right)
            if isinstance(node, A.Select):
                return constant(node.cond) and constant(node.a) and constant(node.b)
            return False

        if not constant(expr):
            return None
        try:
            value = Transaction(self.prog, State(), "_constant", {}).value_of(expr)
        except Revert:
            return None
        return value if type(value) is int else None

    def _tighten(
        self, expr: A.Expr, qvar: str, tname: str, cname: str, bounds: List[int]
    ) -> None:
        """Spec 6.2 Step 3: extract the per-row interval implied by a
        forall-invariant on T[q].c."""
        if isinstance(expr, A.Binary):
            if expr.op == "and":
                self._tighten(expr.left, qvar, tname, cname, bounds)
                self._tighten(expr.right, qvar, tname, cname, bounds)
                return
            op = expr.op
            other = None
            if self._is_col_access(expr.left, qvar, tname, cname):
                other = expr.right
            elif self._is_col_access(expr.right, qvar, tname, cname):
                other = expr.left
                op = {"ge": "le", "le": "ge", "gt": "lt", "lt": "gt"}.get(op, op)
            if other is None:
                return
            val = self._const_eval(other)
            if val is None:
                return
            if op == "ge":
                bounds[0] = max(bounds[0], val)
            elif op == "gt":
                bounds[0] = max(bounds[0], val + 1)
            elif op == "le":
                bounds[1] = min(bounds[1], val)
            elif op == "lt":
                bounds[1] = min(bounds[1], val - 1)
            elif op == "eq":
                bounds[0] = max(bounds[0], val)
                bounds[1] = min(bounds[1], val)

    def _compute_agg_bounds(self) -> None:
        for tname, tinfo in self.prog.tables.items():
            for cname, cinfo in tinfo.columns.items():
                if not cinfo.tracked_sum:
                    continue
                bounds = [cinfo.type.min_value, cinfo.type.max_value]
                for inv in self.prog.invariants.values():
                    if inv.quant_table == tname:
                        self._tighten(inv.expr, inv.quant_var, tname, cname, bounds)
                self.agg_bounds[(tname, cname)] = (bounds[0], bounds[1])

    def _frame_possible(self, tname: str) -> bool:
        """The frame is built when every *write* to `tname` has a statically
        resolvable primary-key handle.  Reads may be skipped (they cannot change
        the sum); allocation slots are fresh primary keys."""
        for rec in self.tr.reads + self.tr.writes:
            if rec.table == tname and rec.key is not None and rec.tier != 1:
                return False
        return True

    def _collect_frame_keys(self, tname: str) -> Optional[List[str]]:
        """Primary-key expressions accessed in `tname`, or None when a write key
        cannot be established (the frame would be unsound)."""
        keys: List[str] = []
        seen = set()

        def add(expr: str) -> None:
            if expr not in seen:
                seen.add(expr)
                keys.append(expr)

        # Reads: only primary-key handles can index the local sum.
        for rec in self.tr.reads:
            if rec.table != tname or rec.key is None:
                continue
            try:
                kt = self._infer_type(rec.key, self.env)
            except Exception:
                continue
            if kt.kind != "id64" or kt.key_table != tname:
                continue
            key = self._encode_key_simple(rec.key, self.env)
            if key is not None:
                add(key)
        # Writes: every write key must be resolvable to a primary key.
        for rec in self.tr.writes:
            if rec.table != tname or rec.key is None:
                continue
            try:
                kt = self._infer_type(rec.key, self.env)
            except Exception:
                return None
            if kt.kind != "id64" or kt.key_table != tname:
                return None
            key = self._encode_key_simple(rec.key, self.env)
            if key is None:
                return None
            add(key)
        # Allocation slots are fresh primary keys.
        tinfo = self.prog.tables[tname]
        kb = tinfo.layout.key_bits
        gen_bits = tinfo.layout.generation_bits
        for slot, new_gen in self.alloc_records.get(tname, []):
            raw = new_gen if kb == 0 else f"(concat {new_gen} {slot})"
            add(self._extend(raw, gen_bits + kb, 64, False))
        return keys

    def _delta_terms(self, keys: List[str], tname: str, occ_fn) -> List[str]:
        deltas: List[str] = []
        for i, k in enumerate(keys):
            occ = occ_fn(k)
            if i == 0:
                deltas.append(occ)
            else:
                slot = self._slot_of_key(k, tname)
                ne = [
                    f"(not (= {slot} {self._slot_of_key(keys[j], tname)}))"
                    for j in range(i)
                ]
                deltas.append(self._and(occ, *ne))
        return deltas

    def _build_frame(self) -> None:
        """Frame-decomposed pre-aggregate (Spec 6.2).

        sigma_0 = sigma_local,0 + sigma_rest, where sigma_local,0 is the exact
        deduplicated sum over the touched footprint K_rw and sigma_rest is a
        symbolic scalar bounded by the invariant-derived convex hull
        n_rest * L_c <= sigma_rest <= n_rest * U_c.  This keeps Footprint
        Localization sound without instantiating all C slots.
        """
        for tname, tinfo in self.prog.tables.items():
            if not tinfo.layout.agg_offsets:
                continue
            cap = tinfo.layout.capacity
            keys = self._collect_frame_keys(tname)
            for cname in tinfo.layout.agg_offsets:
                if not self.frame_ok.get((tname, cname)):
                    continue
                ctype = tinfo.columns[cname].type
                wide_w = self.agg_wide[(tname, cname)]
                if keys is None:
                    # A write key could not be resolved to a primary key: the
                    # frame would be unsound, so fall back to the naive modes.
                    self.frame_ok[(tname, cname)] = False
                    if self.config.exact_sum:
                        exact = self._exact_sum(
                            tname, self.pre_col[tname][cname], ctype, cap, wide_w
                        )
                        self.assert_common(f"(= {self.pre_agg[tname][cname]} {exact})")
                    else:
                        self.assert_common(
                            self._agg_domain_check(
                                self.pre_agg[tname][cname], wide_w, cap, tname, cname
                            )
                        )
                    continue
                signed = self.agg_signed[(tname, cname)]
                n_bits = self.frame_nbits[(tname, cname)]
                rest = self.frame_rest[(tname, cname)]
                nrest = self.frame_nrest[(tname, cname)]
                self.frame_keys[(tname, cname)] = keys
                deltas = self._delta_terms(
                    keys, tname, lambda k: self._pre_occ(k, tname)
                )
                m_terms = [
                    self._ite(d, self._bv(1, n_bits), self._bv(0, n_bits))
                    for d in deltas
                ]
                m_local = self._add_all(m_terms, n_bits)
                self.frame_m[(tname, cname)] = m_local
                terms = []
                for d, k in zip(deltas, keys):
                    slot = self._slot_of_key(k, tname)
                    v = self._apply(self.pre_col[tname][cname], slot)
                    terms.append(
                        self._ite(
                            d,
                            self._extend(v, ctype.width, wide_w, signed),
                            self._bv(0, wide_w),
                        )
                    )
                local_pre = self._add_all(terms, wide_w)
                self.frame_local_pre[(tname, cname)] = local_pre
                self.assert_common(
                    f"(= {self.pre_agg[tname][cname]} (bvadd {local_pre} {rest}))"
                )
                lo_b, hi_b = self.agg_bounds.get(
                    (tname, cname), (ctype.min_value, ctype.max_value)
                )
                lo = self._bound_mul(nrest, lo_b, wide_w, n_bits)
                hi = self._bound_mul(nrest, hi_b, wide_w, n_bits)
                self.assert_common(f"(bvsle {lo} {rest})")
                self.assert_common(f"(bvsle {rest} {hi})")
                # Every allocation consumes a free slot, so the untouched frame
                # must leave room for all of them.
                room = f"(bvsub {self._bv(cap, n_bits)} {m_local})"
                self.assert_common(f"(bvule {nrest} {room})")

    def _frame_local_post(self, tname: str, cname: str) -> str:
        ctype = self.prog.tables[tname].columns[cname].type
        wide_w = self.agg_wide[(tname, cname)]
        signed = self.agg_signed[(tname, cname)]
        keys = self.frame_keys.get((tname, cname), [])
        deltas = self._delta_terms(
            keys, tname, lambda k: self._current_occ(tname, self._slot_of_key(k, tname))
        )
        terms = []
        for d, k in zip(deltas, keys):
            v = self._current_col(tname, cname, k, ctype)
            terms.append(
                self._ite(
                    d, self._extend(v, ctype.width, wide_w, signed), self._bv(0, wide_w)
                )
            )
        return self._add_all(terms, wide_w)

    def _wide_width(self, tinfo) -> int:
        from .types import accumulator_type

        return max(
            (
                accumulator_type(tinfo.columns[c].type, tinfo.layout.capacity).width
                for c in tinfo.layout.agg_offsets
            ),
            default=128,
        )

    def _exact_sum(
        self, tname: str, fn: str, ctype: ScalarType, capacity: int, wide_w: int
    ) -> str:
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
        return self._select_arr(self.cur_occ[table], slot)

    def _current_gen(self, table: str, slot: str) -> str:
        expr = self._apply(self.pre_gen[table], slot)
        for alloc_slot, new_gen in self.alloc_records.get(table, []):
            expr = self._ite(f"(= {slot} {alloc_slot})", new_gen, expr)
        return expr

    def _current_col(
        self, table: str, cname: str, key_expr: str, ctype: ScalarType
    ) -> str:
        # O(1) last-write-wins lookup: widx[slot] is the last write index (0 =
        # never written), wval[idx] is the value written there.
        slot = self._slot_of_key(key_expr, table)
        ib = self.idx_bits[(table, cname)]
        widx = self._select_arr(self.cur_widx[(table, cname)], slot)
        wval = self._select_arr(self.cur_wval[(table, cname)], widx)
        base = self._apply(self.pre_col[table][cname], slot)
        return self._ite(f"(= {widx} {self._bv(0, ib)})", base, wval)

    def _current_col_slot(
        self, table: str, cname: str, slot: str, ctype: ScalarType
    ) -> str:
        ib = self.idx_bits[(table, cname)]
        widx = self._select_arr(self.cur_widx[(table, cname)], slot)
        wval = self._select_arr(self.cur_wval[(table, cname)], widx)
        base = self._apply(self.pre_col[table][cname], slot)
        return self._ite(f"(= {widx} {self._bv(0, ib)})", base, wval)

    def _record_write(self, table: str, cname: str, slot: str, value: str) -> None:
        ib = self.idx_bits[(table, cname)]
        idx = self.write_counter.get((table, cname), 0) + 1
        self.write_counter[(table, cname)] = idx
        base = f"{self._column_symbol(table, cname)}_widx"
        new_widx = self._fresh(
            base,
            f"(declare-const {base}_{self._version.get(base, 0)} (Array (_ BitVec {self.table_slot_bits[table]}) (_ BitVec {ib})))",
            ("array", self.table_slot_bits[table], ib),
        )
        self.assert_common(
            f"(= {new_widx} {self._store_arr(self.cur_widx[(table, cname)], slot, self._bv(idx, ib))})"
        )
        self.cur_widx[(table, cname)] = new_widx
        base = f"{self._column_symbol(table, cname)}_wval"
        new_wval = self._fresh(
            base,
            f"(declare-const {base}_{self._version.get(base, 0)} (Array (_ BitVec {ib}) {self._sort_of(self.prog.tables[table].columns[cname].type)}))",
            (
                "array",
                ib,
                "bool"
                if self.prog.tables[table].columns[cname].type.is_bool
                else self.prog.tables[table].columns[cname].type.width,
            ),
        )
        self.assert_common(
            f"(= {new_wval} {self._store_arr(self.cur_wval[(table, cname)], self._bv(idx, ib), value)})"
        )
        self.cur_wval[(table, cname)] = new_wval

    def _current_valid(self, key_expr: str, table: str) -> str:
        slot = self._slot_of_key(key_expr, table)
        occ = self._current_occ(table, slot)
        gen = self._current_gen(table, slot)
        shift = self.prog.tables[table].layout.key_bits
        gen_bits = min(64 - shift, 32)
        expected = f"((_ extract {shift + gen_bits - 1} {shift}) {key_expr})"
        high_bits = 64 - (shift + gen_bits)
        if high_bits > 0:
            high = f"((_ extract 63 {shift + gen_bits}) {key_expr})"
            return self._and(
                occ, f"(= {gen} {expected})", f"(= {high} {self._bv(0, high_bits)})"
            )
        return self._and(occ, f"(= {gen} {expected})")

    def _pre_valid(self, key_expr: str, table: str) -> str:
        slot = self._slot_of_key(key_expr, table)
        occ = self._select_arr(self.pre_occ[table], slot)
        gen = self._apply(self.pre_gen[table], slot)
        shift = self.prog.tables[table].layout.key_bits
        gen_bits = min(64 - shift, 32)
        expected = f"((_ extract {shift + gen_bits - 1} {shift}) {key_expr})"
        high_bits = 64 - (shift + gen_bits)
        if high_bits > 0:
            high = f"((_ extract 63 {shift + gen_bits}) {key_expr})"
            return self._and(
                occ, f"(= {gen} {expected})", f"(= {high} {self._bv(0, high_bits)})"
            )
        return self._and(occ, f"(= {gen} {expected})")

    # --- secondary unique-key index (linear probing, P_max bounded) ---------
    def _hash_int_smt(self, ctype: ScalarType, expr: str) -> str:
        if ctype.is_bool:
            return self._ite(expr, self._bv(1, 64), self._bv(0, 64))
        if ctype.width > 64:
            return f"((_ extract 63 0) {expr})"
        if ctype.width < 64:
            return self._extend(expr, ctype.width, 64, False)
        return expr

    def _mix64_smt(self, x: str) -> str:
        t = f"(bvxor {x} (bvlshr {x} {self._bv(30, 64)}))"
        t = f"(bvmul {t} {self._bv(0xBF58476D1CE4E5B9, 64)})"
        t = f"(bvxor {t} (bvlshr {t} {self._bv(27, 64)}))"
        t = f"(bvmul {t} {self._bv(0x94D049BB133111EB, 64)})"
        return f"(bvxor {t} (bvlshr {t} {self._bv(31, 64)}))"

    def _bucket_smt(self, ctype: ScalarType, value_expr: str, buckets: int) -> str:
        h = self._mix64_smt(self._hash_int_smt(ctype, value_expr))
        bb = max(1, buckets.bit_length() - 1)
        return f"((_ extract {bb - 1} 0) (bvand {h} {self._bv(buckets - 1, 64)}))"

    def _bind(self, base: str, value: str, sort: str, kind: Tuple) -> str:
        version = self._version.get(base, 0)
        name = self._fresh(base, f"(declare-const {base}_{version} {sort})", kind)
        self.assert_common(f"(= {name} {value})")
        return name

    def _hash_lookup_smt(
        self,
        tname: str,
        cname: str,
        value_expr: str,
        ctype: ScalarType,
        state: str = "post",
    ):
        known = self.known_unique.get((tname, cname), []) if state == "post" else []
        for value, slot, guard in reversed(known):
            if value == value_expr and guard in ("true", self.active):
                return slot, "true"
        B = self.prog.tables[tname].layout.unique_hashes[cname]
        bb = max(1, B.bit_length() - 1)
        sbits = self.table_slot_bits[tname]
        h = self._bucket_smt(ctype, value_expr, B)
        hused = (self.pre_hused if state == "pre" else self.cur_hused)[(tname, cname)]
        hkey = (self.pre_hkey if state == "pre" else self.cur_hkey)[(tname, cname)]
        occ = (self.pre_occ if state == "pre" else self.cur_occ)[tname]
        # A lookup is pure in the index, occupancy and unique-column versions.
        # Reusing its result lets an existence guard and the subsequent read
        # share a proof, rather than forcing the solver to equate independent
        # probe chains containing nonlinear 64-bit hash arithmetic. Include column
        # writes as well as index arrays: the frame encoding abstracts insertion
        # without updating the concrete index arrays.
        version = (
            (self.pre_col[tname][cname],)
            if state == "pre"
            else (self.cur_widx[(tname, cname)], self.cur_wval[(tname, cname)])
        )
        cache_key = (tname, cname, value_expr, hused, hkey, occ, *version)
        if cache_key in self._hash_lookups:
            return self._hash_lookups[cache_key]
        result, found, active = self._bv(0, sbits), "false", "true"
        for i in range(MAX_PROBES):
            pos = f"(bvadd {h} {self._bv(i, bb)})"
            used = self._select_arr(hused, pos)
            slot = self._select_arr(hkey, pos)
            val = (
                self._apply(self.pre_col[tname][cname], slot)
                if state == "pre"
                else self._current_col_slot(tname, cname, slot, ctype)
            )
            home = self._bucket_smt(ctype, val, B)
            distance = f"(bvsub {pos} {home})"
            reachable = self._and(active, used, f"(bvuge {distance} {self._bv(i, bb)})")
            take = self._and(reachable, f"(= {val} {value_expr})")
            result = self._bind(
                f"{self._column_symbol(tname, cname)}_lookup_slot",
                self._ite(take, slot, result),
                f"(_ BitVec {sbits})",
                ("bv", sbits),
            )
            found = self._bind(
                f"{self._column_symbol(tname, cname)}_lookup_found",
                self._or(found, take),
                "Bool",
                ("bool",),
            )
            active = self._bind(
                f"{self._column_symbol(tname, cname)}_lookup_active",
                self._and(reachable, self._not(take)),
                "Bool",
                ("bool",),
            )
        for value, slot, guard in known:
            take = self._and(guard, f"(= {value} {value_expr})")
            result = self._ite(take, slot, result)
            found = self._or(found, take)
        # The reference lookup returns a handle only for an occupied row.
        # Keep this check in the returned predicate, so a guarded table_get
        # does not have to rediscover row liveness through every hash probe.
        occupied = self._select_arr(occ, result)
        found = self._bind(
            f"{self._column_symbol(tname, cname)}_lookup_live",
            self._and(found, occupied),
            "Bool",
            ("bool",),
        )
        self._hash_lookups[cache_key] = result, found
        return result, found

    def _hash_insert_smt(
        self, tname: str, cname: str, value_expr: str, slot_expr: str, ctype: ScalarType
    ) -> None:
        """Bounded Robin Hood insertion, including every displaced bucket."""
        self.known_unique.setdefault((tname, cname), []).append(
            (value_expr, slot_expr, self.active)
        )
        if not self.config.exact_sum:
            outcome = self._bind(
                f"{self._column_symbol(tname, cname)}_index_ok",
                "true",
                "Bool",
                ("bool",),
            )
            # Successful index insertion preserves logical lookup.  Resource
            # saturation and duplicate keys are handled reverts; allowing a
            # commit here only enlarges the safety/panic search space.
            self._pi_update(outcome)
            return
        B = self.prog.tables[tname].layout.unique_hashes[cname]
        bb = max(1, B.bit_length() - 1)
        sbits = self.table_slot_bits[tname]
        hused, hkey = self.cur_hused[(tname, cname)], self.cur_hkey[(tname, cname)]
        pos, incoming, distance = (
            self._bucket_smt(ctype, value_expr, B),
            slot_expr,
            self._bv(0, bb),
        )
        active, placed, duplicate = self.active, "false", "false"
        bsort, ssort = f"(_ BitVec {bb})", f"(_ BitVec {sbits})"
        for _ in range(MAX_PROBES):
            used = self._select_arr(hused, pos)
            resident = self._select_arr(hkey, pos)
            resident_value = self._current_col_slot(tname, cname, resident, ctype)
            incoming_value = self._current_col_slot(tname, cname, incoming, ctype)
            resident_home = self._bucket_smt(ctype, resident_value, B)
            resident_dist = self._bind(
                f"{self._column_symbol(tname, cname)}_resident_dist",
                f"(bvsub {pos} {resident_home})",
                bsort,
                ("bv", bb),
            )
            match = self._and(active, used, f"(= {resident_value} {incoming_value})")
            free = self._and(active, self._not(used))
            swap = self._and(
                active, used, self._not(match), f"(bvult {resident_dist} {distance})"
            )
            put = self._or(free, swap)
            duplicate = self._bind(
                f"{self._column_symbol(tname, cname)}_duplicate",
                self._or(duplicate, match),
                "Bool",
                ("bool",),
            )
            placed = self._bind(
                f"{self._column_symbol(tname, cname)}_placed",
                self._or(placed, free),
                "Bool",
                ("bool",),
            )
            next_key = self._ite(put, self._store_arr(hkey, pos, incoming), hkey)
            hkey = self._bind(
                f"{self._column_symbol(tname, cname)}_hkey",
                next_key,
                f"(Array {bsort} {ssort})",
                ("array", bb, sbits),
            )
            next_used = self._ite(free, self._store_arr(hused, pos, "true"), hused)
            hused = self._bind(
                f"{self._column_symbol(tname, cname)}_hused",
                next_used,
                f"(Array {bsort} Bool)",
                ("array", bb, "bool"),
            )
            incoming = self._bind(
                f"{self._column_symbol(tname, cname)}_incoming",
                self._ite(swap, resident, incoming),
                ssort,
                ("bv", sbits),
            )
            distance = self._bind(
                f"{self._column_symbol(tname, cname)}_distance",
                f"(bvadd {self._ite(swap, resident_dist, distance)} {self._bv(1, bb)})",
                bsort,
                ("bv", bb),
            )
            pos = self._bind(
                f"{self._column_symbol(tname, cname)}_position",
                f"(bvadd {pos} {self._bv(1, bb)})",
                bsort,
                ("bv", bb),
            )
            active = self._bind(
                f"{self._column_symbol(tname, cname)}_insert_active",
                self._and(active, used, self._not(match)),
                "Bool",
                ("bool",),
            )
        self._pi_update(self._and(self._not(duplicate), placed))
        self.cur_hused[(tname, cname)] = hused
        self.cur_hkey[(tname, cname)] = hkey

    # --- well-formedness ---------------------------------------------------
    def _build_well_formedness(self) -> None:
        for tname, tinfo in self.prog.tables.items():
            cap = tinfo.layout.capacity
            sbits = self.table_slot_bits[tname]
            if cap == 1:
                self.assert_common(
                    self._not(self._select_arr(self.pre_occ[tname], self._bv(1, sbits)))
                )
            for i in range(cap):
                slot = self._bv(i, sbits)
                occ = self._select_arr(self.pre_occ[tname], slot)
                for cname, cinfo in tinfo.columns.items():
                    val = self._apply(self.pre_col[tname][cname], slot)
                    self.assert_common(
                        f"(=> {self._not(occ)} (= {val} {self._zero(cinfo.type)}))"
                    )
                    if cinfo.type.kind == "enum":
                        self.assert_common(
                            f"(bvule {val} {self._bv(cinfo.type.max_value, cinfo.type.width)})"
                        )
            for cname in tinfo.layout.unique_hashes:
                B = tinfo.layout.unique_hashes[cname]
                bb = max(1, B.bit_length() - 1)
                hused = self.pre_hused[(tname, cname)]
                hkey = self.pre_hkey[(tname, cname)]
                free = [
                    self._not(self._select_arr(hused, self._bv(i, bb)))
                    for i in range(B)
                ]
                self.assert_common(self._or(*free))
                # Every occupied bucket must point at a live row.
                for i in range(B):
                    slot = self._select_arr(hkey, self._bv(i, bb))
                    used = self._select_arr(hused, self._bv(i, bb))
                    occ = self._select_arr(self.pre_occ[tname], slot)
                    self.assert_common(f"(=> {used} {occ})")
                if self.config.exact_sum:
                    # A realizable secondary index is a bijection with live
                    # rows and every row is reachable by bounded Robin Hood
                    # lookup.  These are representation invariants, not user
                    # assumptions that could discard valid executions.
                    ctype = tinfo.columns[cname].type
                    for i in range(cap):
                        row_slot = self._bv(i, sbits)
                        value = self._apply(self.pre_col[tname][cname], row_slot)
                        actual, found = self._hash_lookup_smt(
                            tname, cname, value, ctype, "pre"
                        )
                        live = self._select_arr(self.pre_occ[tname], row_slot)
                        self.assert_common(
                            f"(=> {live} (and {found} (= {actual} {row_slot})))"
                        )
                    count_bits = max(1, B.bit_length())
                    occupied = self._add_all(
                        [
                            self._ite(
                                self._select_arr(
                                    self.pre_occ[tname], self._bv(i, sbits)
                                ),
                                self._bv(1, count_bits),
                                self._bv(0, count_bits),
                            )
                            for i in range(cap)
                        ],
                        count_bits,
                    )
                    indexed = self._add_all(
                        [
                            self._ite(
                                self._select_arr(hused, self._bv(i, bb)),
                                self._bv(1, count_bits),
                                self._bv(0, count_bits),
                            )
                            for i in range(B)
                        ],
                        count_bits,
                    )
                    self.assert_common(f"(= {occupied} {indexed})")

    # --- pre-invariants ----------------------------------------------------
    def _build_pre_invariants(self) -> None:
        for inv in self.prog.invariants.values():
            if inv.quant_table is None:
                expr = self._spec_expr(inv.expr, {}, "pre")
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
                occ = self._select_arr(self.pre_occ[tname], slot)
                body = self._encode_forall_body(
                    inv.expr, tname, inv.quant_var, slot, "pre"
                )
                self.assert_common(f"(=> {occ} {body})")

    def _encode_forall_body(
        self, expr: A.Expr, table: str, var: str, slot: str, state: str
    ) -> str:
        env = {var: self._key_expr(table, slot, state)}
        return self._spec_expr(expr, env, state)

    def _key_expr(self, table: str, slot: str, state: str = "post") -> str:
        tinfo = self.prog.tables[table]
        gen = (
            self._apply(self.pre_gen[table], slot)
            if state == "pre"
            else self._current_gen(table, slot)
        )
        kb = tinfo.layout.key_bits
        raw = gen if kb == 0 else f"(concat {gen} {slot})"
        return self._extend(raw, tinfo.layout.generation_bits + kb, 64, False)

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
                add(
                    expr.access.table,
                    self._simple_slot(expr.access.key, env, expr.access.table),
                )
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
            if self.prog.tables[tname].layout.capacity == 1:
                self.assert_common(f"(= {w} {self._bv(0, sbits)})")
            self.witness_slots.setdefault(tname, w)
            add(tname, w)
        return out

    def _encode_key_simple(self, expr: A.Expr, env: Dict[str, str]) -> Optional[str]:
        if isinstance(expr, A.Var):
            if expr.name in env:
                ctype = self.type_env.get(expr.name) or getattr(
                    self.prog, "expr_types", {}
                ).get(id(expr))
                if ctype is not None and ctype.is_bool:
                    return None
                return self._as64(env[expr.name], ctype)
            if expr.name in self.params:
                return self._as64(
                    self.params[expr.name], self.tr.param_types.get(expr.name)
                )
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
            names = {
                "add": "bvadd",
                "sub": "bvsub",
                "mul": "bvmul",
                "div": "bvudiv",
                "rem": "bvurem",
            }
            if expr.op in names:
                return f"({names[expr.op]} {a} {b})"
            if expr.op == "and":
                return f"(bvand {a} {b})"
            if expr.op == "or":
                return f"(bvor {a} {b})"
            if expr.op == "xor":
                return f"(bvxor {a} {b})"
            if expr.op == "min":
                return self._ite(self._cmp("lt", a, b, False), a, b)
            if expr.op == "max":
                return self._ite(self._cmp("gt", a, b, False), a, b)
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

    def _simple_slot(
        self, expr: Optional[A.Expr], env: Dict[str, str], table: str
    ) -> Optional[str]:
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
        req_parts = []
        for _, expr in self.tr.decl.requires:
            condition = self._encode_expr(expr, self.params, "pre", BOOL)
            req_parts.append(condition)
            self._pi_update(condition)
        self.precondition = self._and(*req_parts)
        for stmt in self.tr.decl.body:
            self._encode_stmt(stmt, self.env)
        self.final_pi = self.pi
        self.final_trap = self.trap

    @contextmanager
    def _under(self, condition: str):
        previous = self.active
        self.active = self._and(previous, condition)
        try:
            yield
        finally:
            self.active = previous

    def _spec_expr(self, expr: A.Expr, env: Dict[str, str], state: str) -> str:
        """A specification predicate is true only when it is defined."""
        previous = self.silent, self.spec_checks, self.active
        self.silent, self.spec_checks, self.active = True, [], "true"
        try:
            value = self._encode_expr(expr, env, state, BOOL)
            return self._and(*self.spec_checks, value)
        finally:
            self.silent, self.spec_checks, self.active = previous

    def _pi_update(self, cond: str) -> None:
        if self.silent:
            return
        self._pi_counter += 1
        name = f"pi_{self._pi_counter}"
        self.declare(name, f"(declare-const {name} Bool)", ("bool",))
        cond = cond if self.active == "true" else f"(=> {self.active} {cond})"
        self.assert_common(f"(= {name} (and {self.pi} {cond}))")
        self.pi = name

    def _trap_update(self, trap_cond: str) -> None:
        guarded = self._and(self.active, trap_cond)
        if self.silent:
            if self.spec_checks is not None:
                self.spec_checks.append(self._not(guarded))
            return
        self._trap_counter += 1
        name = f"trap_{self._trap_counter}"
        self.declare(name, f"(declare-const {name} Bool)", ("bool",))
        self.assert_common(f"(= {name} (or {self.trap} (and {self.pi} {guarded})))")
        self.trap = name

    def _coerce_value(self, expr: A.Expr, value: str, target: ScalarType) -> str:
        if target.is_bool:
            return value
        width = self._encoded_width(expr)
        if width > target.width:
            fits = self._range_check(value, target, width)
            self._trap_update(self._not(fits))
            self._pi_update(fits)
            return self._truncate(value, width, target.width)
        if width < target.width:
            return self._extend(
                value, width, target.width, self._expr_type(expr).is_signed
            )
        return value

    def _encode_stmt(self, stmt: A.Stmt, env: Dict[str, str]) -> None:
        self._step_index += 1
        if isinstance(stmt, A.Let):
            if isinstance(stmt.value, A.Fold):
                val = self._encode_fold(stmt.value, env)
            else:
                val = self._encode_expr(
                    stmt.value, env, "post", self.tr.ssa_types.get(stmt.name, I64)
                )
            declared = self.tr.ssa_types.get(stmt.name, I64)
            val = self._coerce_value(stmt.value, val, declared)
            env[stmt.name] = val
            self.env[stmt.name] = val
            self.type_env[stmt.name] = declared
        elif isinstance(stmt, A.Guard):
            cond = self._encode_expr(stmt.cond, env, "post", BOOL)
            self._pi_update(cond)
        elif isinstance(stmt, A.TableSet):
            self._encode_table_set(stmt, env)
        elif isinstance(stmt, A.TableAlloc):
            self._encode_table_alloc(stmt, env)
            self.type_env[stmt.name] = id64(stmt.table)
        elif isinstance(stmt, A.Emit):
            from .semantic import _resolve_type_ref

            effect = self.prog.effects[stmt.effect]
            for arg, parameter in zip(stmt.args, effect.params):
                target = _resolve_type_ref(self.prog, parameter.type, "effect") or I64
                value = self._encode_expr(arg, env, "post", target)
                self._coerce_value(arg, value, target)
        elif isinstance(stmt, A.Yield):
            self._encode_expr(stmt.value, env, "post")
        elif isinstance(stmt, A.Commit):
            pass

    def _encode_table_set(self, stmt: A.TableSet, env: Dict[str, str]) -> None:
        tname = stmt.table
        cinfo = self.prog.tables[tname].columns[stmt.column]
        ctype = cinfo.type
        if (
            isinstance(stmt.key, A.Var)
            and stmt.key.name in self.alloc_slots
            and env.get(stmt.key.name) == self.env.get(stmt.key.name)
        ):
            slot = self.alloc_slots[stmt.key.name]
            valid = "true"
        else:
            key = self._encode_expr(stmt.key, env, "post")
            slot = self._slot_of_key(key, tname)
            valid = self._current_valid(key, tname)
        val = self._encode_expr(stmt.value, env, "post", ctype)
        val = self._coerce_value(stmt.value, val, ctype)
        self._trap_update(self._not(valid))
        self._pi_update(valid)
        v_old = self._current_col_slot(tname, stmt.column, slot, ctype)
        self.writes.setdefault((tname, stmt.column), []).append((slot, val))
        self._record_write(tname, stmt.column, slot, val)
        self.modified_slots[tname].append(slot)
        self._record_final(tname, stmt.column, slot, val, None, False)
        if cinfo.tracked_sum:
            self._agg_update(tname, stmt.column, v_old, val, ctype)

    def _record_final(
        self,
        tname: str,
        cname: str,
        slot: str,
        value: str,
        active: Optional[str],
        is_alloc: bool,
    ) -> None:
        key = (tname, cname)
        fv = self.final_values.setdefault(key, {})
        pv = self.slot_pre_value.setdefault(key, {})
        if slot not in pv:
            ctype = self.prog.tables[tname].columns[cname].type
            pv[slot] = (
                self._zero(ctype)
                if is_alloc
                else self._apply(self.pre_col[tname][cname], slot)
            )
        prev = fv.get(slot, pv[slot])
        fv[slot] = value if active is None else self._ite(active, value, prev)

    def _agg_update(
        self, tname: str, cname: str, v_old: str, v_new: str, ctype: ScalarType
    ) -> None:
        wide_w = self.agg_wide[(tname, cname)]
        signed = self.agg_signed[(tname, cname)]
        cur = self.cur_agg[tname][cname]
        w_old = self._extend(v_old, ctype.width, wide_w, signed)
        w_new = self._extend(v_new, ctype.width, wide_w, signed)
        new_val = f"(bvadd (bvsub {cur} {w_old}) {w_new})"
        in_range = self._agg_domain_check(
            new_val, wide_w, self.prog.tables[tname].layout.capacity, tname, cname
        )
        self._trap_update(self._not(in_range))
        self._pi_update(in_range)
        new_agg = self._fresh(
            f"{self._column_symbol(tname, cname)}_agg",
            f"(declare-const {self._column_symbol(tname, cname)}_agg_{self._version[f'{self._column_symbol(tname, cname)}_agg']} (_ BitVec {wide_w}))",
            ("bv", wide_w),
        )
        self.assert_common(f"(= {new_agg} {new_val})")
        self.cur_agg[tname][cname] = new_agg

    def _range_check(self, expr: str, ctype: ScalarType, wide_w: int) -> str:
        mn = ctype.min_value & ((1 << wide_w) - 1)
        mx = ctype.max_value & ((1 << wide_w) - 1)
        if ctype.is_signed:
            return self._and(
                f"(bvsge {expr} {self._bv(mn, wide_w)})",
                f"(bvsle {expr} {self._bv(mx, wide_w)})",
            )
        return self._and(
            f"(bvuge {expr} {self._bv(0, wide_w)})",
            f"(bvule {expr} {self._bv(mx, wide_w)})",
        )

    def _encode_table_alloc(
        self, stmt: A.TableAlloc, env: Dict[str, str], active: Optional[str] = None
    ) -> None:
        tname = stmt.table
        tinfo = self.prog.tables[tname]
        cap = tinfo.layout.capacity
        sbits = self.table_slot_bits[tname]
        gen_bits = min(64 - tinfo.layout.key_bits, 32)
        initializers = {}
        for cname, fexpr in stmt.fields:
            cinfo = tinfo.columns[cname]
            initializers[cname] = self._coerce_value(
                fexpr, self._encode_expr(fexpr, env, "post", cinfo.type), cinfo.type
            )
        for cname, cinfo in tinfo.columns.items():
            initializers.setdefault(cname, self._zero(cinfo.type))
        slot = self._symbolic_alloc_slot(tname)
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
            raw_id = new_gen
        else:
            raw_id = f"(concat {new_gen} {slot})"
        id_width = tinfo.layout.generation_bits + tinfo.layout.key_bits
        new_id = self._extend(raw_id, id_width, 64, False)
        if active is not None:
            new_id = self._ite(active, new_id, self._bv(0, 64))
        env[stmt.name] = new_id
        self.env[stmt.name] = new_id
        self.alloc_slots[stmt.name] = slot
        stored_gen = new_gen if active is None else self._ite(active, new_gen, gen)
        self.alloc_records.setdefault(tname, []).append((slot, stored_gen))
        self.modified_slots[tname].append(slot)
        # occupancy array: mark the allocated slot live
        base = f"{self._table_symbol(tname)}_occ"
        new_occ_arr = self._fresh(
            base,
            f"(declare-const {base}_{self._version.get(base, 0)} (Array (_ BitVec {sbits}) Bool))",
            ("array", sbits, "bool"),
        )
        stored = self._store_arr(self.cur_occ[tname], slot, "true")
        if active is not None:
            stored = self._ite(active, stored, self.cur_occ[tname])
        self.assert_common(f"(= {new_occ_arr} {stored})")
        self.cur_occ[tname] = new_occ_arr
        for cname, cinfo in tinfo.columns.items():
            init = initializers[cname]
            stored_init = (
                init
                if active is None
                else self._ite(
                    active, init, self._current_col_slot(tname, cname, slot, cinfo.type)
                )
            )
            self.writes.setdefault((tname, cname), []).append((slot, stored_init))
            self._record_write(tname, cname, slot, stored_init)
            self._record_final(tname, cname, slot, init, active, True)
            if cinfo.tracked_sum:
                self._agg_add(tname, cname, init, cinfo.type, active)
            if cinfo.unique:
                self._hash_insert_smt(tname, cname, init, slot, cinfo.type)

    def _agg_add(
        self,
        tname: str,
        cname: str,
        v_new: str,
        ctype: ScalarType,
        active: Optional[str] = None,
    ) -> None:
        wide_w = self.agg_wide[(tname, cname)]
        signed = self.agg_signed[(tname, cname)]
        cur = self.cur_agg[tname][cname]
        w_new = self._extend(v_new, ctype.width, wide_w, signed)
        delta = (
            w_new if active is None else self._ite(active, w_new, self._bv(0, wide_w))
        )
        new_val = f"(bvadd {cur} {delta})"
        in_range = self._agg_domain_check(
            new_val, wide_w, self.prog.tables[tname].layout.capacity, tname, cname
        )
        if active is None:
            self._trap_update(self._not(in_range))
            self._pi_update(in_range)
        else:
            self._trap_update(self._and(active, self._not(in_range)))
            self._pi_update(f"(=> {active} {in_range})")
        new_agg = self._fresh(
            f"{self._column_symbol(tname, cname)}_agg",
            f"(declare-const {self._column_symbol(tname, cname)}_agg_{self._version[f'{self._column_symbol(tname, cname)}_agg']} (_ BitVec {wide_w}))",
            ("bv", wide_w),
        )
        self.assert_common(f"(= {new_agg} {new_val})")
        self.cur_agg[tname][cname] = new_agg

    def _lowest_free_ite(self, table: str, cap: int, sbits: int) -> str:
        expr = self._bv(cap - 1, sbits)
        for i in range(cap - 1, -1, -1):
            slot = self._bv(i, sbits)
            free = self._not(self._current_occ(table, slot))
            expr = self._ite(free, slot, expr)
        return expr

    def _symbolic_alloc_slot(self, table: str) -> str:
        """Fresh slot variable constrained to be the lowest free slot.

        The ite-chain over C slots is O(C) nested ites whose equality tests
        against earlier symbolic slots grow quadratically; a single symbolic
        slot plus an O(C) lowest-free constraint keeps the slot expression O(1).
        """
        tinfo = self.prog.tables[table]
        cap = tinfo.layout.capacity
        sbits = self.table_slot_bits[table]
        base = f"{self._table_symbol(table)}_alloc_slot"
        name = self._fresh(
            base,
            f"(declare-const {base}_{self._version.get(base, 0)} (_ BitVec {sbits}))",
            ("bv", sbits),
        )
        occ = self.cur_occ[table]
        free = self._or(
            *(self._not(self._select_arr(occ, self._bv(j, sbits))) for j in range(cap))
        )
        if cap == 1:
            self.assert_common(f"(= {name} {self._bv(0, sbits)})")
        self.assert_common(f"(=> {free} {self._not(self._select_arr(occ, name))})")
        for j in range(cap):
            lit = self._bv(j, sbits)
            self.assert_common(
                f"(=> (and {free} (bvult {lit} {name})) {self._select_arr(occ, lit)})"
            )
        return name

    def _symbolic_free_slot(self, table: str, cap: int, sbits: int) -> str:
        name = self._fresh(
            f"{self._table_symbol(table)}_alloc_slot",
            f"(declare-const {self._table_symbol(table)}_alloc_slot_{self._version[f'{self._table_symbol(table)}_alloc_slot']} (_ BitVec {sbits}))",
            ("bv", sbits),
        )
        self.assert_common(self._not(self._current_occ(table, name)))
        return name

    # --- folds -------------------------------------------------------------
    def _encode_fold(
        self, fold: A.Fold, env: Dict[str, str], state: str = "post"
    ) -> str:
        if fold.n > self.config.max_unroll:
            self.diags.error(
                "D_FOLD_BOUND",
                f"fold_bounded<{fold.n}> exceeds the verifier unroll "
                f"limit {self.config.max_unroll}",
                fold.line,
                fold.col,
            )
            return self._zero(self._resolve_fold_type(fold))
        idx_type = self._fold_index_type(fold)
        acc_type = self._resolve_fold_type(fold)
        acc = self._encode_expr(fold.init, env, state, acc_type)
        acc = self._coerce_value(fold.init, acc, acc_type)
        lo = self._coerce_value(
            fold.lo, self._encode_expr(fold.lo, env, state), idx_type
        )
        hi = self._coerce_value(
            fold.hi, self._encode_expr(fold.hi, env, state), idx_type
        )
        wide = idx_type.width + 1
        wide_lo = self._extend(lo, idx_type.width, wide, idx_type.is_signed)
        wide_hi = self._extend(hi, idx_type.width, wide, idx_type.is_signed)
        for j in range(fold.n):
            # The induction index is mathematical.  An iteration after MAX is
            # inactive rather than wrapping back to MIN (or zero).
            wide_idx = f"(bvadd {wide_lo} {self._bv(j, wide)})"
            active = self._and(
                self._cmp("le", wide_idx, wide_hi, idx_type.is_signed),
                self._cmp(
                    "le",
                    wide_idx,
                    self._bv(idx_type.max_value, wide),
                    idx_type.is_signed,
                ),
            )
            idx_val = self._truncate(wide_idx, wide, idx_type.width)
            inner_env = dict(env)
            inner_env[fold.index_name] = idx_val
            inner_env[fold.acc_name] = acc
            saved_types = dict(self.type_env)
            self.type_env[fold.index_name] = idx_type
            self.type_env[fold.acc_name] = acc_type
            try:
                with self._under(active):
                    for stmt in fold.body:
                        self._encode_fold_stmt(stmt, inner_env, self.active, state)
                    yield_val = self._encode_expr(
                        fold.yield_expr, inner_env, state, acc_type
                    )
                    yield_val = self._coerce_value(fold.yield_expr, yield_val, acc_type)
            finally:
                self.type_env = saved_types
            acc = self._ite(active, yield_val, acc)
        return acc

    def _encode_fold_stmt(
        self, stmt: A.Stmt, env: Dict[str, str], active: str, state: str = "post"
    ) -> None:
        if isinstance(stmt, A.Let):
            from .semantic import _resolve_type_ref

            ft = _resolve_type_ref(self.prog, stmt.type, "fold let") or I64
            val = self._encode_expr(stmt.value, env, state, ft)
            val = self._coerce_value(stmt.value, val, ft)
            env[stmt.name] = val
            self.type_env[stmt.name] = ft
        elif isinstance(stmt, A.Guard):
            cond = self._encode_expr(stmt.cond, env, state, BOOL)
            self._pi_update(cond)
        elif isinstance(stmt, A.TableSet):
            tname = stmt.table
            cinfo = self.prog.tables[tname].columns[stmt.column]
            ctype = cinfo.type
            if (
                isinstance(stmt.key, A.Var)
                and stmt.key.name in self.alloc_slots
                and env.get(stmt.key.name) == self.env.get(stmt.key.name)
            ):
                slot = self.alloc_slots[stmt.key.name]
                valid = "true"
            else:
                key = self._encode_expr(stmt.key, env, state)
                slot = self._slot_of_key(key, tname)
                valid = self._current_valid(key, tname)
            val = self._encode_expr(stmt.value, env, state, ctype)
            val = self._coerce_value(stmt.value, val, ctype)
            self._trap_update(self._not(valid))
            self._pi_update(valid)
            v_old = self._current_col_slot(tname, stmt.column, slot, ctype)
            fold_val = self._ite(active, val, v_old)
            self.writes.setdefault((tname, stmt.column), []).append((slot, fold_val))
            self._record_write(tname, stmt.column, slot, fold_val)
            self.modified_slots[tname].append(slot)
            self._record_final(tname, stmt.column, slot, val, active, False)
            if cinfo.tracked_sum:
                wide_w = self.agg_wide[(tname, stmt.column)]
                signed = self.agg_signed[(tname, stmt.column)]
                cur = self.cur_agg[tname][stmt.column]
                w_old = self._extend(v_old, ctype.width, wide_w, signed)
                w_new = self._extend(val, ctype.width, wide_w, signed)
                delta = self._ite(
                    active, f"(bvsub {w_new} {w_old})", self._bv(0, wide_w)
                )
                new_val = f"(bvadd {cur} {delta})"
                in_range = self._agg_domain_check(
                    new_val,
                    wide_w,
                    self.prog.tables[tname].layout.capacity,
                    tname,
                    stmt.column,
                )
                self._trap_update(self._not(in_range))
                self._pi_update(in_range)
                new_agg = self._fresh(
                    f"{self._column_symbol(tname, stmt.column)}_agg",
                    f"(declare-const {self._column_symbol(tname, stmt.column)}_agg_{self._version[f'{self._column_symbol(tname, stmt.column)}_agg']} (_ BitVec {wide_w}))",
                    ("bv", wide_w),
                )
                self.assert_common(f"(= {new_agg} {new_val})")
                self.cur_agg[tname][stmt.column] = new_agg
        elif isinstance(stmt, A.TableAlloc):
            self._encode_table_alloc(stmt, env, active)
            self.type_env[stmt.name] = id64(stmt.table)
        elif isinstance(stmt, A.Emit):
            from .semantic import _resolve_type_ref

            effect = self.prog.effects[stmt.effect]
            for arg, parameter in zip(stmt.args, effect.params):
                target = _resolve_type_ref(self.prog, parameter.type, "effect") or I64
                value = self._encode_expr(arg, env, state, target)
                self._coerce_value(arg, value, target)
        elif isinstance(stmt, A.Yield):
            self._encode_expr(stmt.value, env, state)
        elif not isinstance(stmt, A.Commit):
            self.diags.error(
                "D_TYPE_MISMATCH", f"cannot encode statement {type(stmt).__name__}"
            )

    def _resolve_fold_type(self, fold: A.Fold) -> ScalarType:
        from .semantic import _resolve_type_ref

        t = _resolve_type_ref(self.prog, fold.acc_type, "fold")
        return t or I64

    def _fold_index_type(self, fold: A.Fold) -> ScalarType:
        from .semantic import _TypeChecker

        tc = _TypeChecker(self.prog, dict(self.type_env))
        return tc.type_of(fold.lo)

    # --- expressions -------------------------------------------------------
    def _encode_expr(
        self,
        expr: A.Expr,
        env: Dict[str, str],
        state: str,
        expected: Optional[ScalarType] = None,
    ) -> str:
        if isinstance(expr, A.Literal):
            return self._encode_literal(expr, expected)
        if isinstance(expr, A.Var):
            if expr.name in env:
                return env[expr.name]
            if expr.name in self.params:
                return self.params[expr.name]
            if (
                expected is not None
                and expected.kind == "enum"
                and expr.name in expected.enum_tags
            ):
                return self._bv(expected.enum_tags.index(expr.name), expected.width)
            self.diags.error(
                "D_UNKNOWN_PARAM",
                f"unknown variable {expr.name!r}",
                expr.line,
                expr.col,
            )
            return self._bv(0, expected.width if expected is not None else 64)
        if isinstance(expr, A.TableGet):
            cinfo = self.prog.tables[expr.table].columns[expr.column]
            ctype = cinfo.type
            if (
                state == "post"
                and isinstance(expr.key, A.Var)
                and expr.key.name in self.alloc_slots
                and env.get(expr.key.name) == self.env.get(expr.key.name)
            ):
                # Known-slot fast path: the allocation guarantees liveness.
                slot = self.alloc_slots[expr.key.name]
                return self._current_col_slot(expr.table, expr.column, slot, ctype)
            key = self._encode_expr(expr.key, env, state)
            if state == "pre":
                slot = self._slot_of_key(key, expr.table)
                valid = self._pre_valid(key, expr.table)
                self._trap_update(self._not(valid))
                self._pi_update(valid)
                return self._apply(self.pre_col[expr.table][expr.column], slot)
            valid = self._current_valid(key, expr.table)
            val = self._current_col(expr.table, expr.column, key, ctype)
            self._trap_update(self._not(valid))
            self._pi_update(valid)
            return self._ite(valid, val, self._zero(ctype))
        if isinstance(expr, A.TableExists):
            if (
                state == "post"
                and isinstance(expr.key, A.Var)
                and expr.key.name in self.alloc_slots
                and env.get(expr.key.name) == self.env.get(expr.key.name)
            ):
                return "true"
            key = self._encode_expr(expr.key, env, state)
            if state == "pre":
                return self._pre_valid(key, expr.table)
            return self._current_valid(key, expr.table)
        if isinstance(expr, A.TableExistsUnique):
            tinfo = self.prog.tables[expr.table]
            cinfo = tinfo.columns[expr.key_column]
            key = self._encode_expr(expr.key, env, state)
            slot, found = self._hash_lookup_smt(
                expr.table, expr.key_column, key, cinfo.type, state
            )
            self.lookup_results[id(expr)] = (slot, found)
            return found
        if isinstance(expr, A.TableLookup):
            tinfo = self.prog.tables[expr.table]
            cinfo = tinfo.columns[expr.key_column]
            key = self._encode_expr(expr.key, env, state)
            slot, found = self._hash_lookup_smt(
                expr.table, expr.key_column, key, cinfo.type, state
            )
            self.lookup_results[id(expr)] = (slot, found)
            self._trap_update(self._not(found))
            self._pi_update(found)
            gen = (
                self._apply(self.pre_gen[expr.table], slot)
                if state == "pre"
                else self._current_gen(expr.table, slot)
            )
            width = tinfo.layout.generation_bits + tinfo.layout.key_bits
            raw = gen if tinfo.layout.key_bits == 0 else f"(concat {gen} {slot})"
            return self._extend(raw, width, 64, False)
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
            value = self._encode_expr(expr.expr, env, state)
            ctype = self._expr_type(expr.expr)
            width = self._encoded_width(expr.expr)
            return (
                self._truncate(value, width, 64)
                if width > 64
                else self._extend(value, width, 64, ctype.is_signed)
            )
        if isinstance(expr, A.Select):
            cond = self._encode_expr(expr.cond, env, state, BOOL)
            with self._under(cond):
                a = self._encode_expr(expr.a, env, state, expected)
            with self._under(self._not(cond)):
                b = self._encode_expr(expr.b, env, state, expected)
            aw = self._sumagg_width(expr)
            if aw is not None:
                a = self._widen_operand(expr.a, a, aw)
                b = self._widen_operand(expr.b, b, aw)
            return self._ite(cond, a, b)
        if isinstance(expr, A.Unary):
            return self._encode_unary(expr, env, state, expected)
        if isinstance(expr, A.Binary):
            return self._encode_binary(expr, env, state, expected)
        if isinstance(expr, A.Fold):
            return self._encode_fold(expr, env, state)
        self.diags.error(
            "D_TYPE_MISMATCH",
            f"cannot encode expression {type(expr).__name__}",
            getattr(expr, "line", 0),
            getattr(expr, "col", 0),
        )
        return self._bv(0, expected.width if expected is not None else 64)

    def _encode_literal(self, expr: A.Literal, expected: Optional[ScalarType]) -> str:
        from .types import literal_value

        ctype = expected or self._expr_type(expr)
        val = self.prog.literal_values.get(id(expr))
        if val is None:
            val = literal_value(expr.value, ctype)
        if ctype.is_bool:
            return "true" if val else "false"
        if isinstance(val, bytes):
            val = int.from_bytes(val, "little")
        return self._bv(val, ctype.width)

    def _expr_type(self, expr: A.Expr) -> ScalarType:
        known = getattr(self.prog, "expr_types", {}).get(id(expr))
        if known is not None:
            return known
        from .semantic import _TypeChecker

        return _TypeChecker(self.prog, dict(self.type_env)).type_of(expr)

    def _encode_unary(
        self,
        expr: A.Unary,
        env: Dict[str, str],
        state: str,
        expected: Optional[ScalarType],
    ) -> str:
        if expr.op == "not":
            operand = self._encode_expr(expr.operand, env, state, BOOL)
            return self._not(operand)
        if expr.op == "neg":
            ctype = expected or self._infer_type(expr.operand, env)
            operand = self._encode_expr(expr.operand, env, state, ctype)
            if self._sumagg_width(expr.operand) is not None:
                ctype = self._arithmetic_type(
                    expr.operand, self._sumagg_width(expr.operand)
                )
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
            self.diags.error(
                "D_BAD_OPERAND",
                "neg() on an unsigned type always overflows",
                expr.line,
                expr.col,
            )
            return self._bv(0, ctype.width)
        self.diags.error(
            "D_BAD_OPERAND", f"unknown unary operator {expr.op!r}", expr.line, expr.col
        )
        return self._bv(0, 8)

    def _sumagg_width(self, expr: A.Expr):
        """Physical accumulator width propagated through value expressions."""
        if isinstance(expr, A.SumAgg):
            return self.agg_wide.get((expr.table, expr.column))
        if isinstance(expr, A.Binary):
            if expr.op in ("and", "or", "xor", "eq", "ne", "lt", "le", "gt", "ge"):
                return None
            widths = [self._sumagg_width(expr.left), self._sumagg_width(expr.right)]
        elif isinstance(expr, A.Unary):
            widths = [self._sumagg_width(expr.operand)]
        elif isinstance(expr, A.Select):
            widths = [self._sumagg_width(expr.a), self._sumagg_width(expr.b)]
        else:
            return None
        widths = [w for w in widths if w is not None]
        return max(widths) if widths else None

    def _encoded_width(self, expr: A.Expr) -> int:
        return self._sumagg_width(expr) or self._expr_type(expr).width

    def _widen_operand(self, expr: A.Expr, value: str, width: int) -> str:
        ctype = self._expr_type(expr)
        return self._extend(value, self._encoded_width(expr), width, ctype.is_signed)

    def _arithmetic_type(self, expr: A.Expr, width: int) -> ScalarType:
        source = self._expr_type(expr)
        kind = (
            source.kind
            if source.is_decimal
            else (("i" if source.is_signed else "u") + str(width))
        )
        return ScalarType(kind, width, width // 8, scale=source.scale)

    def _encode_binary(
        self,
        expr: A.Binary,
        env: Dict[str, str],
        state: str,
        expected: Optional[ScalarType],
    ) -> str:
        op = expr.op
        if op in ("and", "or", "xor"):
            # Canonical boolean operators are eager.  Only select is lazy.
            a = self._encode_expr(expr.left, env, state, BOOL)
            b = self._encode_expr(expr.right, env, state, BOOL)
            if op == "and":
                return self._and(a, b)
            if op == "or":
                return self._or(a, b)
            return f"(xor {a} {b})"
        if op in ("add", "sub", "mul", "div", "rem", "min", "max"):
            aw = self._sumagg_width(expr)
            if aw is not None:
                source = self._expr_type(expr)
                a = self._encode_expr(expr.left, env, state, source)
                b = self._encode_expr(expr.right, env, state, source)
                a = self._widen_operand(expr.left, a, aw)
                b = self._widen_operand(expr.right, b, aw)
                return self._encode_arith(
                    op, a, b, self._arithmetic_type(expr, aw), expr
                )
            if expected is not None and not expected.is_numeric:
                expected = None
            ctype = self._infer_arith_type(expr, env, expected)
            a = self._encode_expr(expr.left, env, state, ctype)
            b = self._encode_expr(expr.right, env, state, ctype)
            return self._encode_arith(op, a, b, ctype, expr)
        if op in ("eq", "ne", "lt", "le", "gt", "ge"):
            ctype = self._infer_cmp_type(expr, env, None)
            widths = [self._sumagg_width(expr.left), self._sumagg_width(expr.right)]
            aw = max((w for w in widths if w is not None), default=None)
            a = self._encode_expr(expr.left, env, state, ctype)
            b = self._encode_expr(expr.right, env, state, ctype)
            if aw is not None:
                a = self._widen_operand(expr.left, a, aw)
                b = self._widen_operand(expr.right, b, aw)
            if op in ("eq", "ne"):
                cmp = f"(= {a} {b})"
                return cmp if op == "eq" else self._not(cmp)
            return self._cmp(op, a, b, ctype.is_signed)
        self.diags.error(
            "D_BAD_OPERAND", f"unknown binary operator {op!r}", expr.line, expr.col
        )
        return self._bv(0, 8)

    def _encode_arith(
        self, op: str, a: str, b: str, ctype: ScalarType, expr: A.Expr
    ) -> str:
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
                prod = (
                    f"(bvmul {self._extend(a, w, 2 * w, True)} "
                    f"{self._extend(b, w, 2 * w, True)})"
                )
                low = self._truncate(prod, 2 * w, w)
                ovf = self._not(f"(= {prod} {self._extend(low, w, 2 * w, True)})")
                self._trap_update(ovf)
                self._pi_update(self._not(ovf))
                return low
            prod = (
                f"(bvmul {self._extend(a, w, 2 * w, False)} "
                f"{self._extend(b, w, 2 * w, False)})"
            )
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
                self.diags.error(
                    "D_BAD_OPERAND",
                    "rem() is not defined for fixed-point decimals",
                    expr.line,
                    expr.col,
                )
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
        scale = 10**ctype.scale
        prod = (
            f"(bvmul {self._extend(a, w, 2 * w, True)} "
            f"{self._extend(b, w, 2 * w, True)})"
        )
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
        scale = 10**ctype.scale
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
        return self._expr_type(expr)

    def _infer_arith_type(
        self, expr: A.Binary, env: Dict[str, str], expected: Optional[ScalarType]
    ) -> ScalarType:
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

    def _infer_cmp_type(
        self, expr: A.Binary, env: Dict[str, str], expected: Optional[ScalarType]
    ) -> ScalarType:
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
            parts.append(self._spec_expr(expr, self.env, "post"))
        for inv in self.prog.invariants.values():
            tname = inv.quant_table
            if tname is None:
                parts.append(self._spec_expr(inv.expr, {}, "post"))
                continue
            if tname not in self.touched_tables:
                continue
            for slot in self.modified_slots.get(tname, []):
                occ = self._current_occ(tname, slot)
                body = self._encode_forall_body(
                    inv.expr, tname, inv.quant_var, slot, "post"
                )
                parts.append(f"(=> {occ} {body})")
            w = self.witness_slots.get(tname)
            if w is not None:
                occ = self._current_occ(tname, w)
                body = self._encode_forall_body(
                    inv.expr, tname, inv.quant_var, w, "post"
                )
                parts.append(f"(=> {occ} {body})")
        # aggregate consistency
        for (tname, cname), wide_w in self.agg_wide.items():
            if not self.modified_slots.get(tname):
                continue
            if self.frame_ok.get((tname, cname)):
                # Telescoping frame identity: sigma_final = sigma_rest + sigma_local(final)
                rest = self.frame_rest[(tname, cname)]
                local_post = self._frame_local_post(tname, cname)
                parts.append(
                    f"(= {self.cur_agg[tname][cname]} (bvadd {rest} {local_post}))"
                )
                continue
            if self.config.exact_sum:
                ctype = self.prog.tables[tname].columns[cname].type
                capacity = self.prog.tables[tname].layout.capacity
                sbits = self.table_slot_bits[tname]
                terms = []
                for i in range(capacity):
                    slot = self._bv(i, sbits)
                    value = self._current_col_slot(tname, cname, slot, ctype)
                    terms.append(
                        self._ite(
                            self._current_occ(tname, slot),
                            self._extend(value, ctype.width, wide_w, ctype.is_signed),
                            self._bv(0, wide_w),
                        )
                    )
                parts.append(
                    f"(= {self.cur_agg[tname][cname]} {self._add_all(terms, wide_w)})"
                )
        self.post_condition = self._and(*parts) if parts else "true"
        self.silent = False

    def _build_obligations(self) -> None:
        pre_parts = [a[len("(assert ") : -1] for a in self.common_asserts]
        common_body = self._and(*pre_parts)
        self.declare("common_0", "(declare-const common_0 Bool)", ("bool",))
        self.assert_common(f"(= common_0 {common_body})")
        common = "common_0"
        self.obligation_asserts["safety"] = (
            f"(and {common} {self.final_pi} (not {self.post_condition}))"
        )
        self.obligation_asserts["panic"] = f"(and {common} {self.final_trap})"
        self.obligation_asserts["liveness"] = f"(and {common} {self.final_pi})"

    # --- script ------------------------------------------------------------
    def script_for(self, obligation: Optional[str] = None) -> str:
        lines = ["(set-logic QF_AUFBV)"]
        lines.extend(self.decls)
        lines.extend(self.common_asserts)
        if obligation is None:
            for name in ("safety", "panic", "liveness"):
                lines.append(f"(assert {self.obligation_asserts[name]})")
        else:
            lines.append(f"(assert {self.obligation_asserts[obligation]})")
        return "\n".join(lines) + "\n"

    def script_full(self) -> str:
        lines = ["(set-logic QF_AUFBV)"]
        lines.extend(self.decls)
        lines.extend(self.common_asserts)
        for name, label in (
            ("safety", "Obligation 1: safety"),
            ("panic", "Obligation 2: panic-free"),
            ("liveness", "Obligation 3: liveness"),
        ):
            lines.append(f"; ---- {label} ----")
            lines.append("(push)")
            lines.append(f"(assert {self.obligation_asserts[name]})")
            lines.append("(check-sat)")
            lines.append("(pop)")
        return "\n".join(lines) + "\n"

    # --- solving -----------------------------------------------------------
    def solve(self) -> VerificationResult:
        if not self.config.exact_sum:
            return verify_transition_dual(self.prog, self.tr_name, self.config)
        result = VerificationResult(
            transition=self.tr_name,
            safety=ObligationResult("safety"),
            panic=ObligationResult("panic"),
            liveness=ObligationResult("liveness"),
            script=self.script_full(),
        )
        for name in ("safety", "panic", "liveness"):
            setattr(result, name, self._solve_one(name))
        result.diagnostics = list(self.prog.diags.errors) + list(self.diags.items)
        return result

    def _candidate_constraints(
        self, candidate: Optional[z3.ModelRef], sparse: bool = False
    ):
        """Fix a proposed finite state; failed proposals never establish proofs."""
        from .types import bucket_of

        fixed, keep, values = [], {t: set() for t in self.prog.tables}, {}
        seeded_values = {}
        allocating_tables = set()
        from dataclasses import fields, is_dataclass

        def collect_allocations(node):
            if isinstance(node, A.TableAlloc):
                allocating_tables.add(node.table)
            if is_dataclass(node):
                for member in fields(node):
                    child = getattr(node, member.name)
                    if isinstance(child, list):
                        for item in child:
                            if isinstance(item, tuple):
                                for part in item:
                                    collect_allocations(part)
                            else:
                                collect_allocations(item)
                    elif is_dataclass(child):
                        collect_allocations(child)

        collect_allocations(self.tr.decl)

        def read(term):
            if candidate is not None:
                return candidate.eval(term, model_completion=True)
            return (
                z3.BoolVal(False)
                if term.sort().kind() == z3.Z3_BOOL_SORT
                else z3.BitVecVal(0, term.size())
            )

        for name, ctype in self.tr.param_types.items():
            const = (
                z3.Bool(self.params[name])
                if ctype.is_bool
                else z3.BitVec(self.params[name], ctype.width)
            )
            value = read(const)
            fixed.append(const == value)
            values[name] = value
            if ctype.kind == "id64":
                keep[ctype.key_table].add(
                    value.as_long()
                    & (self.prog.tables[ctype.key_table].layout.capacity - 1)
                )
        # Secondary-key parameters also identify rows needed by a sparse
        # witness, even when the transition has no primary-key parameter.
        from dataclasses import fields, is_dataclass

        def visit(node):
            if isinstance(node, (A.TableLookup, A.TableExistsUnique)):
                source = getattr(self, "witness_source", self)
                previous_lookup = source.lookup_results.get(id(node))
                if previous_lookup is not None and candidate is not None:
                    slot_name, found_name = previous_lookup
                    slot_kind = source.const_kinds.get(slot_name)
                    found_kind = source.const_kinds.get(found_name)
                    if slot_kind and found_kind and found_kind[0] == "bool":
                        was_found = candidate.eval(
                            z3.Bool(found_name), model_completion=True
                        )
                        if z3.is_true(was_found):
                            slot_value = candidate.eval(
                                z3.BitVec(slot_name, slot_kind[1]),
                                model_completion=True,
                            ).as_long()
                            keep[node.table].add(slot_value)
                            return
                key = node.key
                desired = values.get(key.name) if isinstance(key, A.Var) else None
                if desired is not None:
                    tinfo = self.prog.tables[node.table]
                    sbits = self.table_slot_bits[node.table]
                    slot_sort = z3.BitVecSort(sbits)
                    occ = z3.Array(self.pre_occ[node.table], slot_sort, z3.BoolSort())
                    ctype = tinfo.columns[node.key_column].type
                    sort = (
                        z3.BoolSort() if ctype.is_bool else z3.BitVecSort(ctype.width)
                    )
                    fn = z3.Function(
                        self.pre_col[node.table][node.key_column], slot_sort, sort
                    )
                    for i in range(tinfo.layout.capacity):
                        idx = z3.BitVecVal(i, sbits)
                        if z3.is_true(read(occ[idx])) and z3.is_true(
                            z3.simplify(read(fn(idx)) == desired)
                        ):
                            keep[node.table].add(i)
                            break
                    else:
                        if candidate is None and node.table not in allocating_tables:
                            keep[node.table].add(0)
                            seeded_values[(node.table, 0, node.key_column)] = desired
            if is_dataclass(node):
                for member in fields(node):
                    child = getattr(node, member.name)
                    if isinstance(child, list):
                        for item in child:
                            if isinstance(item, tuple):
                                for part in item:
                                    visit(part)
                            else:
                                visit(item)
                    elif is_dataclass(child):
                        visit(child)

        visit(self.tr.decl)
        # A sparse witness retains parameter rows and their foreign-key closure.
        changed = True
        while changed:
            changed = False
            for tname, tinfo in self.prog.tables.items():
                sbits = self.table_slot_bits[tname]
                for i in list(keep[tname]):
                    for cname, cinfo in tinfo.columns.items():
                        if cinfo.type.kind != "id64":
                            continue
                        fn = z3.Function(
                            self.pre_col[tname][cname],
                            z3.BitVecSort(sbits),
                            z3.BitVecSort(64),
                        )
                        raw = read(fn(z3.BitVecVal(i, sbits))).as_long()
                        target = cinfo.type.key_table
                        slot = raw & (self.prog.tables[target].layout.capacity - 1)
                        if slot not in keep[target]:
                            keep[target].add(slot)
                            changed = True
        for tname, tinfo in self.prog.tables.items():
            sbits = self.table_slot_bits[tname]
            slot_sort = z3.BitVecSort(sbits)
            occ = z3.Array(self.pre_occ[tname], slot_sort, z3.BoolSort())
            gen = z3.Function(
                self.pre_gen[tname],
                slot_sort,
                z3.BitVecSort(tinfo.layout.generation_bits),
            )
            rows = {}
            for i in range(tinfo.layout.capacity):
                slot = z3.BitVecVal(i, sbits)
                live = (
                    i in keep[tname]
                    if candidate is None
                    else z3.is_true(read(occ[slot]))
                    and (not sparse or i in keep[tname])
                )
                fixed.append(occ[slot] == live)
                fixed.append(gen(slot) == (read(gen(slot)) if live else 0))
                row = {}
                for cname, cinfo in tinfo.columns.items():
                    sort = (
                        z3.BoolSort()
                        if cinfo.type.is_bool
                        else z3.BitVecSort(cinfo.type.width)
                    )
                    term = z3.Function(self.pre_col[tname][cname], slot_sort, sort)(
                        slot
                    )
                    value = (
                        read(term)
                        if live
                        else (
                            z3.BoolVal(False)
                            if cinfo.type.is_bool
                            else z3.BitVecVal(0, cinfo.type.width)
                        )
                    )
                    if candidate is None and live:
                        override = seeded_values.get((tname, i, cname))
                        if override is not None:
                            value = override
                        elif cinfo.type.is_numeric:
                            interval = [cinfo.type.min_value, cinfo.type.max_value]
                            for inv in self.prog.invariants.values():
                                if inv.quant_table == tname:
                                    self._tighten(
                                        inv.expr, inv.quant_var, tname, cname, interval
                                    )
                            value = z3.BitVecVal(
                                min(max(0, interval[0]), interval[1]), cinfo.type.width
                            )
                    fixed.append(term == value)
                    row[cname] = (
                        z3.is_true(value) if cinfo.type.is_bool else value.as_long()
                    )
                if live:
                    rows[i] = row
            for cname, B in tinfo.layout.unique_hashes.items():
                bb = max(1, B.bit_length() - 1)
                keys = [0] * B
                used = [False] * B
                # Rebuild a canonical valid Robin Hood index for the candidate.
                for row_slot, row in rows.items():
                    incoming = row_slot
                    pos = bucket_of(row[cname], B)
                    distance = 0
                    for _ in range(MAX_PROBES):
                        if not used[pos]:
                            used[pos] = True
                            keys[pos] = incoming
                            break
                        resident = keys[pos]
                        resident_dist = (pos - bucket_of(rows[resident][cname], B)) & (
                            B - 1
                        )
                        if resident_dist < distance:
                            keys[pos], incoming = incoming, resident
                            distance = resident_dist
                        pos = (pos + 1) & (B - 1)
                        distance += 1
                for name, entries, sort in (
                    (self.pre_hused[(tname, cname)], used, z3.BoolSort()),
                    (self.pre_hkey[(tname, cname)], keys, slot_sort),
                ):
                    arr = z3.Array(name, z3.BitVecSort(bb), sort)
                    for i, value in enumerate(entries):
                        fixed.append(arr[z3.BitVecVal(i, bb)] == value)
        return fixed

    def _solve_one(
        self,
        name: str,
        candidate: Optional[z3.ModelRef] = None,
        witness_only: bool = False,
    ) -> ObligationResult:
        res = ObligationResult(name)
        start = time.perf_counter()
        if self.diags.has_errors() or self.prog.diags.has_errors():
            res.status = "error"
            return res
        try:
            assertions = z3.parse_smt2_string(self.script_for(name))
            solver = z3.Solver()
            solver.set("timeout", self.config.timeout_ms)
            solver.add(assertions)
            chk = None
            if candidate is not None or (name == "liveness" and self.config.exact_sum):
                proposals = (
                    [(candidate, True), (candidate, False)]
                    if candidate is not None
                    else [(None, True)]
                )
                for proposal, sparse in proposals:
                    proposed = z3.Solver()
                    proposed.set("timeout", min(self.config.timeout_ms, 3000))
                    proposed.add(assertions)
                    proposed.add(self._candidate_constraints(proposal, sparse))
                    proposal_result = proposed.check()
                    if proposal_result == z3.sat:
                        solver, chk = proposed, proposal_result
                        break
            if chk is None:
                if witness_only:
                    res.status = "unknown"
                    res.elapsed_ms = (time.perf_counter() - start) * 1000.0
                    return res
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


def _intern_symbol(text: str) -> int:
    from .types import symbol_id

    return symbol_id(text)


def verify_transition(
    prog: Program, transition_name: str, config: SmtConfig = None
) -> VerificationResult:
    encoder = SmtEncoder(prog, transition_name, config)
    encoder.build()
    return encoder.solve()


def verify_transition_dual(
    prog: Program, transition_name: str, config: SmtConfig = None
) -> VerificationResult:
    """Use abstractions only for UNSAT proofs; SAT witnesses use exact states.

    A frame over-approximates valid pre-states.  UNSAT is therefore a sound
    universal proof, but a frame SAT model need not describe a realizable
    table (for example, independent interval frames lose parity constraints).
    Non-vacuity and reported counterexamples are checked against exact sums
    and every finite pre-state invariant instance.
    """
    from dataclasses import replace

    config = config or SmtConfig()
    frame = SmtEncoder(
        prog, transition_name, replace(config, exact_sum=False, frame_sum=True)
    )
    frame.build()
    exact = SmtEncoder(
        prog, transition_name, replace(config, exact_sum=True, frame_sum=False)
    )
    exact.build()
    exact.witness_source = frame
    result = VerificationResult(
        transition=transition_name,
        safety=frame._solve_one("safety"),
        panic=ObligationResult("panic"),
        liveness=ObligationResult("liveness"),
    )
    safety_encoder = frame
    if result.safety.status == "sat":
        result.safety = exact._solve_one("safety", result.safety.model)
        safety_encoder = exact
    if all(frame.frame_ok.values()):
        result.panic = frame._solve_one("panic")
        panic_encoder = frame
    else:
        result.panic = exact._solve_one("panic")
        panic_encoder = exact
    if result.panic.status == "sat" and panic_encoder is frame:
        result.panic = exact._solve_one("panic", result.panic.model)
        panic_encoder = exact
    result.liveness = exact._solve_one("liveness", witness_only=True)
    if result.liveness.status != "sat":
        frame_liveness = frame._solve_one("liveness")
        result.liveness = exact._solve_one("liveness", frame_liveness.model)
    result.diagnostics = (
        list(prog.diags.errors) + list(frame.diags.items) + list(exact.diags.items)
    )
    result.encoders = {
        "safety": safety_encoder,
        "panic": panic_encoder,
        "liveness": exact,
    }
    scripts = []
    for name in ("safety", "panic", "liveness"):
        obligation = getattr(result, name)
        encoder = result.encoders[name]
        # Export the actual finite SAT witness constraints. Each query has its
        # own declaration scope; concatenated encoders otherwise redeclare all
        # parameters and table symbols and are not valid SMT-LIB.
        query = (
            obligation.solver.to_smt2()
            if obligation.status == "sat" and obligation.solver is not None
            else encoder.script_for(name) + "(check-sat)\n"
        )
        scripts.append(f"; ---- {name}: {obligation.status} ----\n" + query)
    result.script = "\n(reset)\n".join(scripts)
    return result


def verify_module(
    prog: Program, config: SmtConfig = None, dual: bool = True
) -> List[VerificationResult]:
    fn = verify_transition_dual if dual else verify_transition
    return [fn(prog, name, config) for name in prog.transitions]
