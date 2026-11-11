"""Semantic analysis: symbol tables, strict typing, access footprints, WCET."""

from dataclasses import dataclass, field, fields, is_dataclass
from decimal import Decimal
from typing import Dict, List, Optional, Set, Tuple

from . import ast_nodes as A
from .errors import (
    D_ACCESS_VIOLATION,
    D_BAD_KEY,
    D_BAD_OPERAND,
    D_CAPACITY,
    D_DUPLICATE,
    D_ENUM_TAG,
    D_FOLD_BOUND,
    D_FOLD_DEPTH,
    D_LITERAL_RANGE,
    D_QUANTIFIER,
    D_TYPE_MISMATCH,
    D_UNKNOWN_COLUMN,
    D_UNKNOWN_EFFECT,
    D_UNKNOWN_PARAM,
    D_UNKNOWN_TABLE,
    D_UNKNOWN_TRANSITION,
    D_UNINITIALIZED,
    D_WCET_OVERFLOW,
    DiagnosticBag,
    VeraError,
)
from .types import (
    BOOL,
    BYTES32,
    DEC128,
    DEC64,
    I64,
    SYM32,
    U16,
    U32,
    U64,
    U8,
    ScalarType,
    compute_layout,
    enum_type,
    id64,
    is_primitive_name,
    primitive,
    slot_bits_for,
    literal_value,
    symbol_id,
)


@dataclass
class ColumnInfo:
    name: str
    type: ScalarType
    tracked_sum: bool
    unique: bool = False
    decl: A.ColumnDecl = None


@dataclass
class TableInfo:
    name: str
    decl: A.TableDecl
    key_type: ScalarType
    columns: Dict[str, ColumnInfo] = field(default_factory=dict)
    column_order: List[str] = field(default_factory=list)
    layout: object = None


@dataclass
class AccessRecord:
    table: str
    column: str
    key: Optional[A.Expr]
    tier: int  # 1 = statically resolved, 2 = data-dependent
    line: int = 0
    index: int = 0


@dataclass
class TransitionInfo:
    decl: A.TransitionDecl
    param_types: Dict[str, ScalarType] = field(default_factory=dict)
    reads: List[AccessRecord] = field(default_factory=list)
    writes: List[AccessRecord] = field(default_factory=list)
    agg_writes: List[Tuple[str, str]] = field(default_factory=list)
    wcet_bound: int = 0
    fold_depth: int = 0
    max_fold_product: int = 1
    tier: int = 1
    ssa_types: Dict[str, ScalarType] = field(default_factory=dict)
    def_kind: Dict[str, str] = field(default_factory=dict)


@dataclass
class ProjectionInfo:
    decl: A.ProjectionDecl
    param_types: Dict[str, ScalarType] = field(default_factory=dict)


class Program:
    def __init__(self, module: A.Module):
        self.module = module
        self.diags = DiagnosticBag()
        self.tables: Dict[str, TableInfo] = {}
        self.invariants: Dict[str, A.InvariantDecl] = {}
        self.projections: Dict[str, ProjectionInfo] = {}
        self.effects: Dict[str, A.EffectDecl] = {}
        self.transitions: Dict[str, TransitionInfo] = {}
        self.layouts: Dict[str, object] = {}
        # Type-directed consumers share the analysis result rather than each
        # re-inferring literal widths and fixed-point scales.
        self.expr_types: Dict[int, ScalarType] = {}
        self.literal_values: Dict[int, object] = {}
        self.symbols: Dict[int, str] = {}

    # --- convenience -------------------------------------------------------
    def table(self, name: str) -> Optional[TableInfo]:
        return self.tables.get(name)

    def column(self, table: str, column: str) -> Optional[ColumnInfo]:
        t = self.tables.get(table)
        return t.columns.get(column) if t else None

    def error(
        self,
        code: str,
        message: str,
        line: int = 0,
        col: int = 0,
        details: Optional[List[str]] = None,
    ) -> None:
        self.diags.error(code, message, line, col, details)

    def warn(
        self,
        code: str,
        message: str,
        line: int = 0,
        col: int = 0,
        details: Optional[List[str]] = None,
    ) -> None:
        self.diags.warning(code, message, line, col, details)


def analyze(module: A.Module) -> Program:
    prog = Program(module)
    _collect_tables(prog)
    _check_invariants(prog)
    _check_effects(prog)
    _check_transitions(prog)
    _check_projections(prog)
    return prog


# ---------------------------------------------------------------------------
# Collection / declaration resolution
# ---------------------------------------------------------------------------


def _collect_tables(prog: Program) -> None:
    for decl in prog.module.decls:
        if not isinstance(decl, A.TableDecl):
            continue
        if decl.name in prog.tables:
            prog.error(
                D_DUPLICATE, f"table {decl.name!r} declared twice", decl.line, decl.col
            )
            continue
        if (
            decl.capacity <= 0
            or decl.capacity > (1 << 32)
            or (decl.capacity & (decl.capacity - 1)) != 0
        ):
            prog.error(
                D_CAPACITY,
                f"table {decl.name!r} capacity must be a positive power of two, "
                f"got {decl.capacity}",
                decl.line,
                decl.col,
            )
            continue
        # key type must be id64<self>
        kt = decl.key_type
        if kt.name != "id64" or not kt.type_args:
            prog.error(
                D_BAD_KEY,
                f"table {decl.name!r} key must be id64<{decl.name}>",
                kt.line,
                kt.col,
            )
            continue
        if kt.type_args[0].name != decl.name:
            prog.error(
                D_BAD_KEY,
                f"table {decl.name!r} key must be id64<{decl.name}>, "
                f"got id64<{kt.type_args[0].name}>",
                kt.line,
                kt.col,
            )
            continue
        info = TableInfo(name=decl.name, decl=decl, key_type=id64(decl.name))
        prog.tables[decl.name] = info
    for info in list(prog.tables.values()):
        decl = info.decl
        col_types: List[Tuple[str, ScalarType]] = []
        tracked: List[str] = []
        unique_cols: List[str] = []
        for col in decl.columns:
            if col.name in info.columns:
                prog.error(
                    D_DUPLICATE,
                    f"column {col.name!r} declared twice in table {decl.name!r}",
                    col.line,
                    col.col,
                )
                continue
            ctype = _resolve_type_ref(prog, col.type, decl.name)
            if ctype is None:
                continue
            ci = ColumnInfo(col.name, ctype, col.tracked_sum, col.unique, col)
            if col.tracked_sum and not ctype.is_numeric:
                prog.error(
                    D_BAD_OPERAND,
                    f"column {decl.name}.{col.name} is marked tracked_sum but has "
                    f"non-numeric type {ctype}",
                    col.line,
                    col.col,
                )
            info.columns[col.name] = ci
            info.column_order.append(col.name)
            col_types.append((col.name, ctype))
            if col.tracked_sum:
                tracked.append(col.name)
            if col.unique:
                unique_cols.append(col.name)
        try:
            info.layout = compute_layout(
                decl.name, decl.capacity, col_types, tracked, unique_cols
            )
        except VeraError as exc:
            prog.diags.items.append(exc.diag)
            continue
        prog.tables[decl.name] = info
        prog.layouts[decl.name] = info.layout


