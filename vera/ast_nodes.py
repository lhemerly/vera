"""Abstract syntax tree for the VERA language (Section 3 grammar)."""

from dataclasses import dataclass, field
from typing import List, Optional, Tuple


# ---------------------------------------------------------------------------
# Module / declarations
# ---------------------------------------------------------------------------


@dataclass
class Module:
    name: Optional[str]
    decls: List["Decl"] = field(default_factory=list)


@dataclass
class TypeRef:
    """Syntactic type reference, resolved to a ScalarType during analysis."""

    name: str
    type_args: List["TypeRef"] = field(default_factory=list)
    enum_tags: List[str] = field(default_factory=list)
    line: int = 0
    col: int = 0


@dataclass
class ColumnDecl:
    name: str
    type: TypeRef
    tracked_sum: bool = False
    unique: bool = False
    line: int = 0
    col: int = 0


@dataclass
class TableDecl:
    name: str
    capacity: int
    key_name: str
    key_type: TypeRef
    columns: List[ColumnDecl] = field(default_factory=list)
    line: int = 0
    col: int = 0


@dataclass
class InvariantDecl:
    name: str
    quant_var: Optional[str]
    quant_table: Optional[str]
    expr: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class Param:
    name: str
    type: TypeRef
    line: int = 0
    col: int = 0


@dataclass
class TableSlice:
    table: str
    column: str
    row: Optional["Expr"] = None
    line: int = 0
    col: int = 0


@dataclass
class IntentGuard:
    transition: str
    condition: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class ProjectionDecl:
    name: str
    params: List[Param] = field(default_factory=list)
    allow_if: Optional["Expr"] = None
    expose: List[TableSlice] = field(default_factory=list)
    enable_intents: List[IntentGuard] = field(default_factory=list)
    line: int = 0
    col: int = 0


@dataclass
class EffectDecl:
    name: str
    params: List[Param] = field(default_factory=list)
    line: int = 0
    col: int = 0


@dataclass
class AccessPath:
    table: str
    key: Optional["Expr"]  # None means '*'
    column: str
    line: int = 0
    col: int = 0


@dataclass
class TransitionDecl:
    name: str
    params: List[Param] = field(default_factory=list)
    reads: List[AccessPath] = field(default_factory=list)
    writes: List[AccessPath] = field(default_factory=list)
    requires: List[Tuple[str, "Expr"]] = field(default_factory=list)
    ensures: List[Tuple[str, "Expr"]] = field(default_factory=list)
    body: List["Stmt"] = field(default_factory=list)
    line: int = 0
    col: int = 0


Decl = object  # union alias for readability


# ---------------------------------------------------------------------------
# Expressions (spec-layer logic and canonical synthesis-plane pure exprs)
# ---------------------------------------------------------------------------


@dataclass
class Var:
    name: str
    line: int = 0
    col: int = 0


@dataclass
class Literal:
    value: object
    type_ref: Optional[TypeRef] = None
    line: int = 0
    col: int = 0


@dataclass
class Unary:
    op: str
    operand: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class Binary:
    op: str
    left: "Expr"
    right: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class Select:
    cond: "Expr"
    a: "Expr"
    b: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class TableGet:
    table: str
    key: "Expr"
    column: str
    line: int = 0
    col: int = 0


@dataclass
class TableExists:
    table: str
    key: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class TableExistsUnique:
    """Existence probe for a declared unique secondary key."""

    table: str
    key_column: str
    key: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class TableLookup:
    """Look up a row by a declared unique secondary key.

    Returns ``id64<T>`` and reverts with ``ERR_MISSING_ROW`` when absent.
    """

    table: str
    key_column: str
    key: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class TableAccess:
    """`Table[key].column` in the human specification layer."""

    table: str
    key: "Expr"
    column: str
    line: int = 0
    col: int = 0


@dataclass
class Old:
    access: AccessPath
    line: int = 0
    col: int = 0


@dataclass
class SumAgg:
    table: str
    column: str
    old: bool = False
    line: int = 0
    col: int = 0


@dataclass
class KeyOf:
    """Explicit `id64<T>(expr)` key constructor (canonical, non-implicit)."""

    table: str
    expr: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class Fold:
    n: int
    acc_name: str
    acc_type: TypeRef
    init: "Expr"
    index_name: str
    lo: "Expr"
    hi: "Expr"
    body: List["Stmt"] = field(default_factory=list)
    yield_expr: "Expr" = field(default=None)
    line: int = 0
    col: int = 0


Expr = object  # union alias


# ---------------------------------------------------------------------------
# Statements (canonical SSA synthesis IR)
# ---------------------------------------------------------------------------


@dataclass
class Let:
    name: str
    type: TypeRef
    value: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class Guard:
    cond: "Expr"
    code: str
    line: int = 0
    col: int = 0


@dataclass
class TableSet:
    table: str
    key: "Expr"
    column: str
    value: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class TableAlloc:
    name: str
    table: str
    fields: List[Tuple[str, "Expr"]] = field(default_factory=list)
    line: int = 0
    col: int = 0


@dataclass
class Emit:
    effect: str
    args: List["Expr"] = field(default_factory=list)
    line: int = 0
    col: int = 0


@dataclass
class Yield:
    value: "Expr"
    line: int = 0
    col: int = 0


@dataclass
class Commit:
    line: int = 0
    col: int = 0


Stmt = object  # union alias
