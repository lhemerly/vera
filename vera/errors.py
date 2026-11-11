"""Diagnostics and error codes for VERA."""

from dataclasses import dataclass, field
from typing import List, Optional

SEV_ERROR = "error"
SEV_WARNING = "warning"

# --- VERA reserved error codes -------------------------------------------------
ERR_ARITH_OVERFLOW = "ERR_ARITH_OVERFLOW"
ERR_DIV_BY_ZERO = "ERR_DIV_BY_ZERO"
ERR_INDEX_SATURATED = "ERR_INDEX_SATURATED"
ERR_DUPLICATE_KEY = "ERR_DUPLICATE_KEY"
ERR_TABLE_FULL = "ERR_TABLE_FULL"
ERR_MISSING_ROW = "ERR_MISSING_ROW"
ERR_GENERATION_EXHAUSTED = "ERR_GENERATION_EXHAUSTED"
ERR_VACUOUS_IMPLEMENTATION = "ERR_VACUOUS_IMPLEMENTATION"
ERR_DEAD_TRAP = "ERR_DEAD_TRAP"

# --- Compiler diagnostics codes ------------------------------------------------
D_PARSE = "D_PARSE"
D_UNKNOWN_TABLE = "D_UNKNOWN_TABLE"
D_UNKNOWN_COLUMN = "D_UNKNOWN_COLUMN"
D_UNKNOWN_TRANSITION = "D_UNKNOWN_TRANSITION"
D_UNKNOWN_EFFECT = "D_UNKNOWN_EFFECT"
D_UNKNOWN_PARAM = "D_UNKNOWN_PARAM"
D_TYPE_MISMATCH = "D_TYPE_MISMATCH"
D_BAD_OPERAND = "D_BAD_OPERAND"
D_BAD_KEY = "D_BAD_KEY"
D_ACCESS_VIOLATION = "D_ACCESS_VIOLATION"
D_WCET_OVERFLOW = "D_WCET_OVERFLOW"
D_FOLD_DEPTH = "D_FOLD_DEPTH"
D_FOLD_BOUND = "D_FOLD_BOUND"
D_CAPACITY = "D_CAPACITY"
D_DUPLICATE = "D_DUPLICATE"
D_UNINITIALIZED = "D_UNINITIALIZED"
D_UNREACHABLE = "D_UNREACHABLE"
D_LITERAL_RANGE = "D_LITERAL_RANGE"
D_ENUM_TAG = "D_ENUM_TAG"
D_PROJECTION = "D_PROJECTION"
D_QUANTIFIER = "D_QUANTIFIER"


@dataclass
class Diagnostic:
    severity: str
    code: str
    message: str
    line: int = 0
    col: int = 0
    details: List[str] = field(default_factory=list)

    def render(self) -> str:
        loc = f":{self.line}:{self.col}" if self.line else ""
        out = f"[{self.severity.upper()}] {self.code}{loc}: {self.message}"
        for d in self.details:
            out += f"\n    {d}"
        return out


class VeraError(Exception):
    """A fatal compilation/verification error carrying a Diagnostic."""

    def __init__(
        self,
        code: str,
        message: str,
        line: int = 0,
        col: int = 0,
        details: Optional[List[str]] = None,
    ):
        super().__init__(message)
        self.diag = Diagnostic(SEV_ERROR, code, message, line, col, details or [])


class DiagnosticBag:
    """Collects diagnostics without aborting at the first problem."""

    def __init__(self):
        self.items: List[Diagnostic] = []

    def add(
        self,
        severity: str,
        code: str,
        message: str,
        line: int = 0,
        col: int = 0,
        details: Optional[List[str]] = None,
    ) -> None:
        self.items.append(Diagnostic(severity, code, message, line, col, details or []))

    def error(
        self,
        code: str,
        message: str,
        line: int = 0,
        col: int = 0,
        details: Optional[List[str]] = None,
    ) -> None:
        self.add(SEV_ERROR, code, message, line, col, details)

    def warning(
        self,
        code: str,
        message: str,
        line: int = 0,
        col: int = 0,
        details: Optional[List[str]] = None,
    ) -> None:
        self.add(SEV_WARNING, code, message, line, col, details)

    @property
    def errors(self) -> List[Diagnostic]:
        return [d for d in self.items if d.severity == SEV_ERROR]

    @property
    def warnings(self) -> List[Diagnostic]:
        return [d for d in self.items if d.severity == SEV_WARNING]

    def has_errors(self) -> bool:
        return any(d.severity == SEV_ERROR for d in self.items)

    def render(self) -> str:
        return "\n".join(d.render() for d in self.items)

    def __len__(self) -> int:
        return len(self.items)