def _resolve_type_ref(
    prog: Program, ref: A.TypeRef, context: str
) -> Optional[ScalarType]:
    if ref.name == "id64":
        if len(ref.type_args) != 1:
            prog.error(
                D_BAD_KEY,
                f"id64 in {context} requires exactly one table argument",
                ref.line,
                ref.col,
            )
            return None
        target = ref.type_args[0].name
        if target not in prog.tables:
            prog.error(
                D_UNKNOWN_TABLE,
                f"id64<{target}> in {context} refers to unknown table {target!r}",
                ref.line,
                ref.col,
            )
            return None
        return id64(target)
    if ref.name == "enum":
        tags = tuple(ref.enum_tags)
        if not tags:
            prog.error(D_ENUM_TAG, f"enum in {context} has no tags", ref.line, ref.col)
            return None
        if len(set(tags)) != len(tags):
            prog.error(
                D_ENUM_TAG, f"enum in {context} has duplicate tags", ref.line, ref.col
            )
            return None
        try:
            return enum_type(tags)
        except VeraError as exc:
            prog.diags.items.append(exc.diag)
            return None
    if is_primitive_name(ref.name):
        return primitive(ref.name)
    prog.error(
        D_TYPE_MISMATCH, f"unknown type {ref.name!r} in {context}", ref.line, ref.col
    )
    return None


def _check_spec_purity(
    prog: Program,
    expr: A.Expr,
    depth: int = 0,
    product: int = 1,
    allow_guards: bool = False,
) -> None:
    if isinstance(expr, A.Fold):
        depth += 1
        product *= expr.n
        if depth > 2:
            prog.error(
                D_FOLD_DEPTH,
                "specification fold nesting depth exceeds 2",
                expr.line,
                expr.col,
            )
        if product > 1024:
            prog.error(
                D_FOLD_BOUND,
                "specification nested fold product exceeds 1024",
                expr.line,
                expr.col,
            )
        for stmt in expr.body:
            if not isinstance(stmt, (A.Let, A.Guard) if allow_guards else A.Let):
                prog.error(
                    D_BAD_OPERAND,
                    "specification folds may only bind pure values",
                    stmt.line,
                    stmt.col,
                )
        if expr.n <= 0 or expr.n > 1024:
            prog.error(
                D_FOLD_BOUND,
                "specification fold bound must be between 1 and 1024",
                expr.line,
                expr.col,
            )
    if is_dataclass(expr):
        for f in fields(expr):
            value = getattr(expr, f.name)
            if isinstance(value, list):
                for child in value:
                    if is_dataclass(child):
                        _check_spec_purity(prog, child, depth, product, allow_guards)
            elif is_dataclass(value):
                _check_spec_purity(prog, value, depth, product, allow_guards)


def _check_invariants(prog: Program) -> None:
    for decl in prog.module.decls:
        if not isinstance(decl, A.InvariantDecl):
            continue
        if decl.name in prog.invariants:
            prog.error(
                D_DUPLICATE,
                f"invariant {decl.name!r} declared twice",
                decl.line,
                decl.col,
            )
            continue
        if decl.quant_table is not None:
            if decl.quant_table not in prog.tables:
                prog.error(
                    D_UNKNOWN_TABLE,
                    f"invariant {decl.name!r} quantifies over unknown table "
                    f"{decl.quant_table!r}",
                    decl.line,
                    decl.col,
                )
                continue
            if decl.quant_var is None:
                prog.error(
                    D_QUANTIFIER,
                    f"invariant {decl.name!r} missing quantified variable",
                    decl.line,
                    decl.col,
                )
                continue
        prog.invariants[decl.name] = decl
        # type check the body in an env containing the quantified variable
        env: Dict[str, ScalarType] = {}
        if decl.quant_var and decl.quant_table:
            env[decl.quant_var] = id64(decl.quant_table)
        tc = _TypeChecker(prog, env)
        _check_spec_purity(prog, decl.expr)
        t = tc.type_of(decl.expr)
        if t != BOOL:
            prog.error(
                D_TYPE_MISMATCH,
                f"invariant {decl.name!r} must be boolean, got {t}",
                decl.line,
                decl.col,
            )


def _check_effects(prog: Program) -> None:
    for decl in prog.module.decls:
        if not isinstance(decl, A.EffectDecl):
            continue
        if decl.name in prog.effects:
            prog.error(
                D_DUPLICATE, f"effect {decl.name!r} declared twice", decl.line, decl.col
            )
            continue
        names = set()
        for param in decl.params:
            if param.name in names:
                prog.error(
                    D_DUPLICATE,
                    f"effect parameter {param.name!r} declared twice",
                    param.line,
                    param.col,
                )
            names.add(param.name)
            _resolve_type_ref(prog, param.type, f"effect {decl.name!r}")
        prog.effects[decl.name] = decl


def _check_projections(prog: Program) -> None:
    for decl in prog.module.decls:
        if not isinstance(decl, A.ProjectionDecl):
            continue
        if decl.name in prog.projections:
            prog.error(
                D_DUPLICATE,
                f"projection {decl.name!r} declared twice",
                decl.line,
                decl.col,
            )
            continue
        info = ProjectionInfo(decl)
        env: Dict[str, ScalarType] = {}
        for p in decl.params:
            if p.name in info.param_types:
                prog.error(
                    D_DUPLICATE,
                    f"projection {decl.name!r} parameter {p.name!r} declared twice",
                    p.line,
                    p.col,
                )
                continue
            pt = _resolve_type_ref(prog, p.type, f"projection {decl.name!r}")
            if pt is None:
                continue
            info.param_types[p.name] = pt
            env[p.name] = pt
        tc = _TypeChecker(prog, env)
        if decl.allow_if is not None:
            _check_spec_purity(prog, decl.allow_if)
            t = tc.type_of(decl.allow_if)
            if t != BOOL:
                prog.error(
                    D_TYPE_MISMATCH,
                    f"projection {decl.name!r} allow_if must be boolean, got {t}",
                    decl.line,
                    decl.col,
                )
        for sl in decl.expose:
            _check_table_column(
                prog, sl.table, sl.column, decl.line, decl.col, "projection expose"
            )
            if sl.row is not None:
                rt = tc.type_of(sl.row)
                tbl = prog.table(sl.table)
                if tbl is not None and rt != tbl.key_type:
                    prog.error(
                        D_TYPE_MISMATCH,
                        f"projection {decl.name!r} row expression for "
                        f"{sl.table}.{sl.column} must be {tbl.key_type}, got {rt}",
                        sl.line,
                        sl.col,
                    )
        for ig in decl.enable_intents:
            if ig.transition not in prog.transitions:
                prog.error(
                    D_UNKNOWN_TRANSITION,
                    f"projection {decl.name!r} enables unknown transition "
                    f"{ig.transition!r}",
                    ig.line,
                    ig.col,
                )
                continue
            _check_spec_purity(prog, ig.condition)
            t = tc.type_of(ig.condition)
            if t != BOOL:
                prog.error(
                    D_TYPE_MISMATCH,
                    f"projection {decl.name!r} intent guard must be boolean, got {t}",
                    ig.line,
                    ig.col,
                )
        prog.projections[decl.name] = info


