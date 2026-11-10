"""Recursive-descent parser for the VERA grammar (Section 3).

The synthesis plane is parsed with prefix-canonical operators only; the human
specification layer additionally accepts infix sugar which is normalized to the
same canonical AST (Law 4: one internal representation per operation).
"""

from typing import List, Optional, Tuple

from . import ast_nodes as A
from .errors import D_PARSE, VeraError
from .lexer import Lexer, Token, tokenize

_BINARY_PRECEDENCE = [
    ("or", "||"),
    ("and", "&&"),
]
_COMPARISON_OPS = {"==": "eq", "!=": "ne", "<": "lt", "<=": "le", ">": "gt", ">=": "ge"}
_ADDITIVE_OPS = {"+": "add", "-": "sub"}
_MULTIPLICATIVE_OPS = {"*": "mul", "/": "div", "%": "rem"}
_PREFIX_UNARY = {"!": "not", "-": "neg"}
_CANONICAL_BINARY = {
    "add", "sub", "mul", "div", "rem", "eq", "ne", "lt", "le", "gt", "ge",
    "and", "or", "xor", "min", "max",
}
_CANONICAL_UNARY = {"not", "neg"}


class Parser:
    def __init__(self, tokens: List[Token]):
        self.tokens = tokens
        self.pos = 0

    # --- token helpers -----------------------------------------------------
    def peek(self, offset: int = 0) -> Token:
        idx = self.pos + offset
        if idx >= len(self.tokens):
            return self.tokens[-1]
        return self.tokens[idx]

    def advance(self) -> Token:
        tok = self.peek()
        if self.pos < len(self.tokens) - 1:
            self.pos += 1
        return tok

    def at_end(self) -> bool:
        return self.peek().kind == "eof"

    def error(self, message: str, tok: Optional[Token] = None) -> None:
        tok = tok or self.peek()
        raise VeraError(D_PARSE, message, tok.line, tok.col)

    def expect_punct(self, value: str) -> Token:
        tok = self.peek()
        if tok.kind != "punct" or tok.value != value:
            self.error(f"expected {value!r} but found {tok.text!r}", tok)
        return self.advance()

    def expect_keyword(self, value: str) -> Token:
        tok = self.peek()
        if tok.kind != "keyword" or tok.value != value:
            self.error(f"expected keyword {value!r} but found {tok.text!r}", tok)
        return self.advance()

    def accept_punct(self, value: str) -> Optional[Token]:
        tok = self.peek()
        if tok.kind == "punct" and tok.value == value:
            return self.advance()
        return None

    def accept_keyword(self, value: str) -> Optional[Token]:
        tok = self.peek()
        if tok.kind == "keyword" and tok.value == value:
            return self.advance()
        return None

    def expect_ident(self) -> Token:
        tok = self.peek()
        if tok.kind != "ident":
            self.error(f"expected identifier but found {tok.text!r}", tok)
        return self.advance()

    def expect_ssa(self) -> Token:
        tok = self.peek()
        if tok.kind != "ssa":
            self.error(f"expected SSA identifier (%name) but found {tok.text!r}", tok)
        return self.advance()

    def expect_int(self) -> Tuple[int, Token]:
        tok = self.peek()
        if tok.kind != "int":
            self.error(f"expected integer literal but found {tok.text!r}", tok)
        self.advance()
        return int(tok.value), tok

    # --- entry point -------------------------------------------------------
    def parse_module(self) -> A.Module:
        name = None
        if self.accept_keyword("module"):
            name = self.expect_ident().value
            self.expect_punct(";")
        decls: List[A.Decl] = []
        while not self.at_end():
            decls.append(self.parse_decl())
        return A.Module(name=name, decls=decls)

    def parse_decl(self) -> A.Decl:
        tok = self.peek()
        if tok.kind != "keyword":
            self.error(f"expected declaration keyword but found {tok.text!r}", tok)
        if tok.value == "table":
            return self.parse_table()
        if tok.value == "invariant":
            return self.parse_invariant()
        if tok.value == "projection":
            return self.parse_projection()
        if tok.value == "effect":
            return self.parse_effect()
        if tok.value == "transition":
            return self.parse_transition()
        self.error(f"unexpected declaration keyword {tok.value!r}", tok)

    # --- declaration parsers ----------------------------------------------
    def parse_type_ref(self) -> A.TypeRef:
        tok = self.peek()
        if tok.kind != "keyword" and tok.kind != "ident":
            self.error(f"expected type but found {tok.text!r}", tok)
        if tok.kind == "ident":
            self.error(f"unknown type {tok.value!r}", tok)
        name = tok.value
        line, col = tok.line, tok.col
        if name in ("bool", "u8", "u16", "u32", "u64", "i64", "dec64",
                    "dec128", "sym32", "bytes32"):
            self.advance()
            return A.TypeRef(name, line=line, col=col)
        if name == "id64":
            self.advance()
            self.expect_punct("<")
            arg = self.expect_ident()
            self.expect_punct(">")
            return A.TypeRef("id64", type_args=[A.TypeRef(arg.value, line=arg.line, col=arg.col)],
                             line=line, col=col)
        if name == "enum":
            self.advance()
            self.expect_punct("{")
            tags: List[str] = []
            while True:
                tag = self.expect_ident()
                tags.append(tag.value)
                if not self.accept_punct(","):
                    break
            self.expect_punct("}")
            return A.TypeRef("enum", enum_tags=tags, line=line, col=col)
        self.error(f"unknown type {name!r}", tok)

    def parse_key_type(self) -> A.TypeRef:
        tok = self.expect_keyword("id64")
        self.expect_punct("<")
        arg = self.expect_ident()
        self.expect_punct(">")
        return A.TypeRef("id64", type_args=[A.TypeRef(arg.value, line=arg.line, col=arg.col)],
                         line=tok.line, col=tok.col)

    def parse_table(self) -> A.TableDecl:
        tok = self.expect_keyword("table")
        name = self.expect_ident().value
        self.expect_keyword("capacity")
        capacity, _ = self.expect_int()
        self.expect_punct("{")
        self.expect_keyword("key")
        key_name = self.expect_ident().value
        self.expect_punct(":")
        key_type = self.parse_key_type()
        self.expect_punct(";")
        columns: List[A.ColumnDecl] = []
        while not self.accept_punct("}"):
            if self.peek().kind == "eof":
                self.error("unterminated table declaration")
            self.expect_keyword("col")
            col_tok = self.expect_ident()
            self.expect_punct(":")
            ctype = self.parse_type_ref()
            tracked = False
            if self.accept_punct("["):
                self.expect_keyword("tracked_sum")
                self.expect_punct("]")
                tracked = True
            self.expect_punct(";")
            columns.append(A.ColumnDecl(col_tok.value, ctype, tracked,
                                        col_tok.line, col_tok.col))
        return A.TableDecl(name, capacity, key_name, key_type, columns,
                           tok.line, tok.col)

    def parse_invariant(self) -> A.InvariantDecl:
        tok = self.expect_keyword("invariant")
        name = self.expect_ident().value
        self.expect_punct("{")
        quant_var = None
        quant_table = None
        if self.accept_keyword("forall"):
            quant_var = self.expect_ident().value
            self.expect_keyword("in")
            quant_table = self.expect_ident().value
            self.expect_punct(":")
        expr = self.parse_logic_expr()
        self.expect_punct(";")
        self.expect_punct("}")
        return A.InvariantDecl(name, quant_var, quant_table, expr, tok.line, tok.col)

    def parse_param_list(self) -> List[A.Param]:
        params: List[A.Param] = []
        self.expect_punct("(")
        if self.accept_punct(")"):
            return params
        while True:
            name_tok = self.expect_ident()
            self.expect_punct(":")
            ptype = self.parse_type_ref()
            params.append(A.Param(name_tok.value, ptype, name_tok.line, name_tok.col))
            if not self.accept_punct(","):
                break
        self.expect_punct(")")
        return params

    def parse_table_slice(self) -> A.TableSlice:
        tok = self.expect_ident()
        self.expect_punct(".")
        col = self.expect_ident()
        row = None
        if self.accept_punct("["):
            if self.accept_punct("*"):
                row = None
            else:
                row = self.parse_logic_expr()
            self.expect_punct("]")
        return A.TableSlice(tok.value, col.value, row, tok.line, tok.col)

    def parse_projection(self) -> A.ProjectionDecl:
        tok = self.expect_keyword("projection")
        name = self.expect_ident().value
        params = self.parse_param_list()
        self.expect_punct("{")
        self.expect_keyword("allow_if")
        self.expect_punct(":")
        allow_if = self.parse_logic_expr()
        self.expect_punct(";")
        self.expect_keyword("expose")
        self.expect_punct(":")
        self.expect_punct("[")
        expose: List[A.TableSlice] = []
        if not self.accept_punct("]"):
            while True:
                expose.append(self.parse_table_slice())
                if not self.accept_punct(","):
                    break
            self.expect_punct("]")
        self.expect_punct(";")
        self.expect_keyword("enable_intents")
        self.expect_punct(":")
        self.expect_punct("[")
        intents: List[A.IntentGuard] = []
        if not self.accept_punct("]"):
            while True:
                itok = self.expect_ident()
                self.expect_keyword("when")
                self.expect_punct("(")
                cond = self.parse_logic_expr()
                self.expect_punct(")")
                intents.append(A.IntentGuard(itok.value, cond, itok.line, itok.col))
                if not self.accept_punct(","):
                    break
            self.expect_punct("]")
        self.expect_punct(";")
        self.expect_punct("}")
        return A.ProjectionDecl(name, params, allow_if, expose, intents,
                                tok.line, tok.col)

    def parse_effect(self) -> A.EffectDecl:
        tok = self.expect_keyword("effect")
        name = self.expect_ident().value
        params = self.parse_param_list()
        self.expect_punct(";")
        return A.EffectDecl(name, params, tok.line, tok.col)

    def parse_access_path(self) -> A.AccessPath:
        tok = self.expect_ident()
        self.expect_punct("[")
        if self.accept_punct("*"):
            key = None
        else:
            key = self.parse_logic_expr()
        self.expect_punct("]")
        self.expect_punct(".")
        col = self.expect_ident().value
        return A.AccessPath(tok.value, key, col, tok.line, tok.col)

    def parse_transition(self) -> A.TransitionDecl:
        tok = self.expect_keyword("transition")
        name = self.expect_ident().value
        params = self.parse_param_list()
        self.expect_keyword("reads")
        self.expect_punct("[")
        reads: List[A.AccessPath] = []
        if not self.accept_punct("]"):
            while True:
                reads.append(self.parse_access_path())
                if not self.accept_punct(","):
                    break
            self.expect_punct("]")
        self.expect_keyword("writes")
        self.expect_punct("[")
        writes: List[A.AccessPath] = []
        if not self.accept_punct("]"):
            while True:
                writes.append(self.parse_access_path())
                if not self.accept_punct(","):
                    break
            self.expect_punct("]")
        requires: List[Tuple[str, A.Expr]] = []
        ensures: List[Tuple[str, A.Expr]] = []
        while self.peek().kind == "keyword" and self.peek().value == "require":
            self.advance()
            rname = self.expect_ident().value
            self.expect_punct(":")
            requires.append((rname, self.parse_logic_expr()))
            self.expect_punct(";")
        while self.peek().kind == "keyword" and self.peek().value == "ensure":
            self.advance()
            ename = self.expect_ident().value
            self.expect_punct(":")
            ensures.append((ename, self.parse_post_logic_expr()))
            self.expect_punct(";")
        self.expect_keyword("body")
        self.expect_punct("{")
        body = self.parse_stmt_list(stop_keyword="commit")
        self.expect_keyword("commit")
        self.expect_punct(";")
        self.expect_punct("}")
        return A.TransitionDecl(name, params, reads, writes, requires, ensures,
                                body, tok.line, tok.col)

    # --- statements --------------------------------------------------------
    def parse_stmt_list(self, stop_keyword: Optional[str] = None) -> List[A.Stmt]:
        stmts: List[A.Stmt] = []
        while True:
            tok = self.peek()
            if tok.kind == "eof":
                self.error("unexpected end of file in statement list")
            if tok.kind == "punct" and tok.value == "}":
                break
            if stop_keyword is not None and tok.kind == "keyword" and tok.value == stop_keyword:
                break
            stmts.append(self.parse_stmt())
        return stmts

    def parse_stmt(self) -> A.Stmt:
        tok = self.peek()
        if tok.kind != "keyword":
            self.error(f"expected statement keyword but found {tok.text!r}", tok)
        if tok.value == "let":
            return self.parse_let()
        if tok.value == "guard":
            return self.parse_guard()
        if tok.value == "table_set":
            return self.parse_table_set()
        if tok.value == "emit":
            return self.parse_emit()
        if tok.value == "yield":
            self.advance()
            value = self.parse_pure_expr()
            self.expect_punct(";")
            return A.Yield(value, tok.line, tok.col)
        self.error(f"unexpected statement keyword {tok.value!r}", tok)

    def parse_let(self) -> A.Stmt:
        tok = self.expect_keyword("let")
        name = self.expect_ssa().value
        self.expect_punct(":")
        tref = self.parse_type_ref()
        self.expect_punct("=")
        if self.peek().kind == "keyword" and self.peek().value == "table_alloc":
            self.advance()
            self.expect_punct("(")
            table = self.expect_ident().value
            self.expect_punct(",")
            self.expect_punct("{")
            fields: List[Tuple[str, A.Expr]] = []
            if not self.accept_punct("}"):
                while True:
                    fname = self.expect_ident().value
                    self.expect_punct(":")
                    fields.append((fname, self.parse_pure_expr()))
                    if not self.accept_punct(","):
                        break
                self.expect_punct("}")
            self.expect_punct(")")
            self.expect_punct(";")
            return A.TableAlloc(name, table, fields, tok.line, tok.col)
        value = self.parse_pure_expr()
        self.expect_punct(";")
        return A.Let(name, tref, value, tok.line, tok.col)

    def parse_guard(self) -> A.Guard:
        tok = self.expect_keyword("guard")
        self.expect_punct("(")
        cond = self.parse_pure_expr()
        self.expect_punct(",")
        code = self.expect_ident().value
        self.expect_punct(")")
        self.expect_punct(";")
        return A.Guard(cond, code, tok.line, tok.col)

    def parse_table_set(self) -> A.TableSet:
        tok = self.expect_keyword("table_set")
        self.expect_punct("(")
        table = self.expect_ident().value
        self.expect_punct(",")
        key = self.parse_pure_expr()
        self.expect_punct(",")
        column = self.expect_ident().value
        self.expect_punct(",")
        value = self.parse_pure_expr()
        self.expect_punct(")")
        self.expect_punct(";")
        return A.TableSet(table, key, column, value, tok.line, tok.col)

    def parse_emit(self) -> A.Emit:
        tok = self.expect_keyword("emit")
        name = self.expect_ident().value
        self.expect_punct("(")
        args: List[A.Expr] = []
        if not self.accept_punct(")"):
            while True:
                args.append(self.parse_pure_expr())
                if not self.accept_punct(","):
                    break
            self.expect_punct(")")
        self.expect_punct(";")
        return A.Emit(name, args, tok.line, tok.col)

    # --- pure expressions (canonical prefix IR) ----------------------------
    def parse_pure_expr(self) -> A.Expr:
        tok = self.peek()
        if tok.kind == "int":
            self.advance()
            return A.Literal(int(tok.value), line=tok.line, col=tok.col)
        if tok.kind == "decimal":
            self.advance()
            return A.Literal(float(tok.value), line=tok.line, col=tok.col)
        if tok.kind == "string":
            self.advance()
            return A.Literal(str(tok.value), line=tok.line, col=tok.col)
        if tok.kind == "ssa":
            self.advance()
            return A.Var(tok.value, tok.line, tok.col)
        if tok.kind == "ident":
            self.advance()
            return A.Var(tok.value, tok.line, tok.col)
        if tok.kind == "punct" and tok.value == "(":
            self.advance()
            expr = self.parse_pure_expr()
            self.expect_punct(")")
            return expr
        if tok.kind == "keyword":
            kw = tok.value
            if kw == "true":
                self.advance()
                return A.Literal(True, line=tok.line, col=tok.col)
            if kw == "false":
                self.advance()
                return A.Literal(False, line=tok.line, col=tok.col)
            if kw in _CANONICAL_UNARY:
                self.advance()
                self.expect_punct("(")
                operand = self.parse_pure_expr()
                self.expect_punct(")")
                return A.Unary(kw, operand, tok.line, tok.col)
            if kw in _CANONICAL_BINARY:
                self.advance()
                self.expect_punct("(")
                left = self.parse_pure_expr()
                self.expect_punct(",")
                right = self.parse_pure_expr()
                self.expect_punct(")")
                return A.Binary(kw, left, right, tok.line, tok.col)
            if kw == "select":
                self.advance()
                self.expect_punct("(")
                cond = self.parse_pure_expr()
                self.expect_punct(",")
                a = self.parse_pure_expr()
                self.expect_punct(",")
                b = self.parse_pure_expr()
                self.expect_punct(")")
                return A.Select(cond, a, b, tok.line, tok.col)
            if kw == "table_get":
                self.advance()
                self.expect_punct("(")
                table = self.expect_ident().value
                self.expect_punct(",")
                key = self.parse_pure_expr()
                self.expect_punct(",")
                column = self.expect_ident().value
                self.expect_punct(")")
                return A.TableGet(table, key, column, tok.line, tok.col)
            if kw == "table_exists":
                self.advance()
                self.expect_punct("(")
                table = self.expect_ident().value
                self.expect_punct(",")
                key = self.parse_pure_expr()
                self.expect_punct(")")
                return A.TableExists(table, key, tok.line, tok.col)
            if kw == "key_of":
                self.advance()
                self.expect_punct("<")
                table = self.expect_ident().value
                self.expect_punct(">")
                self.expect_punct("(")
                expr = self.parse_pure_expr()
                self.expect_punct(")")
                return A.KeyOf(table, expr, tok.line, tok.col)
            if kw == "fold_bounded":
                return self.parse_fold(tok)
        self.error(f"expected expression but found {tok.text!r}", tok)

    def parse_fold(self, tok: Token) -> A.Fold:
        self.expect_keyword("fold_bounded")
        self.expect_punct("<")
        n, _ = self.expect_int()
        self.expect_punct(">")
        self.expect_punct("(")
        acc_name = self.advance().value
        self.expect_punct(":")
        acc_type = self.parse_type_ref()
        self.expect_punct("=")
        init = self.parse_pure_expr()
        self.expect_punct(",")
        index_name = self.advance().value
        self.expect_keyword("in")
        self.expect_punct("[")
        lo = self.parse_pure_expr()
        self.expect_punct("..")
        hi = self.parse_pure_expr()
        self.expect_punct("]")
        self.expect_punct(")")
        self.expect_punct("{")
        body = self.parse_stmt_list(stop_keyword="yield")
        self.expect_keyword("yield")
        yield_expr = self.parse_pure_expr()
        self.expect_punct(";")
        self.expect_punct("}")
        return A.Fold(n, acc_name, acc_type, init, index_name, lo, hi, body,
                      yield_expr, tok.line, tok.col)

    # --- logic expressions (human spec layer) ------------------------------
    def parse_post_logic_expr(self) -> A.Expr:
        tok = self.peek()
        if tok.kind == "keyword" and tok.value == "old":
            self.advance()
            self.expect_punct("(")
            access = self.parse_access_path()
            self.expect_punct(")")
            return A.Old(access, tok.line, tok.col)
        if tok.kind == "keyword" and tok.value in ("sum", "old_sum"):
            self.advance()
            self.expect_punct("(")
            table = self.expect_ident().value
            self.expect_punct(".")
            column = self.expect_ident().value
            self.expect_punct(")")
            return A.SumAgg(table, column, old=(tok.value == "old_sum"),
                            line=tok.line, col=tok.col)
        return self.parse_logic_expr()

    def parse_logic_expr(self) -> A.Expr:
        return self.parse_logic_or()

    def parse_logic_or(self) -> A.Expr:
        left = self.parse_logic_and()
        while True:
            tok = self.peek()
            if (tok.kind == "keyword" and tok.value == "or") or \
               (tok.kind == "punct" and tok.value == "||"):
                self.advance()
                right = self.parse_logic_and()
                left = A.Binary("or", left, right, tok.line, tok.col)
            else:
                return left

    def parse_logic_and(self) -> A.Expr:
        left = self.parse_logic_not()
        while True:
            tok = self.peek()
            if (tok.kind == "keyword" and tok.value == "and") or \
               (tok.kind == "punct" and tok.value == "&&"):
                self.advance()
                right = self.parse_logic_not()
                left = A.Binary("and", left, right, tok.line, tok.col)
            else:
                return left

    def parse_logic_not(self) -> A.Expr:
        tok = self.peek()
        if (tok.kind == "keyword" and tok.value == "not") or \
           (tok.kind == "punct" and tok.value == "!"):
            self.advance()
            operand = self.parse_logic_not()
            return A.Unary("not", operand, tok.line, tok.col)
        return self.parse_logic_comparison()

    def parse_logic_comparison(self) -> A.Expr:
        left = self.parse_logic_additive()
        tok = self.peek()
        if tok.kind == "punct" and tok.value in _COMPARISON_OPS:
            self.advance()
            right = self.parse_logic_additive()
            return A.Binary(_COMPARISON_OPS[tok.value], left, right, tok.line, tok.col)
        return left

    def parse_logic_additive(self) -> A.Expr:
        left = self.parse_logic_multiplicative()
        while True:
            tok = self.peek()
            if tok.kind == "punct" and tok.value in _ADDITIVE_OPS:
                self.advance()
                right = self.parse_logic_multiplicative()
                left = A.Binary(_ADDITIVE_OPS[tok.value], left, right, tok.line, tok.col)
            else:
                return left

    def parse_logic_multiplicative(self) -> A.Expr:
        left = self.parse_logic_unary()
        while True:
            tok = self.peek()
            if tok.kind == "punct" and tok.value in _MULTIPLICATIVE_OPS:
                self.advance()
                right = self.parse_logic_unary()
                left = A.Binary(_MULTIPLICATIVE_OPS[tok.value], left, right, tok.line, tok.col)
            else:
                return left

    def parse_logic_unary(self) -> A.Expr:
        tok = self.peek()
        if tok.kind == "punct" and tok.value in _PREFIX_UNARY:
            self.advance()
            operand = self.parse_logic_unary()
            return A.Unary(_PREFIX_UNARY[tok.value], operand, tok.line, tok.col)
        if tok.kind == "keyword" and tok.value in _CANONICAL_UNARY:
            self.advance()
            self.expect_punct("(")
            operand = self.parse_pure_expr()
            self.expect_punct(")")
            return A.Unary(tok.value, operand, tok.line, tok.col)
        return self.parse_logic_atom()

    def parse_logic_atom(self) -> A.Expr:
        tok = self.peek()
        if tok.kind == "punct" and tok.value == "(":
            self.advance()
            expr = self.parse_logic_expr()
            self.expect_punct(")")
            return expr
        if tok.kind in ("int", "decimal", "string"):
            return self.parse_pure_expr()
        if tok.kind == "ssa" or tok.kind == "ident":
            # Could be Table[key].col, Table.col[key], Table.col, or a plain var.
            name = self.advance().value
            if self.accept_punct("["):
                key = self.parse_logic_expr()
                self.expect_punct("]")
                self.expect_punct(".")
                column = self.expect_ident().value
                return A.TableAccess(name, key, column, tok.line, tok.col)
            if self.accept_punct("."):
                column = self.expect_ident().value
                if self.accept_punct("["):
                    if self.accept_punct("*"):
                        return A.TableAccess(name, None, column, tok.line, tok.col)
                    row = self.parse_logic_expr()
                    self.expect_punct("]")
                    return A.TableAccess(name, row, column, tok.line, tok.col)
                return A.TableAccess(name, None, column, tok.line, tok.col)
            return A.Var(name, tok.line, tok.col)
        if tok.kind == "keyword":
            kw = tok.value
            if kw == "true":
                self.advance()
                return A.Literal(True, line=tok.line, col=tok.col)
            if kw == "false":
                self.advance()
                return A.Literal(False, line=tok.line, col=tok.col)
            if kw in _CANONICAL_BINARY:
                self.advance()
                self.expect_punct("(")
                left = self.parse_logic_expr()
                self.expect_punct(",")
                right = self.parse_logic_expr()
                self.expect_punct(")")
                return A.Binary(kw, left, right, tok.line, tok.col)
            if kw == "select":
                self.advance()
                self.expect_punct("(")
                cond = self.parse_logic_expr()
                self.expect_punct(",")
                a = self.parse_logic_expr()
                self.expect_punct(",")
                b = self.parse_logic_expr()
                self.expect_punct(")")
                return A.Select(cond, a, b, tok.line, tok.col)
            if kw == "table_exists":
                self.advance()
                self.expect_punct("(")
                table = self.expect_ident().value
                self.expect_punct(",")
                key = self.parse_logic_expr()
                self.expect_punct(")")
                return A.TableExists(table, key, tok.line, tok.col)
            if kw == "table_get":
                self.advance()
                self.expect_punct("(")
                table = self.expect_ident().value
                self.expect_punct(",")
                key = self.parse_logic_expr()
                self.expect_punct(",")
                column = self.expect_ident().value
                self.expect_punct(")")
                return A.TableGet(table, key, column, tok.line, tok.col)
            if kw == "key_of":
                self.advance()
                self.expect_punct("<")
                table = self.expect_ident().value
                self.expect_punct(">")
                self.expect_punct("(")
                expr = self.parse_logic_expr()
                self.expect_punct(")")
                return A.KeyOf(table, expr, tok.line, tok.col)
            if kw == "fold_bounded":
                return self.parse_fold(tok)
            if kw == "old":
                self.advance()
                self.expect_punct("(")
                access = self.parse_access_path()
                self.expect_punct(")")
                return A.Old(access, tok.line, tok.col)
            if kw in ("sum", "old_sum"):
                self.advance()
                self.expect_punct("(")
                table = self.expect_ident().value
                self.expect_punct(".")
                column = self.expect_ident().value
                self.expect_punct(")")
                return A.SumAgg(table, column, old=(kw == "old_sum"),
                                line=tok.line, col=tok.col)
        self.error(f"expected logic expression but found {tok.text!r}", tok)


def parse_source(source: str) -> A.Module:
    return Parser(tokenize(source)).parse_module()


def parse_file(path: str) -> A.Module:
    with open(path, "r", encoding="utf-8") as fh:
        return parse_source(fh.read())
