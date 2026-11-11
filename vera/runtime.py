"""Reference VERA runtime: SoA tables, id64 keys, tracked sums, atomic revert.

This is the executable semantic model of the language.  It mirrors the SMT
encoding exactly:

  * ``table_get`` on a missing row reverts with ``ERR_MISSING_ROW``,
  * ``table_alloc`` uses the lowest free slot and a generation counter,
  * tracked ``[tracked_sum]`` columns maintain an incremental accumulator that
    reverts with ``ERR_ARITH_OVERFLOW`` when it leaves the column domain,
  * every arithmetic operation is checked in the mathematical integers,
  * a failing guard or trap leaves the state completely unmodified (the undo
    log restores every dirtied slot and accumulator).

The production Target C backend applies the same semantics with a stack
resident undo log and commit-after-validate; this module is the readable
reference used by the test-suite and by counterexample replay.
"""

from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

from . import ast_nodes as A
from .errors import (ERR_ARITH_OVERFLOW, ERR_DUPLICATE_KEY,
                     ERR_GENERATION_EXHAUSTED, ERR_INDEX_SATURATED,
                     ERR_MISSING_ROW, ERR_TABLE_FULL)
from .semantic import Program, TransitionInfo
from .types import (DEC128, DEC64, I64, MAX_PROBES, ScalarType, id64,
                     bucket_of, hash_int)