def _check_table_column(
    prog: Program, table: str, column: str, line: int, col: int, context: str
) -> None:
    tbl = prog.table(table)
    if tbl is None:
        prog.error(
            D_UNKNOWN_TABLE, f"{context} refers to unknown table {table!r}", line, col
        )
        return
    if column not in tbl.columns:
        prog.error(
            D_UNKNOWN_COLUMN,
            f"{context} refers to unknown column {table}.{column!r}",
            line,
            col,
        )


# ---------------------------------------------------------------------------
# Type checker
# ---------------------------------------------------------------------------


class _TypeChecker:
    """Strict, coercion-free type checker with literal type inference."""

    def __init__(
        self,
        prog: Program,
        env: Dict[str, ScalarType],
        def_kind: Optional[Dict[str, str]] = None,
    ):
        self.prog = prog
        self.env = env
        self.def_kind = def_kind if def_kind is not None else {}

    def type_of(
        self, expr: A.Expr, expected: Optional[ScalarType] = None
    ) -> ScalarType:
        ctype = self._type_of(expr, expected)
        self.prog.expr_types[id(expr)] = ctype
        if isinstance(expr, A.Literal):
            try:
                self.prog.literal_values[id(expr)] = literal_value(expr.value, ctype)
            except ValueError as exc:
                self.prog.error(D_LITERAL_RANGE, str(exc), expr.line, expr.col)
        return ctype

    def _type_of(
        self, expr: A.Expr, expected: Optional[ScalarType] = None
    ) -> ScalarType:
        if isinstance(expr, A.Literal):
            return self._literal_type(expr, expected)
        if isinstance(expr, A.Var):
            return self._var_type(expr, expected)
        if isinstance(expr, A.TableGet):
            return self._table_get_type(expr)
        if isinstance(expr, A.TableExists):
            self._check_key(expr.table, expr.key, expr.line, expr.col)
            return BOOL
        if isinstance(expr, A.TableExistsUnique):
            tbl = self._require_table(expr.table, expr.line, expr.col)
            if tbl is None:
                return BOOL
            col = tbl.columns.get(expr.key_column)
            if col is None:
                self.prog.error(
                    D_UNKNOWN_COLUMN,
                    f"unknown lookup column {expr.table}.{expr.key_column!r}",
                    expr.line,
                    expr.col,
                )
                return BOOL
            if not col.unique:
                self.prog.error(
                    D_BAD_OPERAND,
                    f"{expr.table}.{expr.key_column} is not declared [unique]",
                    expr.line,
                    expr.col,
                )
            kt = self.type_of(expr.key, col.type)
            if kt != col.type:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"lookup key for {expr.table}.{expr.key_column} must be "
                    f"{col.type}, got {kt}",
                    expr.line,
                    expr.col,
                )
            return BOOL
        if isinstance(expr, A.TableLookup):
            tbl = self._require_table(expr.table, expr.line, expr.col)
            if tbl is None:
                return I64
            col = tbl.columns.get(expr.key_column)
            if col is None:
                self.prog.error(
                    D_UNKNOWN_COLUMN,
                    f"unknown lookup column {expr.table}.{expr.key_column!r}",
                    expr.line,
                    expr.col,
                )
                return I64
            if not col.unique:
                self.prog.error(
                    D_BAD_OPERAND,
                    f"{expr.table}.{expr.key_column} is not declared [unique]",
                    expr.line,
                    expr.col,
                )
            kt = self.type_of(expr.key, col.type)
            if kt != col.type:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"lookup key for {expr.table}.{expr.key_column} must be "
                    f"{col.type}, got {kt}",
                    expr.line,
                    expr.col,
                )
            return id64(expr.table)
        if isinstance(expr, A.TableAccess):
            return self._table_access_type(expr)
        if isinstance(expr, A.Old):
            return self._old_type(expr)
        if isinstance(expr, A.SumAgg):
            return self._sum_type(expr)
        if isinstance(expr, A.KeyOf):
            tbl = self._require_table(expr.table, expr.line, expr.col)
            if tbl is not None:
                target = (
                    U64
                    if isinstance(expr.expr, A.Literal)
                    and type(expr.expr.value) is int
                    and expr.expr.value >= 0
                    else None
                )
                kt = self.type_of(expr.expr, target)
                if not kt.is_integer:
                    self.prog.error(
                        D_TYPE_MISMATCH,
                        f"id64 constructor requires an integer slot expression, "
                        f"got {kt}",
                        expr.line,
                        expr.col,
                    )
            return id64(expr.table)
        if isinstance(expr, A.Select):
            ct = self.type_of(expr.cond, BOOL)
            if ct != BOOL:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"select condition must be bool, got {ct}",
                    expr.line,
                    expr.col,
                )
            branch_type = (
                expected or self._concrete_type(expr.a) or self._concrete_type(expr.b)
            )
            at = self.type_of(expr.a, branch_type)
            bt = self.type_of(expr.b, at)
            if at != bt:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"select branches must have the same type, got {at} and {bt}",
                    expr.line,
                    expr.col,
                )
            return at
        if isinstance(expr, A.Unary):
            return self._unary_type(expr, expected)
        if isinstance(expr, A.Binary):
            return self._binary_type(expr, expected)
        if isinstance(expr, A.Fold):
            return self._fold_type(expr)
        raise VeraError(
            D_TYPE_MISMATCH,
            f"cannot type expression {type(expr).__name__}",
            getattr(expr, "line", 0),
            getattr(expr, "col", 0),
        )

    # --- atoms -------------------------------------------------------------
    def _literal_type(
        self, expr: A.Literal, expected: Optional[ScalarType]
    ) -> ScalarType:
        val = expr.value
        if type(val) is bool:
            return BOOL
        if isinstance(val, str):
            if expected is not None and expected.kind == "enum":
                if val not in expected.enum_tags:
                    self.prog.error(
                        D_ENUM_TAG,
                        f"unknown enum tag {val!r} for {expected}",
                        expr.line,
                        expr.col,
                    )
                return expected
            if expected is None or expected.kind != "sym32":
                self.prog.error(
                    D_TYPE_MISMATCH,
                    "string literals require sym32 or enum context",
                    expr.line,
                    expr.col,
                )
            code = symbol_id(val)
            previous = self.prog.symbols.get(code)
            if previous is not None and previous != val:
                self.prog.error(
                    D_LITERAL_RANGE,
                    f"symbol hash collision between {previous!r} and {val!r}",
                    expr.line,
                    expr.col,
                )
            self.prog.symbols[code] = val
            return SYM32
        if isinstance(val, (Decimal, float)):
            if expected is not None and not expected.is_decimal:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    "fractional literals require a decimal type",
                    expr.line,
                    expr.col,
                )
                return expected
            return expected or DEC64
        if expected is not None:
            if expected.is_bool:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    "integer literal cannot be used as bool",
                    expr.line,
                    expr.col,
                )
            return expected
        return I64

    def _var_type(self, expr: A.Var, expected: Optional[ScalarType]) -> ScalarType:
        name = expr.name
        if name in self.env:
            return self.env[name]
        # enum tag used as a literal
        if (
            expected is not None
            and expected.kind == "enum"
            and name in expected.enum_tags
        ):
            return expected
        self.prog.error(
            D_UNKNOWN_PARAM, f"unknown variable {name!r}", expr.line, expr.col
        )
        return expected if expected is not None else I64

    def _require_table(self, name: str, line: int, col: int) -> Optional[TableInfo]:
        tbl = self.prog.table(name)
        if tbl is None:
            self.prog.error(D_UNKNOWN_TABLE, f"unknown table {name!r}", line, col)
        return tbl

    def _check_key(self, table: str, key: A.Expr, line: int, col: int) -> ScalarType:
        tbl = self._require_table(table, line, col)
        kt = self.type_of(key)
        if tbl is not None and kt != tbl.key_type:
            self.prog.error(
                D_TYPE_MISMATCH,
                f"{table} key must be {tbl.key_type}, got {kt}",
                line,
                col,
            )
        return kt

    def _table_get_type(self, expr: A.TableGet) -> ScalarType:
        self._check_key(expr.table, expr.key, expr.line, expr.col)
        tbl = self.prog.table(expr.table)
        if tbl is None:
            return I64
        col = tbl.columns.get(expr.column)
        if col is None:
            self.prog.error(
                D_UNKNOWN_COLUMN,
                f"unknown column {expr.table}.{expr.column!r}",
                expr.line,
                expr.col,
            )
            return I64
        return col.type

    def _table_access_type(self, expr: A.TableAccess) -> ScalarType:
        tbl = self._require_table(expr.table, expr.line, expr.col)
        if expr.key is None:
            self.prog.error(
                D_TYPE_MISMATCH,
                f"{expr.table}.{expr.column} exposes all rows; "
                f"a scalar row key is required",
                expr.line,
                expr.col,
            )
        else:
            self._check_key(expr.table, expr.key, expr.line, expr.col)
        if tbl is None:
            return I64
        col = tbl.columns.get(expr.column)
        if col is None:
            self.prog.error(
                D_UNKNOWN_COLUMN,
                f"unknown column {expr.table}.{expr.column!r}",
                expr.line,
                expr.col,
            )
            return I64
        return col.type

    def _old_type(self, expr: A.Old) -> ScalarType:
        acc = expr.access
        tbl = self._require_table(acc.table, expr.line, expr.col)
        if acc.key is not None:
            _check_spec_purity(self.prog, acc.key, allow_guards=True)
            self._check_key(acc.table, acc.key, expr.line, expr.col)
        else:
            self.prog.error(
                D_TYPE_MISMATCH, "old() requires a scalar row key", expr.line, expr.col
            )
        if tbl is None:
            return I64
        col = tbl.columns.get(acc.column)
        if col is None:
            self.prog.error(
                D_UNKNOWN_COLUMN,
                f"unknown column {acc.table}.{acc.column!r}",
                expr.line,
                expr.col,
            )
            return I64
        return col.type

    def _sum_type(self, expr: A.SumAgg) -> ScalarType:
        tbl = self._require_table(expr.table, expr.line, expr.col)
        if tbl is None:
            return I64
        col = tbl.columns.get(expr.column)
        if col is None:
            self.prog.error(
                D_UNKNOWN_COLUMN,
                f"unknown column {expr.table}.{expr.column!r}",
                expr.line,
                expr.col,
            )
            return I64
        if not col.tracked_sum:
            self.prog.error(
                D_BAD_OPERAND,
                f"sum({expr.table}.{expr.column}) requires the column to be "
                f"marked [tracked_sum]",
                expr.line,
                expr.col,
            )
        return col.type

    # --- operators ---------------------------------------------------------
    def _unary_type(self, expr: A.Unary, expected: Optional[ScalarType]) -> ScalarType:
        if expr.op == "not":
            t = self.type_of(expr.operand, BOOL)
            if t != BOOL:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"not() requires bool, got {t}",
                    expr.line,
                    expr.col,
                )
            return BOOL
        if expr.op == "neg":
            t = self.type_of(expr.operand, expected)
            if not t.is_signed:
                self.prog.error(
                    D_BAD_OPERAND,
                    f"neg() requires a signed operand, got {t}",
                    expr.line,
                    expr.col,
                )
            return t
        self.prog.error(
            D_BAD_OPERAND, f"unknown unary operator {expr.op!r}", expr.line, expr.col
        )
        return expected if expected is not None else I64

    def _binary_type(
        self, expr: A.Binary, expected: Optional[ScalarType]
    ) -> ScalarType:
        op = expr.op
        if op in ("and", "or", "xor"):
            lt = self.type_of(expr.left, BOOL)
            rt = self.type_of(expr.right, BOOL)
            if lt != BOOL:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"{op}() left operand must be bool, got {lt}",
                    expr.line,
                    expr.col,
                )
            if rt != BOOL:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"{op}() right operand must be bool, got {rt}",
                    expr.line,
                    expr.col,
                )
            return BOOL
        if op in ("add", "sub", "mul", "div", "rem", "min", "max"):
            target = self._infer_arith_target(expr, expected)
            lt = self.type_of(expr.left, target)
            rt = self.type_of(expr.right, target)
            if lt != rt:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"{op}() operands must have the same type, got {lt} and {rt}",
                    expr.line,
                    expr.col,
                )
                return lt
            if not lt.is_numeric:
                self.prog.error(
                    D_BAD_OPERAND,
                    f"{op}() requires numeric operands, got {lt}",
                    expr.line,
                    expr.col,
                )
            return lt
        if op in ("eq", "ne"):
            target = self._infer_equality_target(expr, None)
            lt = self.type_of(expr.left, target)
            rt = self.type_of(expr.right, lt)
            if lt != rt:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"{op}() operands must have the same type, got {lt} and {rt}",
                    expr.line,
                    expr.col,
                )
            return BOOL
        if op in ("lt", "le", "gt", "ge"):
            target = self._infer_arith_target(expr, None)
            lt = self.type_of(expr.left, target)
            rt = self.type_of(expr.right, target)
            if lt != rt:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"{op}() operands must have the same type, got {lt} and {rt}",
                    expr.line,
                    expr.col,
                )
                return BOOL
            if not lt.is_ordered:
                self.prog.error(
                    D_BAD_OPERAND,
                    f"{op}() requires ordered operands, got {lt}",
                    expr.line,
                    expr.col,
                )
            return BOOL
        self.prog.error(
            D_BAD_OPERAND, f"unknown binary operator {op!r}", expr.line, expr.col
        )
        return expected if expected is not None else I64

    def _concrete_type(self, expr: A.Expr) -> Optional[ScalarType]:
        """Return the type if it is determined without integer literal defaults."""
        if isinstance(expr, A.Literal):
            if type(expr.value) is bool:
                return BOOL
            return None
        if isinstance(expr, A.Var):
            return self.env.get(expr.name)
        if isinstance(expr, A.TableGet):
            tbl = self.prog.table(expr.table)
            if tbl and expr.column in tbl.columns:
                return tbl.columns[expr.column].type
            return None
        if isinstance(expr, A.TableAccess):
            tbl = self.prog.table(expr.table)
            if tbl and expr.column in tbl.columns:
                return tbl.columns[expr.column].type
            return None
        if isinstance(expr, A.Old):
            tbl = self.prog.table(expr.access.table)
            if tbl and expr.access.column in tbl.columns:
                return tbl.columns[expr.access.column].type
            return None
        if isinstance(expr, A.SumAgg):
            tbl = self.prog.table(expr.table)
            if tbl and expr.column in tbl.columns:
                return tbl.columns[expr.column].type
            return None
        if isinstance(expr, A.KeyOf):
            return id64(expr.table)
        if isinstance(expr, A.Select):
            return self._concrete_type(expr.a) or self._concrete_type(expr.b)
        if isinstance(expr, A.Fold):
            return _resolve_type_ref(self.prog, expr.acc_type, "fold accumulator")
        if isinstance(expr, A.Binary):
            if expr.op in ("eq", "ne", "lt", "le", "gt", "ge", "and", "or", "xor"):
                return BOOL
            lt = self._concrete_type(expr.left)
            rt = self._concrete_type(expr.right)
            if lt is not None:
                return lt
            return rt
        if isinstance(expr, A.Unary):
            if expr.op == "not":
                return BOOL
            return self._concrete_type(expr.operand)
        return None

    def _infer_arith_target(
        self, expr: A.Binary, expected: Optional[ScalarType]
    ) -> Optional[ScalarType]:
        if expected is not None:
            return expected
        candidates: List[ScalarType] = []
        for operand in (expr.left, expr.right):
            ct = self._concrete_type(operand)
            if ct is not None and ct.is_numeric:
                candidates.append(ct)
        if candidates:
            first = candidates[0]
            if all(c == first for c in candidates):
                return first
        return None

    def _infer_equality_target(
        self, expr: A.Binary, expected: Optional[ScalarType]
    ) -> Optional[ScalarType]:
        if expected is not None:
            return expected
        candidates: List[ScalarType] = []
        for operand in (expr.left, expr.right):
            ct = self._concrete_type(operand)
            if ct is not None:
                candidates.append(ct)
        if candidates:
            first = candidates[0]
            if all(c == first for c in candidates):
                return first
        return None

    def _fold_type(self, expr: A.Fold) -> ScalarType:
        acc_type = _resolve_type_ref(self.prog, expr.acc_type, "fold accumulator")
        if acc_type is None:
            return I64
        init_t = self.type_of(expr.init, acc_type)
        if init_t != acc_type:
            self.prog.error(
                D_TYPE_MISMATCH,
                f"fold initializer must be {acc_type}, got {init_t}",
                expr.line,
                expr.col,
            )
        range_type = self._concrete_type(expr.lo) or self._concrete_type(expr.hi)
        lo_t = self.type_of(expr.lo, range_type)
        hi_t = self.type_of(expr.hi, lo_t)
        if lo_t != hi_t:
            self.prog.error(
                D_TYPE_MISMATCH,
                f"fold range bounds must have the same type, got {lo_t} and {hi_t}",
                expr.line,
                expr.col,
            )
        if not lo_t.is_integer:
            self.prog.error(
                D_BAD_OPERAND,
                f"fold range must be an integer, got {lo_t}",
                expr.line,
                expr.col,
            )
        if (
            expr.acc_name == expr.index_name
            or expr.acc_name in self.env
            or expr.index_name in self.env
        ):
            self.prog.error(
                D_DUPLICATE,
                "fold bindings must be distinct fresh names",
                expr.line,
                expr.col,
            )
        inner_env = dict(self.env)
        inner_env[expr.acc_name] = acc_type
        inner_env[expr.index_name] = lo_t
        inner = _TypeChecker(self.prog, inner_env, dict(self.def_kind))
        for stmt in expr.body:
            if isinstance(stmt, (A.Commit, A.Yield)):
                self.prog.error(
                    D_BAD_OPERAND,
                    "fold body cannot commit or yield before its final yield",
                    stmt.line,
                    stmt.col,
                )
            inner.check_stmt(stmt)
        yield_t = inner.type_of(expr.yield_expr, acc_type)
        if yield_t != acc_type:
            self.prog.error(
                D_TYPE_MISMATCH,
                f"fold yield must be {acc_type}, got {yield_t}",
                expr.line,
                expr.col,
            )
        return acc_type

    # --- statements ---------------------------------------------------------
    def check_stmt(self, stmt: A.Stmt) -> None:
        if isinstance(stmt, A.Let):
            if stmt.name in self.env:
                self.prog.error(
                    D_DUPLICATE,
                    f"SSA binding {stmt.name!r} is already defined",
                    stmt.line,
                    stmt.col,
                )
            t = _resolve_type_ref(self.prog, stmt.type, "let binding")
            if t is None:
                return
            vt = self.type_of(stmt.value, t)
            if vt != t:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"let {stmt.name} declares {t} but value is {vt}",
                    stmt.line,
                    stmt.col,
                )
            self.env[stmt.name] = t
            self.def_kind[stmt.name] = self._expr_kind(stmt.value)
        elif isinstance(stmt, A.Guard):
            t = self.type_of(stmt.cond, BOOL)
            if t != BOOL:
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"guard condition must be bool, got {t}",
                    stmt.line,
                    stmt.col,
                )
        elif isinstance(stmt, A.TableSet):
            tbl = self._require_table(stmt.table, stmt.line, stmt.col)
            if tbl is not None:
                self._check_key(stmt.table, stmt.key, stmt.line, stmt.col)
                col = tbl.columns.get(stmt.column)
                if col is None:
                    self.prog.error(
                        D_UNKNOWN_COLUMN,
                        f"unknown column {stmt.table}.{stmt.column!r}",
                        stmt.line,
                        stmt.col,
                    )
                elif col.unique:
                    self.prog.error(
                        D_BAD_OPERAND,
                        f"table_set on unique column {stmt.table}.{stmt.column!r} "
                        f"is forbidden (secondary indexes are immutable)",
                        stmt.line,
                        stmt.col,
                    )
                else:
                    vt = self.type_of(stmt.value, col.type)
                    if vt != col.type:
                        self.prog.error(
                            D_TYPE_MISMATCH,
                            f"table_set {stmt.table}.{stmt.column} expects "
                            f"{col.type}, got {vt}",
                            stmt.line,
                            stmt.col,
                        )
        elif isinstance(stmt, A.TableAlloc):
            if stmt.name in self.env:
                self.prog.error(
                    D_DUPLICATE,
                    f"SSA binding {stmt.name!r} is already defined",
                    stmt.line,
                    stmt.col,
                )
            tbl = self._require_table(stmt.table, stmt.line, stmt.col)
            if tbl is not None:
                seen: Set[str] = set()
                for fname, fexpr in stmt.fields:
                    col = tbl.columns.get(fname)
                    if col is None:
                        self.prog.error(
                            D_UNKNOWN_COLUMN,
                            f"table_alloc {stmt.table} initializes unknown "
                            f"column {fname!r}",
                            stmt.line,
                            stmt.col,
                        )
                        continue
                    if fname in seen:
                        self.prog.error(
                            D_DUPLICATE,
                            f"table_alloc {stmt.table} initializes column "
                            f"{fname!r} twice",
                            stmt.line,
                            stmt.col,
                        )
                    seen.add(fname)
                    ft = self.type_of(fexpr, col.type)
                    if ft != col.type:
                        self.prog.error(
                            D_TYPE_MISMATCH,
                            f"table_alloc {stmt.table}.{fname} expects "
                            f"{col.type}, got {ft}",
                            stmt.line,
                            stmt.col,
                        )
                missing = [c for c in tbl.column_order if c not in seen]
                if missing:
                    self.prog.error(
                        D_UNINITIALIZED,
                        f"table_alloc {stmt.table} leaves columns uninitialized: "
                        f"{', '.join(missing)}",
                        stmt.line,
                        stmt.col,
                    )
                self.env[stmt.name] = id64(stmt.table)
                self.def_kind[stmt.name] = "alloc"
        elif isinstance(stmt, A.Emit):
            eff = self.prog.effects.get(stmt.effect)
            if eff is None:
                self.prog.error(
                    D_UNKNOWN_EFFECT,
                    f"unknown effect {stmt.effect!r}",
                    stmt.line,
                    stmt.col,
                )
                for arg in stmt.args:
                    self.type_of(arg)
                return
            if len(stmt.args) != len(eff.params):
                self.prog.error(
                    D_TYPE_MISMATCH,
                    f"effect {stmt.effect} expects {len(eff.params)} "
                    f"arguments, got {len(stmt.args)}",
                    stmt.line,
                    stmt.col,
                )
            for arg, param in zip(stmt.args, eff.params):
                pt = _resolve_type_ref(self.prog, param.type, f"effect {eff.name!r}")
                if pt is None:
                    continue
                at = self.type_of(arg, pt)
                if at != pt:
                    self.prog.error(
                        D_TYPE_MISMATCH,
                        f"effect {stmt.effect} argument {param.name} expects "
                        f"{pt}, got {at}",
                        stmt.line,
                        stmt.col,
                    )
        elif isinstance(stmt, A.Yield):
            self.type_of(stmt.value)
        elif isinstance(stmt, A.Commit):
            pass
        else:
            self.prog.error(
                D_TYPE_MISMATCH,
                f"cannot type statement {type(stmt).__name__}",
                getattr(stmt, "line", 0),
                getattr(stmt, "col", 0),
            )

    def _expr_kind(self, expr: A.Expr) -> str:
        if isinstance(expr, (A.TableGet, A.TableLookup, A.TableExistsUnique)):
            return "read"
        if isinstance(expr, A.KeyOf):
            return "key"
        if isinstance(expr, A.Var):
            return self.def_kind.get(expr.name, "pure")
        if isinstance(expr, A.Binary):
            return (
                self._expr_kind(expr.left)
                if self._expr_kind(expr.left) != "pure"
                else self._expr_kind(expr.right)
            )
        if isinstance(expr, A.Unary):
            return self._expr_kind(expr.operand)
        if isinstance(expr, A.Select):
            return (
                self._expr_kind(expr.a)
                if self._expr_kind(expr.a) != "pure"
                else self._expr_kind(expr.b)
            )
        return "pure"


