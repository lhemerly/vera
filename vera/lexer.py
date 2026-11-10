"""Tokenizer for the VERA language."""

from dataclasses import dataclass
from typing import List, Optional, Tuple

from .errors import D_PARSE, VeraError

KEYWORDS = {
    # declaration structure
    "module", "table", "capacity", "key", "col", "tracked_sum", "invariant",
    "forall", "in", "projection", "allow_if", "expose", "enable_intents",
    "when", "effect", "transition", "reads", "writes", "require", "ensure",
    "body", "commit",
    # synthesis IR statements / rvalues
    "let", "guard", "table_set", "table_alloc", "table_get", "table_exists",
    "select", "fold_bounded", "yield", "emit", "key_of",
    # scalar types
    "bool", "u8", "u16", "u32", "u64", "i64", "dec64", "dec128", "sym32",
    "bytes32", "id64", "enum",
    # literals
    "true", "false",
    # logic / arithmetic canonical operators
    "and", "or", "not", "xor", "add", "sub", "mul", "div", "rem",
    "eq", "ne", "lt", "le", "gt", "ge", "min", "max",
    # postcondition accessors
    "old", "sum", "old_sum",
}

PUNCTUATION = [
    "..", "==", "!=", "<=", ">=", "&&", "||", "->",
    "{", "}", "(", ")", "[", "]", ";", ":", ",", ".", "<", ">", "=",
    "+", "-", "*", "/", "%", "!",
]

IDENT_START = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_")
IDENT_CONT = IDENT_START | set("0123456789")
DIGITS = set("0123456789")
HEX_DIGITS = set("0123456789abcdefABCDEF")


@dataclass
class Token:
    kind: str          # ident | ssa | int | decimal | string | keyword | punct | eof
    value: object
    line: int
    col: int
    text: str = ""

    def __repr__(self) -> str:
        return f"Token({self.kind}, {self.value!r}, {self.line}:{self.col})"


def _is_ident_start(ch: str) -> bool:
    return ch in IDENT_START


def _is_ident_cont(ch: str) -> bool:
    return ch in IDENT_CONT


class Lexer:
    def __init__(self, source: str):
        self.src = source
        self.pos = 0
        self.line = 1
        self.col = 1
        self.tokens: List[Token] = []

    def error(self, message: str, line: Optional[int] = None,
              col: Optional[int] = None) -> None:
        raise VeraError(D_PARSE, message, line or self.line, col or self.col)

    def _peek(self, offset: int = 0) -> str:
        idx = self.pos + offset
        return self.src[idx] if idx < len(self.src) else ""

    def _advance(self, n: int = 1) -> str:
        out = self.src[self.pos:self.pos + n]
        for ch in out:
            if ch == "\n":
                self.line += 1
                self.col = 1
            else:
                self.col += 1
        self.pos += n
        return out

    def _skip_trivia(self) -> None:
        while self.pos < len(self.src):
            ch = self._peek()
            if ch in " \t\r\n":
                self._advance()
            elif ch == "/" and self._peek(1) == "/":
                while self.pos < len(self.src) and self._peek() != "\n":
                    self._advance()
            elif ch == "/" and self._peek(1) == "*":
                self._advance(2)
                while self.pos < len(self.src) and not (self._peek() == "*" and self._peek(1) == "/"):
                    self._advance()
                if self.pos >= len(self.src):
                    self.error("unterminated block comment")
                self._advance(2)
            elif ch == "#":
                # permit '#' line comments as a convenience
                while self.pos < len(self.src) and self._peek() != "\n":
                    self._advance()
            else:
                break

    def tokenize(self) -> List[Token]:
        while True:
            self._skip_trivia()
            if self.pos >= len(self.src):
                self.tokens.append(Token("eof", None, self.line, self.col, "<eof>"))
                return self.tokens
            line, col = self.line, self.col
            ch = self._peek()

            # SSA identifier: %name
            if ch == "%" and _is_ident_start(self._peek(1)):
                self._advance()
                start = self.pos
                while self.pos < len(self.src) and _is_ident_cont(self._peek()):
                    self._advance()
                name = "%" + self.src[start:self.pos]
                self.tokens.append(Token("ssa", name, line, col, name))
                continue

            # Identifier / keyword
            if _is_ident_start(ch):
                start = self.pos
                while self.pos < len(self.src) and _is_ident_cont(self._peek()):
                    self._advance()
                text = self.src[start:self.pos]
                kind = "keyword" if text in KEYWORDS else "ident"
                self.tokens.append(Token(kind, text, line, col, text))
                continue

            # Numeric literal: hex, decimal, or fixed-point decimal
            if ch in DIGITS or (ch == "." and self._peek(1) in DIGITS):
                token = self._lex_number(line, col)
                self.tokens.append(token)
                continue

            # String literal (ingress symbol literals; interned at compile time)
            if ch == '"':
                self._advance()
                buf = []
                while self.pos < len(self.src) and self._peek() != '"':
                    c = self._advance()
                    if c == "\\":
                        nxt = self._advance()
                        escapes = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\", "0": "\0"}
                        buf.append(escapes.get(nxt, nxt))
                    else:
                        buf.append(c)
                if self.pos >= len(self.src):
                    self.error("unterminated string literal", line, col)
                self._advance()  # closing quote
                self.tokens.append(Token("string", "".join(buf), line, col,
                                         '"' + "".join(buf) + '"'))
                continue

            # Punctuation
            matched = None
            for p in PUNCTUATION:
                if self.src.startswith(p, self.pos):
                    matched = p
                    break
            if matched is None:
                self.error(f"unexpected character {ch!r}", line, col)
            self._advance(len(matched))
            self.tokens.append(Token("punct", matched, line, col, matched))

    def _lex_number(self, line: int, col: int) -> Token:
        start = self.pos
        is_decimal = False
        if self._peek() == "0" and self._peek(1) in ("x", "X"):
            self._advance(2)
            digits_start = self.pos
            while self.pos < len(self.src) and self._peek() in HEX_DIGITS:
                self._advance()
            if self.pos == digits_start:
                self.error("empty hexadecimal literal", line, col)
            text = self.src[start:self.pos]
            return Token("int", int(text, 16), line, col, text)
        while self.pos < len(self.src) and (self._peek() in DIGITS or self._peek() == "_"):
            self._advance()
        if self._peek() == "." and self._peek(1) in DIGITS:
            is_decimal = True
            self._advance()
            while self.pos < len(self.src) and (self._peek() in DIGITS or self._peek() == "_"):
                self._advance()
        text = self.src[start:self.pos].replace("_", "")
        if is_decimal:
            return Token("decimal", float(text), line, col, text)
        return Token("int", int(text), line, col, text)


def tokenize(source: str) -> List[Token]:
    return Lexer(source).tokenize()