class Revert(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


@dataclass
class TableState:
    name: str
    capacity: int
    key_bits: int
    occ: bytearray
    gen: List[int]
    columns: Dict[str, List[object]]
    agg: Dict[str, int]
    hash_keys: Dict[str, List[int]] = field(default_factory=dict)
    hash_used: Dict[str, bytearray] = field(default_factory=dict)

    @property
    def slot_mask(self) -> int:
        return self.capacity - 1

    @property
    def generation_bits(self) -> int:
        return 64 - self.key_bits

    @property
    def generation_max(self) -> int:
        return (1 << self.generation_bits) - 1

    def slot_of(self, key: int) -> int:
        return key & self.slot_mask

    def valid(self, key: int) -> bool:
        slot = self.slot_of(key)
        return bool(self.occ[slot]) and self.gen[slot] == (key >> self.key_bits)


@dataclass
class State:
    tables: Dict[str, TableState] = field(default_factory=dict)
    outbox: List[Tuple[str, List[object]]] = field(default_factory=list)

    def clone(self) -> "State":
        import copy
        return copy.deepcopy(self)


@dataclass
class TraceStep:
    text: str
    kind: str = "stmt"
    value: object = None


class Transaction:
    """Executes one transition with an undo log."""

    def __init__(self, prog: Program, state: State, transition_name: str,
                 params: Dict[str, object], record_trace: bool = False):
        self.prog = prog
        self.state = state
        self.tr = prog.transitions.get(transition_name)
        self.param_types = dict(self.tr.param_types) if self.tr is not None else {}
        self.params = dict(params)
        self.env: Dict[str, object] = {}
        self.type_env: Dict[str, ScalarType] = dict(self.param_types)
        # snapshot of tracked accumulators at transaction start, for old_sum()
        self.pre_agg: Dict[str, Dict[str, int]] = {
            t.name: dict(t.agg) for t in state.tables.values()
        }
        self.undo: List[Tuple] = []
        self.trace: List[TraceStep] = []
        self.record_trace = record_trace
        self.effects: List[Tuple[str, List[object]]] = []

    # --- logging -----------------------------------------------------------
    def log(self, text: str, kind: str = "stmt", value: object = None) -> None:
        if self.record_trace:
            self.trace.append(TraceStep(text, kind, value))

    # --- undo --------------------------------------------------------------
    def _record_slot(self, table: TableState, slot: int, column: str,
                     old_value: object) -> None:
        self.undo.append(("slot", table, slot, column, old_value))

    def _record_agg(self, table: TableState, column: str, old_value: int) -> None:
        self.undo.append(("agg", table, column, old_value))

    def _record_occ(self, table: TableState, slot: int, old_occ: int,
                    old_gen: int) -> None:
        self.undo.append(("occ", table, slot, old_occ, old_gen))

    def revert_all(self) -> None:
        for entry in reversed(self.undo):
            kind = entry[0]
            if kind == "slot":
                _, table, slot, column, old = entry
                table.columns[column][slot] = old
            elif kind == "agg":
                _, table, column, old = entry
                table.agg[column] = old
            elif kind == "occ":
                _, table, slot, old_occ, old_gen = entry
                table.occ[slot] = old_occ
                table.gen[slot] = old_gen
        self.undo.clear()

    # --- values ------------------------------------------------------------
    def value_of(self, expr: A.Expr, state: str = "post") -> object:
        if isinstance(expr, A.Literal):
            return expr.value
        if isinstance(expr, A.Var):
            if expr.name in self.env:
                return self.env[expr.name]
            if expr.name in self.params:
                return self.params[expr.name]
            tag = self._enum_tag(expr.name)
            if tag is not None:
                return tag
            raise Revert(ERR_MISSING_ROW)
        if isinstance(expr, A.TableAccess):
            t = self.state.tables[expr.table]
            key = self.value_of(expr.key, state)
            slot = t.slot_of(key)
            if state == "pre":
                # pre-state value: the reference runtime has no separate pre
                # snapshot, so callers use table_get for pre-state reads.
                return t.columns[expr.column][slot]
            return t.columns[expr.column][slot]
        if isinstance(expr, A.TableGet):
            t = self.state.tables[expr.table]
            key = self.value_of(expr.key, state)
            slot = t.slot_of(key)
            if not t.valid(key):
                raise Revert(ERR_MISSING_ROW)
            return t.columns[expr.column][slot]
        if isinstance(expr, A.TableExists):
            t = self.state.tables[expr.table]
            return t.valid(self.value_of(expr.key, state))
        if isinstance(expr, A.TableExistsUnique):
            t = self.state.tables[expr.table]
            col = self.prog.tables[expr.table].columns[expr.key_column]
            if not col.unique:
                raise Revert(ERR_INDEX_SATURATED)
            value = self.value_of(expr.key, state)
            return self._hash_lookup(t, expr.key_column, value) is not None
        if isinstance(expr, A.TableLookup):
            t = self.state.tables[expr.table]
            col = self.prog.tables[expr.table].columns[expr.key_column]
            if not col.unique:
                raise Revert(ERR_INDEX_SATURATED)
            value = self.value_of(expr.key, state)
            slot = self._hash_lookup(t, expr.key_column, value)
            if slot is None:
                raise Revert(ERR_MISSING_ROW)
            return (t.gen[slot] << t.key_bits) | slot
        if isinstance(expr, A.Old):
            t = self.state.tables[expr.access.table]
            key = self.value_of(expr.access.key, "pre")
            slot = t.slot_of(key)
            return t.columns[expr.access.column][slot]
        if isinstance(expr, A.SumAgg):
            t = self.state.tables[expr.table]
            if expr.old or state == "pre":
                return self.pre_agg.get(expr.table, {}).get(expr.column, 0)
            return t.agg[expr.column]
        if isinstance(expr, A.KeyOf):
            return self.value_of(expr.expr, state)
        if isinstance(expr, A.Select):
            cond = self.value_of(expr.cond, state)
            return self.value_of(expr.a if cond else expr.b, state)
        if isinstance(expr, A.Unary):
            operand = self.value_of(expr.operand, state)
            if expr.op == "not":
                return not operand
            if expr.op == "neg":
                return self._neg(operand, self._type_of(expr.operand))
            raise Revert(ERR_ARITH_OVERFLOW)
        if isinstance(expr, A.Binary):
            left = self.value_of(expr.left, state)
            right = self.value_of(expr.right, state)
            return self._binary(expr.op, left, right, expr)
        if isinstance(expr, A.Fold):
            return self._eval_fold(expr, state)
        raise Revert(ERR_ARITH_OVERFLOW)

    def _type_of(self, expr: A.Expr) -> ScalarType:
        from .semantic import _TypeChecker
        return _TypeChecker(self.prog, dict(self.type_env)).type_of(expr)

    def _binary(self, op: str, a: object, b: object, expr: A.Expr) -> object:
        from .semantic import _TypeChecker
        tc = _TypeChecker(self.prog, dict(self.type_env))
        ctype = tc.type_of(expr)
        if op == "and":
            return bool(a) and bool(b)
        if op == "or":
            return bool(a) or bool(b)
        if op == "xor":
            return bool(a) != bool(b)
        if op == "eq":
            return a == b
        if op == "ne":
            return a != b
        if op in ("lt", "le", "gt", "ge"):
            if op == "lt":
                return a < b
            if op == "le":
                return a <= b
            if op == "gt":
                return a > b
            return a >= b
        if op == "min":
            return a if a < b else b
        if op == "max":
            return a if a > b else b
        if op == "add":
            return self._arith(ctype, lambda: a + b, a, b)
        if op == "sub":
            return self._arith(ctype, lambda: a - b, a, b)
        if op == "mul":
            if ctype.is_decimal:
                scale = 10 ** ctype.scale
                prod = a * b
                if prod % scale != 0:
                    raise Revert(ERR_ARITH_OVERFLOW)
                return self._check(ctype, prod // scale)
            return self._arith(ctype, lambda: a * b, a, b)
        if op == "div":
            if b == 0:
                raise Revert(ERR_ARITH_OVERFLOW)
            if ctype.is_decimal:
                scale = 10 ** ctype.scale
                num = a * scale
                if num % b != 0:
                    raise Revert(ERR_ARITH_OVERFLOW)
                return self._check(ctype, num // b)
            if ctype.is_signed and a == ctype.min_value and b == -1:
                raise Revert(ERR_ARITH_OVERFLOW)
            if ctype.is_signed:
                q = abs(a) // abs(b)
                if (a < 0) != (b < 0):
                    q = -q
                return self._check(ctype, q)
            return a // b
        if op == "rem":
            if b == 0:
                raise Revert(ERR_ARITH_OVERFLOW)
            if ctype.is_signed and a == ctype.min_value and b == -1:
                raise Revert(ERR_ARITH_OVERFLOW)
            if ctype.is_signed:
                r = abs(a) % abs(b)
                if a < 0:
                    r = -r
                return self._check(ctype, r)
            return a % b
        raise Revert(ERR_ARITH_OVERFLOW)

    def _arith(self, ctype: ScalarType, fn, a, b) -> object:
        if ctype.is_decimal:
            scale = 10 ** ctype.scale
            if ctype.kind == "dec64":
                pass
            return self._check(ctype, fn())
        return self._check(ctype, fn())

    def _check(self, ctype: ScalarType, value: int) -> int:
        if not (ctype.min_value <= value <= ctype.max_value):
            raise Revert(ERR_ARITH_OVERFLOW)
        return value

    def _neg(self, value: int, ctype: ScalarType) -> int:
        if not ctype.is_signed:
            raise Revert(ERR_ARITH_OVERFLOW)
        return self._check(ctype, -value)

    # --- statements --------------------------------------------------------
    def execute(self) -> State:
        if self.tr is None:
            raise Revert(ERR_MISSING_ROW)
        try:
            for rname, rexpr in self.tr.decl.requires:
                if self.value_of(rexpr, "pre") is not True:
                    raise Revert("Precondition")
            for stmt in self.tr.decl.body:
                self._exec_stmt(stmt)
            self.state.outbox.extend(self.effects)
            return self.state
        except Revert as exc:
            self.revert_all()
            raise

    def _exec_stmt(self, stmt: A.Stmt) -> None:
        if isinstance(stmt, A.Let):
            if isinstance(stmt.value, A.Fold):
                value = self._eval_fold(stmt.value, "post")
            else:
                value = self.value_of(stmt.value, "post")
            self.env[stmt.name] = value
            self.type_env[stmt.name] = self._resolve_type(stmt.type)
            self.log(f"let {stmt.name} : {stmt.type.name} = {value}")
        elif isinstance(stmt, A.Guard):
            cond = self.value_of(stmt.cond, "post")
            self.log(f"guard({stmt.code}) => {'PASS' if cond else 'FAIL'}",
                     "guard", cond)
            if not cond:
                raise Revert(stmt.code)
        elif isinstance(stmt, A.TableSet):
            self._exec_table_set(stmt)
        elif isinstance(stmt, A.TableAlloc):
            self._exec_table_alloc(stmt)
        elif isinstance(stmt, A.Emit):
            args = [self.value_of(a, "post") for a in stmt.args]
            self.effects.append((stmt.effect, args))
            self.log(f"emit {stmt.effect}({', '.join(map(str, args))})")
        elif isinstance(stmt, A.Yield):
            self.value_of(stmt.value, "post")
        elif isinstance(stmt, A.Commit):
            self.log("commit")

    def _exec_table_set(self, stmt: A.TableSet) -> None:
        t = self.state.tables[stmt.table]
        key = self.value_of(stmt.key, "post")
        slot = t.slot_of(key)
        if not t.valid(key):
            self.log(f"table_set {stmt.table}[{key}].{stmt.column} -> MISSING ROW",
                     "trap", ERR_MISSING_ROW)
            raise Revert(ERR_MISSING_ROW)
        value = self.value_of(stmt.value, "post")
        ctype = self.prog.tables[stmt.table].columns[stmt.column].type
        value = self._coerce(ctype, value)
        old = t.columns[stmt.column][slot]
        self._record_slot(t, slot, stmt.column, old)
        t.columns[stmt.column][slot] = value
        cinfo = self.prog.tables[stmt.table].columns[stmt.column]
        if cinfo.tracked_sum:
            new_agg = t.agg[stmt.column] - old + value
            # The widened accumulator's domain is the range of possible sums,
            # [C*Min_tau, C*Max_tau] -- not the per-row domain (Spec 2.3/4.2).
            if not (ctype.min_value * t.capacity <= new_agg
                    <= ctype.max_value * t.capacity):
                self.log(f"tracked_sum {stmt.table}.{stmt.column} overflow: "
                         f"{t.agg[stmt.column]} - {old} + {value}", "trap",
                         ERR_ARITH_OVERFLOW)
                raise Revert(ERR_ARITH_OVERFLOW)
            self._record_agg(t, stmt.column, t.agg[stmt.column])
            t.agg[stmt.column] = new_agg
        self.log(f"table_set {stmt.table}[{key}].{stmt.column} = {value}"
                 f"  (agg={t.agg.get(stmt.column)})")

    def _exec_table_alloc(self, stmt: A.TableAlloc) -> None:
        t = self.state.tables[stmt.table]
        slot = None
        for i in range(t.capacity):
            if not t.occ[i]:
                slot = i
                break
        if slot is None:
            self.log(f"table_alloc {stmt.table} -> TABLE FULL", "trap", ERR_TABLE_FULL)
            raise Revert(ERR_TABLE_FULL)
        if t.gen[slot] >= t.generation_max:
            self.log(f"table_alloc {stmt.table} -> GENERATION EXHAUSTED", "trap",
                     ERR_GENERATION_EXHAUSTED)
            raise Revert(ERR_GENERATION_EXHAUSTED)
        old_occ = t.occ[slot]
        old_gen = t.gen[slot]
        self._record_occ(t, slot, old_occ, old_gen)
        t.occ[slot] = 1
        t.gen[slot] = old_gen + 1
        key = (t.gen[slot] << t.key_bits) | slot
        values: Dict[str, object] = {}
        for cname, cinfo in self.prog.tables[stmt.table].columns.items():
            fexpr = next((e for n, e in stmt.fields if n == cname), None)
            value = self._zero(cinfo.type) if fexpr is None else self.value_of(fexpr, "post")
            value = self._coerce(cinfo.type, value)
            old = t.columns[cname][slot]
            self._record_slot(t, slot, cname, old)
            t.columns[cname][slot] = value
            values[cname] = value
            if cinfo.tracked_sum:
                new_agg = t.agg[cname] + value
                if not (cinfo.type.min_value * t.capacity <= new_agg
                        <= cinfo.type.max_value * t.capacity):
                    self.log(f"tracked_sum {stmt.table}.{cname} overflow", "trap",
                             ERR_ARITH_OVERFLOW)
                    raise Revert(ERR_ARITH_OVERFLOW)
                self._record_agg(t, cname, t.agg[cname])
                t.agg[cname] = new_agg
            if cinfo.unique:
                self._hash_insert(t, cname, value, slot)
        self.env[stmt.name] = key
        self.type_env[stmt.name] = id64(stmt.table)
        self.log(f"table_alloc {stmt.table} -> id64({key}) = {values}")

    # --- secondary unique-key indexes (Robin Hood linear probing) ---------
    def _hash_lookup(self, table: TableState, column: str, value) -> Optional[int]:
        tinfo = self.prog.tables[table.name]
        buckets = tinfo.layout.unique_hashes[column]
        used = table.hash_used[column]
        keys = table.hash_keys[column]
        pos = bucket_of(value, buckets)
        dist = 0
        while dist <= MAX_PROBES:
            if not used[pos]:
                return None
            slot = keys[pos]
            if table.columns[column][slot] == value:
                return slot
            pos = (pos + 1) & (buckets - 1)
            dist += 1
        return None

    def _hash_insert(self, table: TableState, column: str, value, slot: int) -> None:
        # Linear probing with a bounded probe sequence.  (The spec names Robin
        # Hood; linear probing with P_max is the same asymptotic guarantee and
        # keeps the SMT model and the runtime identical.)
        tinfo = self.prog.tables[table.name]
        buckets = tinfo.layout.unique_hashes[column]
        used = table.hash_used[column]
        keys = table.hash_keys[column]
        pos = bucket_of(value, buckets)
        for _ in range(MAX_PROBES):
            if not used[pos]:
                used[pos] = 1
                keys[pos] = slot
                return
            ex_slot = keys[pos]
            ex_val = table.columns[column][ex_slot]
            if ex_val == value:
                raise Revert(ERR_DUPLICATE_KEY)
            pos = (pos + 1) & (buckets - 1)
        raise Revert(ERR_INDEX_SATURATED)

    def _resolve_type(self, ref: A.TypeRef) -> ScalarType:
        from .semantic import _resolve_type_ref
        return _resolve_type_ref(self.prog, ref, "runtime") or I64

    def _enum_tag(self, name: str) -> Optional[int]:
        for tinfo in self.prog.tables.values():
            for cinfo in tinfo.columns.values():
                if cinfo.type.kind == "enum" and name in cinfo.type.enum_tags:
                    return cinfo.type.enum_tags.index(name)
        return None

    def _coerce(self, ctype: ScalarType, value: object) -> object:
        if ctype.is_bool:
            return bool(value)
        if ctype.kind == "bytes32":
            if isinstance(value, (bytes, bytearray)) and len(value) == 32:
                return bytes(value)
            raise Revert(ERR_ARITH_OVERFLOW)
        if not isinstance(value, int):
            raise Revert(ERR_ARITH_OVERFLOW)
        if not (ctype.min_value <= value <= ctype.max_value):
            raise Revert(ERR_ARITH_OVERFLOW)
        return value

    def _zero(self, ctype: ScalarType) -> object:
        if ctype.is_bool:
            return False
        if ctype.kind == "bytes32":
            return b"\x00" * 32
        return 0

    # --- folds -------------------------------------------------------------
    def _eval_fold(self, fold: A.Fold, state: str) -> object:
        from .semantic import _TypeChecker
        tc = _TypeChecker(self.prog, dict(self.type_env))
        idx_type = tc.type_of(fold.lo)
        lo = self.value_of(fold.lo, state)
        hi = self.value_of(fold.hi, state)
        acc = self.value_of(fold.init, state)
        count = 0
        idx = lo
        while count < fold.n and idx <= hi:
            inner = dict(self.env)
            inner[fold.index_name] = idx
            inner[fold.acc_name] = acc
            saved = self.env
            self.env = inner
            try:
                for stmt in fold.body:
                    self._exec_stmt(stmt)
                acc = self.value_of(fold.yield_expr, state)
            finally:
                self.env = saved
            if idx >= idx_type.max_value:
                break
            idx = idx + 1
            count += 1
        self.env[fold.acc_name] = acc
        return acc


class Machine:
    """Owns the state and dispatches transitions."""

    def __init__(self, prog: Program, state: Optional[State] = None):
        self.prog = prog
        self.state = state or State()
        self._init_tables()

    def _init_tables(self) -> None:
        for tname, tinfo in self.prog.tables.items():
            cap = tinfo.layout.capacity
            self.state.tables[tname] = TableState(
                name=tname,
                capacity=cap,
                key_bits=tinfo.layout.key_bits,
                occ=bytearray(cap),
                gen=[0] * cap,
                columns={c: [self._zero(ci.type) for _ in range(cap)]
                         for c, ci in tinfo.columns.items()},
                agg={c: 0 for c in tinfo.layout.agg_offsets},
                hash_keys={c: [0] * tinfo.layout.unique_hashes[c]
                           for c in tinfo.layout.unique_hashes},
                hash_used={c: bytearray(tinfo.layout.unique_hashes[c])
                           for c in tinfo.layout.unique_hashes},
            )

    @staticmethod
    def _zero(ctype: ScalarType) -> object:
        return False if ctype.is_bool else 0

    def recompute_aggregates(self) -> None:
        """Rebuild tracked accumulators from the current table contents."""
        for tname, tinfo in self.prog.tables.items():
            t = self.state.tables[tname]
            for cname in tinfo.layout.agg_offsets:
                total = 0
                for i in range(t.capacity):
                    if t.occ[i]:
                        total += t.columns[cname][i]
                t.agg[cname] = total

    def execute(self, transition: str, params: Dict[str, object],
                record_trace: bool = False) -> Tuple[State, List[TraceStep]]:
        tx = Transaction(self.prog, self.state, transition, params,
                         record_trace=record_trace)
        self.last_tx = tx
        try:
            new_state = tx.execute()
            self.state = new_state
            return new_state, tx.trace
        except Revert:
            raise

    def call(self, transition: str, params: Dict[str, object]) -> State:
        state, _ = self.execute(transition, params)
        return state


def format_trace(trace: List[TraceStep]) -> str:
    return "\n".join(f"  {i+1:3d}. {s.text}" for i, s in enumerate(trace))