# ---------------------------------------------------------------------------
# Transition checking
# ---------------------------------------------------------------------------


def _check_transitions(prog: Program) -> None:
    for decl in prog.module.decls:
        if not isinstance(decl, A.TransitionDecl):
            continue
        if decl.name in prog.transitions:
            prog.error(
                D_DUPLICATE,
                f"transition {decl.name!r} declared twice",
                decl.line,
                decl.col,
            )
            continue
        info = TransitionInfo(decl)
        env: Dict[str, ScalarType] = {}
        for p in decl.params:
            if p.name in info.param_types:
                prog.error(
                    D_DUPLICATE,
                    f"transition {decl.name!r} parameter {p.name!r} declared twice",
                    p.line,
                    p.col,
                )
                continue
            pt = _resolve_type_ref(prog, p.type, f"transition {decl.name!r}")
            if pt is None:
                continue
            info.param_types[p.name] = pt
            env[p.name] = pt
        # declared access paths
        declared_reads = _check_access_paths(prog, decl.reads, env, "reads", decl)
        declared_writes = _check_access_paths(prog, decl.writes, env, "writes", decl)
        # requires / ensures
        tc = _TypeChecker(prog, env)
        for rname, rexpr in decl.requires:
            _check_spec_purity(prog, rexpr)
            t = tc.type_of(rexpr, BOOL)
            if t != BOOL:
                prog.error(
                    D_TYPE_MISMATCH,
                    f"require {rname!r} must be boolean, got {t}",
                    decl.line,
                    decl.col,
                )
        for ename, eexpr in decl.ensures:
            _check_spec_purity(prog, eexpr)
            t = tc.type_of(eexpr, BOOL)
            if t != BOOL:
                prog.error(
                    D_TYPE_MISMATCH,
                    f"ensure {ename!r} must be boolean, got {t}",
                    decl.line,
                    decl.col,
                )
        # A transition has one explicit final commit; nested folds never commit.
        if (
            not decl.body
            or not isinstance(decl.body[-1], A.Commit)
            or any(isinstance(st, A.Commit) for st in decl.body[:-1])
        ):
            prog.error(
                D_TYPE_MISMATCH,
                "transition must end with exactly one commit",
                decl.line,
                decl.col,
            )
        if any(isinstance(st, A.Yield) for st in decl.body):
            prog.error(
                D_BAD_OPERAND,
                "yield is only valid as a fold result",
                decl.line,
                decl.col,
            )
        # body
        body_env = dict(env)
        body_tc = _TypeChecker(prog, body_env, info.def_kind)
        for stmt in decl.body:
            body_tc.check_stmt(stmt)
        info.ssa_types = dict(body_tc.env)
        # access extraction
        _extract_accesses(prog, info, declared_reads, declared_writes)
        # WCET / fold checks
        _check_wcet(prog, info)
        prog.transitions[decl.name] = info


def _check_access_paths(
    prog: Program,
    paths: List[A.AccessPath],
    env: Dict[str, ScalarType],
    clause: str,
    decl: A.TransitionDecl,
) -> List[Tuple[str, str, Optional[A.Expr]]]:
    out: List[Tuple[str, str, Optional[A.Expr]]] = []
    tc = _TypeChecker(prog, env)
    for path in paths:
        tbl = prog.table(path.table)
        if tbl is None:
            prog.error(
                D_UNKNOWN_TABLE,
                f"{clause} clause refers to unknown table {path.table!r}",
                path.line,
                path.col,
            )
            continue
        if path.column not in tbl.columns:
            prog.error(
                D_UNKNOWN_COLUMN,
                f"{clause} clause refers to unknown column "
                f"{path.table}.{path.column!r}",
                path.line,
                path.col,
            )
            continue
        if path.key is not None:
            kt = tc.type_of(path.key)
            if kt != tbl.key_type:
                prog.error(
                    D_TYPE_MISMATCH,
                    f"{clause} key for {path.table} must be {tbl.key_type}, got {kt}",
                    path.line,
                    path.col,
                )
        out.append((path.table, path.column, path.key))
    return out


def _expr_key(expr: Optional[A.Expr]) -> Optional[str]:
    """Canonical structural key for access-footprint matching."""
    if expr is None:
        return None
    if isinstance(expr, A.Var):
        return f"var:{expr.name}"
    if isinstance(expr, A.Literal):
        return f"lit:{expr.value!r}"
    if isinstance(expr, A.Unary):
        return f"un:{expr.op}({_expr_key(expr.operand)})"
    if isinstance(expr, A.Binary):
        return f"bin:{expr.op}({_expr_key(expr.left)},{_expr_key(expr.right)})"
    if isinstance(expr, A.TableGet):
        return f"get:{expr.table},{_expr_key(expr.key)},{expr.column}"
    if isinstance(expr, A.KeyOf):
        return f"key:{expr.table},{_expr_key(expr.expr)}"
    if isinstance(expr, A.Select):
        return f"sel:{_expr_key(expr.cond)},{_expr_key(expr.a)},{_expr_key(expr.b)}"
    return f"expr:{type(expr).__name__}"


def _expr_uses_read(expr: A.Expr, def_kind: Dict[str, str]) -> bool:
    if isinstance(expr, A.Var):
        return def_kind.get(expr.name) in ("read", "alloc")
    if isinstance(
        expr,
        (
            A.TableGet,
            A.TableAccess,
            A.TableLookup,
            A.TableExistsUnique,
            A.SumAgg,
            A.Old,
        ),
    ):
        return True
    if isinstance(expr, A.KeyOf):
        return _expr_uses_read(expr.expr, def_kind)
    if isinstance(expr, A.Unary):
        return _expr_uses_read(expr.operand, def_kind)
    if isinstance(expr, A.Binary):
        return _expr_uses_read(expr.left, def_kind) or _expr_uses_read(
            expr.right, def_kind
        )
    if isinstance(expr, A.Select):
        return (
            _expr_uses_read(expr.cond, def_kind)
            or _expr_uses_read(expr.a, def_kind)
            or _expr_uses_read(expr.b, def_kind)
        )
    if isinstance(expr, A.Fold):
        return any(
            _expr_uses_read(x, def_kind)
            for x in (expr.init, expr.lo, expr.hi, expr.yield_expr)
        ) or any(isinstance(st, (A.TableSet, A.TableAlloc)) for st in expr.body)
    return False


def _extract_accesses(
    prog: Program,
    info: TransitionInfo,
    declared_reads: List[Tuple[str, str, Optional[A.Expr]]],
    declared_writes: List[Tuple[str, str, Optional[A.Expr]]],
) -> None:
    decl = info.decl
    reads: List[AccessRecord] = []
    writes: List[AccessRecord] = []
    agg_writes: Set[Tuple[str, str]] = set()

    # Reads performed by require clauses and by the body.
    for _, rexpr in decl.requires:
        _collect_reads(prog, rexpr, info.def_kind, reads)
    _collect_stmt_reads(decl.body, info.def_kind, reads)
    _collect_stmt_writes(prog, decl.body, info.def_kind, writes, agg_writes)

    info.reads = reads
    info.writes = writes
    info.agg_writes = sorted(agg_writes)
    info.tier = 1 if all(r.tier == 1 for r in reads + writes) else 2

    # Verify every extracted access is covered by the declared footprint.
    def covered(
        records: List[Tuple[str, str, Optional[A.Expr]]],
        table: str,
        column: str,
        key: Optional[A.Expr],
    ) -> bool:
        k = _expr_key(key)
        for dt, dc, dk in records:
            if dt != table or dc != column:
                continue
            if dk is None:  # '*' wildcard
                return True
            if _expr_key(dk) == k:
                return True
        return False

    for rec in reads:
        if rec.column is None:
            # table-level existence probes read the occupancy bitmap, which is
            # implicit in every declared column access on that table.
            continue
        if not covered(declared_reads, rec.table, rec.column, rec.key):
            prog.error(
                D_ACCESS_VIOLATION,
                f"transition {decl.name!r} reads {rec.table}[...].{rec.column} "
                f"which is not declared in its reads clause",
                rec.line,
                details=[f"extracted key: {_expr_key(rec.key)}"],
            )
    for rec in writes:
        if not covered(declared_writes, rec.table, rec.column, rec.key):
            prog.error(
                D_ACCESS_VIOLATION,
                f"transition {decl.name!r} writes {rec.table}[...].{rec.column} "
                f"which is not declared in its writes clause",
                rec.line,
                details=[f"extracted key: {_expr_key(rec.key)}"],
            )
    # Declared-but-unused paths are dead declarations.
    for dt, dc, dk in declared_reads:
        if not any(r.table == dt and r.column == dc for r in reads):
            prog.warn(
                D_ACCESS_VIOLATION,
                f"transition {decl.name!r} declares read {dt}.{dc} but never reads it",
                decl.line,
                decl.col,
            )
    for dt, dc, dk in declared_writes:
        if not any(w.table == dt and w.column == dc for w in writes):
            prog.warn(
                D_ACCESS_VIOLATION,
                f"transition {decl.name!r} declares write {dt}.{dc} but never writes it",
                decl.line,
                decl.col,
            )


def _collect_reads(
    prog: Program, expr: A.Expr, def_kind: Dict[str, str], out: List[AccessRecord]
) -> None:
    if isinstance(expr, A.TableGet):
        tier = 2 if _expr_uses_read(expr.key, def_kind) else 1
        out.append(
            AccessRecord(expr.table, expr.column, expr.key, tier, expr.line, len(out))
        )
        _collect_reads(prog, expr.key, def_kind, out)
        return
    if isinstance(expr, A.TableAccess):
        if expr.key is not None:
            tier = 2 if _expr_uses_read(expr.key, def_kind) else 1
            out.append(
                AccessRecord(
                    expr.table, expr.column, expr.key, tier, expr.line, len(out)
                )
            )
            _collect_reads(prog, expr.key, def_kind, out)
        return
    if isinstance(expr, A.TableExists):
        tier = 2 if _expr_uses_read(expr.key, def_kind) else 1
        out.append(AccessRecord(expr.table, None, expr.key, tier, expr.line, len(out)))
        _collect_reads(prog, expr.key, def_kind, out)
        return
    if isinstance(expr, A.TableLookup):
        tier = 2 if _expr_uses_read(expr.key, def_kind) else 1
        out.append(
            AccessRecord(
                expr.table, expr.key_column, expr.key, tier, expr.line, len(out)
            )
        )
        _collect_reads(prog, expr.key, def_kind, out)
        return
    if isinstance(expr, A.TableExistsUnique):
        tier = 2 if _expr_uses_read(expr.key, def_kind) else 1
        out.append(
            AccessRecord(
                expr.table, expr.key_column, expr.key, tier, expr.line, len(out)
            )
        )
        _collect_reads(prog, expr.key, def_kind, out)
        return
    if isinstance(expr, A.Old):
        if expr.access.key is not None:
            out.append(
                AccessRecord(
                    expr.access.table,
                    expr.access.column,
                    expr.access.key,
                    2 if _expr_uses_read(expr.access.key, def_kind) else 1,
                    expr.line,
                    len(out),
                )
            )
            _collect_reads(prog, expr.access.key, def_kind, out)
        return
    if isinstance(expr, A.SumAgg):
        out.append(AccessRecord(expr.table, expr.column, None, 1, expr.line, len(out)))
        return
    if isinstance(expr, A.Var) or isinstance(expr, A.Literal):
        return
    if isinstance(expr, A.Unary):
        _collect_reads(prog, expr.operand, def_kind, out)
        return
    if isinstance(expr, A.Binary):
        _collect_reads(prog, expr.left, def_kind, out)
        _collect_reads(prog, expr.right, def_kind, out)
        return
    if isinstance(expr, A.Select):
        _collect_reads(prog, expr.cond, def_kind, out)
        _collect_reads(prog, expr.a, def_kind, out)
        _collect_reads(prog, expr.b, def_kind, out)
        return
    if isinstance(expr, A.KeyOf):
        _collect_reads(prog, expr.expr, def_kind, out)
        return
    if isinstance(expr, A.Fold):
        _collect_reads(prog, expr.init, def_kind, out)
        _collect_reads(prog, expr.lo, def_kind, out)
        _collect_reads(prog, expr.hi, def_kind, out)
        for stmt in expr.body:
            _collect_stmt_reads([stmt], def_kind, out)
        _collect_reads(prog, expr.yield_expr, def_kind, out)
        return


def _collect_stmt_reads(
    stmts: List[A.Stmt], def_kind: Dict[str, str], out: List[AccessRecord]
) -> None:
    for stmt in stmts:
        if isinstance(stmt, A.Let):
            _collect_reads(None, stmt.value, def_kind, out)  # type: ignore[arg-type]
        elif isinstance(stmt, A.Guard):
            _collect_reads(None, stmt.cond, def_kind, out)  # type: ignore[arg-type]
        elif isinstance(stmt, A.TableSet):
            _collect_reads(None, stmt.key, def_kind, out)  # type: ignore[arg-type]
            _collect_reads(None, stmt.value, def_kind, out)  # type: ignore[arg-type]
        elif isinstance(stmt, A.TableAlloc):
            for _, fexpr in stmt.fields:
                _collect_reads(None, fexpr, def_kind, out)  # type: ignore[arg-type]
        elif isinstance(stmt, A.Emit):
            for arg in stmt.args:
                _collect_reads(None, arg, def_kind, out)  # type: ignore[arg-type]
        elif isinstance(stmt, A.Yield):
            _collect_reads(None, stmt.value, def_kind, out)  # type: ignore[arg-type]


def _collect_stmt_writes(
    prog: Program,
    stmts: List[A.Stmt],
    def_kind: Dict[str, str],
    out: List[AccessRecord],
    agg_writes: Set[Tuple[str, str]],
) -> None:
    for stmt in stmts:
        if isinstance(stmt, A.TableSet):
            tier = 2 if _expr_uses_read(stmt.key, def_kind) else 1
            out.append(
                AccessRecord(
                    stmt.table, stmt.column, stmt.key, tier, stmt.line, len(out)
                )
            )
            tbl = prog.table(stmt.table)
            if (
                tbl is not None
                and stmt.column in tbl.columns
                and tbl.columns[stmt.column].tracked_sum
            ):
                agg_writes.add((stmt.table, stmt.column))
        elif isinstance(stmt, A.TableAlloc):
            tbl = prog.table(stmt.table)
            if tbl is not None:
                for cname in tbl.column_order:
                    out.append(
                        AccessRecord(stmt.table, cname, None, 2, stmt.line, len(out))
                    )
                    if tbl.columns[cname].tracked_sum:
                        agg_writes.add((stmt.table, cname))

        def nested(value):
            if isinstance(value, A.Fold):
                _collect_stmt_writes(prog, value.body, def_kind, out, agg_writes)
                for child in (value.init, value.lo, value.hi, value.yield_expr):
                    nested(child)
            elif is_dataclass(value):
                for f in fields(value):
                    nested(getattr(value, f.name))
            elif isinstance(value, (list, tuple)):
                for child in value:
                    nested(child)

        for f in fields(stmt):
            nested(getattr(stmt, f.name))


def _check_wcet(prog: Program, info: TransitionInfo) -> None:
    """Bound all folds, including ones embedded in other pure expressions."""
    max_depth, max_product = 0, 1

    def cost(value, depth=0, product=1):
        nonlocal max_depth, max_product
        if isinstance(value, A.Fold):
            next_depth, next_product = depth + 1, product * value.n
            max_depth = max(max_depth, next_depth)
            max_product = max(max_product, next_product)
            if value.n <= 0 or value.n > 1024:
                prog.error(
                    D_FOLD_BOUND,
                    f"fold_bounded<{value.n}> requires 1 <= N <= N_max=1024",
                    value.line,
                    value.col,
                )
            if next_depth > 2:
                prog.error(
                    D_FOLD_DEPTH, "fold nesting depth exceeds 2", value.line, value.col
                )
            if next_product > 1024:
                prog.error(
                    D_FOLD_BOUND,
                    "nested fold product exceeds 1024",
                    value.line,
                    value.col,
                )
            setup = sum(
                cost(child, next_depth, next_product)
                for child in (value.init, value.lo, value.hi)
            )
            body = sum(stmt_cost(stmt, next_depth, next_product) for stmt in value.body)
            body += 1 + cost(value.yield_expr, next_depth, next_product)
            return 1 + setup + max(0, value.n) * body
        if is_dataclass(value):
            return sum(
                cost(getattr(value, f.name), depth, product) for f in fields(value)
            )
        if isinstance(value, (list, tuple)):
            return sum(cost(child, depth, product) for child in value)
        return 0

    def stmt_cost(stmt, depth=0, product=1):
        if isinstance(stmt, A.Commit):
            return 0
        extra = sum(cost(getattr(stmt, f.name), depth, product) for f in fields(stmt))
        # A fold binding itself is the fold instruction, not an extra opcode.
        return (
            extra
            if isinstance(stmt, A.Let) and isinstance(stmt.value, A.Fold)
            else 1 + extra
        )

    info.wcet_bound = sum(stmt_cost(stmt) for stmt in info.decl.body)
    for _, expr in info.decl.requires + info.decl.ensures:
        cost(expr)
    info.fold_depth = max_depth
    info.max_fold_product = max_product
