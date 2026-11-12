//! Native recursive-descent parser for VERA v1.1 syntax.

use crate::ast::*;
use crate::diagnostic::{Diagnostic, Span};
use crate::lexer::{tokenize, Token, TokenKind};
use crate::literal::IntegerLiteral;

type Result<T> = std::result::Result<T, Diagnostic>;

pub fn parse_source(source: &str) -> Result<Module> {
    Parser {
        tokens: tokenize(source)?,
        position: 0,
        last_end: 0,
    }
    .module()
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    last_end: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        self.last_end = token.span.end;
        if self.position + 1 < self.tokens.len() {
            self.position += 1;
        }
        token
    }

    fn cover(&self, start: Span) -> Span {
        Span {
            end: self.last_end.max(start.end),
            ..start
        }
    }

    fn error(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::parse(message, self.peek().span)
    }

    fn punct(&self, wanted: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Punct(value) if value == wanted)
    }

    fn keyword(&self, wanted: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Keyword(value) if value == wanted)
    }

    fn accept_punct(&mut self, wanted: &str) -> bool {
        if self.punct(wanted) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn accept_keyword(&mut self, wanted: &str) -> bool {
        if self.keyword(wanted) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, wanted: &str) -> Result<Token> {
        // `id64<T>=...` and fold accumulator types need not contain whitespace.
        if wanted == ">" && self.punct(">=") {
            let token = self.peek().clone();
            let first = Span {
                end: token.span.start + 1,
                ..token.span
            };
            self.tokens[self.position] = Token {
                kind: TokenKind::Punct("=".into()),
                span: Span {
                    start: token.span.start + 1,
                    column: token.span.column + 1,
                    ..token.span
                },
                text: "=".into(),
            };
            self.last_end = first.end;
            return Ok(Token {
                kind: TokenKind::Punct(">".into()),
                span: first,
                text: ">".into(),
            });
        }
        if !self.punct(wanted) {
            return Err(self.error(format!(
                "expected {wanted:?} but found {:?}",
                self.peek().text
            )));
        }
        Ok(self.advance())
    }

    fn expect_keyword(&mut self, wanted: &str) -> Result<Token> {
        if !self.keyword(wanted) {
            return Err(self.error(format!(
                "expected keyword {wanted:?} but found {:?}",
                self.peek().text
            )));
        }
        Ok(self.advance())
    }

    fn ident(&mut self) -> Result<(String, Span)> {
        if let TokenKind::Ident(value) = self.peek().kind.clone() {
            let span = self.advance().span;
            Ok((value, span))
        } else {
            Err(self.error(format!(
                "expected identifier but found {:?}",
                self.peek().text
            )))
        }
    }

    fn ssa(&mut self) -> Result<String> {
        if let TokenKind::Ssa(value) = self.peek().kind.clone() {
            self.advance();
            Ok(value)
        } else {
            Err(self.error(format!(
                "expected SSA identifier (%name) but found {:?}",
                self.peek().text
            )))
        }
    }

    fn binding_name(&mut self, description: &str) -> Result<String> {
        match self.peek().kind.clone() {
            TokenKind::Ident(value) | TokenKind::Ssa(value) => {
                self.advance();
                Ok(value)
            }
            _ => Err(self.error(format!("fold {description} must be an identifier"))),
        }
    }

    fn integer(&mut self) -> Result<IntegerLiteral> {
        if let TokenKind::Integer(value) = self.peek().kind.clone() {
            self.advance();
            Ok(value)
        } else {
            Err(self.error(format!(
                "expected integer literal but found {:?}",
                self.peek().text
            )))
        }
    }

    fn module(&mut self) -> Result<Module> {
        let start = self.peek().span;
        let name = if self.accept_keyword("module") {
            let name = self.ident()?.0;
            self.expect_punct(";")?;
            Some(name)
        } else {
            None
        };
        let mut decls = Vec::new();
        while !matches!(self.peek().kind, TokenKind::Eof) {
            decls.push(self.declaration()?);
        }
        Ok(Module {
            name,
            decls,
            span: self.cover(start),
        })
    }

    fn declaration(&mut self) -> Result<Decl> {
        if matches!(&self.peek().kind,TokenKind::Ident(name) if name=="record" || name=="union") {
            return self.structured_declaration();
        }
        if matches!(&self.peek().kind, TokenKind::Ident(name) if name == "function") {
            return self.function().map(Decl::Function);
        }
        match &self.peek().kind {
            TokenKind::Keyword(name) => match name.as_str() {
                "table" => self.table().map(Decl::Table),
                "invariant" => self.invariant().map(Decl::Invariant),
                "projection" => self.projection().map(Decl::Projection),
                "effect" => self.effect().map(Decl::Effect),
                "transition" => self.transition().map(Decl::Transition),
                _ => Err(self.error(format!("unexpected declaration keyword {name:?}"))),
            },
            _ => Err(self.error(format!(
                "expected declaration keyword but found {:?}",
                self.peek().text
            ))),
        }
    }

    fn type_ref(&mut self) -> Result<TypeRef> {
        let token = self.peek().clone();
        let name = match &token.kind {
            TokenKind::Keyword(name) => name.clone(),
            TokenKind::Ident(name) => name.clone(),
            _ => return Err(self.error(format!("expected type but found {:?}", token.text))),
        };
        let mut type_args = Vec::new();
        let mut enum_tags = Vec::new();
        match name.as_str() {
            "bool" | "u8" | "u16" | "u32" | "u64" | "i64" | "dec64" | "dec128" | "sym32"
            | "bytes32" => {
                self.advance();
            }
            "id64" => {
                self.advance();
                self.expect_punct("<")?;
                let (table, span) = self.ident()?;
                type_args.push(TypeRef {
                    name: table,
                    type_args: Vec::new(),
                    enum_tags: Vec::new(),
                    span,
                });
                self.expect_punct(">")?;
            }
            "enum" => {
                self.advance();
                self.expect_punct("{")?;
                loop {
                    enum_tags.push(self.ident()?.0);
                    if !self.accept_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
            }
            _ if matches!(token.kind, TokenKind::Ident(_)) => {
                self.advance();
            }
            _ => return Err(self.error(format!("unknown type {name:?}"))),
        }
        Ok(TypeRef {
            name,
            type_args,
            enum_tags,
            span: self.cover(token.span),
        })
    }

    fn key_type(&mut self) -> Result<TypeRef> {
        if !self.keyword("id64") {
            return Err(self.error("expected keyword \"id64\""));
        }
        self.type_ref()
    }

    fn table(&mut self) -> Result<TableDecl> {
        let start = self.expect_keyword("table")?.span;
        let name = self.ident()?.0;
        self.expect_keyword("capacity")?;
        let capacity = self.integer()?;
        self.expect_punct("{")?;
        self.expect_keyword("key")?;
        let key_name = self.ident()?.0;
        self.expect_punct(":")?;
        let key_type = self.key_type()?;
        self.expect_punct(";")?;
        let mut columns = Vec::new();
        while !self.accept_punct("}") {
            if matches!(self.peek().kind, TokenKind::Eof) {
                return Err(self.error("unterminated table declaration"));
            }
            self.expect_keyword("col")?;
            let (name, span) = self.ident()?;
            self.expect_punct(":")?;
            let ty = self.type_ref()?;
            let (mut tracked_sum, mut unique) = (false, false);
            if self.accept_punct("[") {
                loop {
                    if self.accept_keyword("tracked_sum") {
                        tracked_sum = true;
                    } else if self.accept_keyword("unique") {
                        unique = true;
                    } else {
                        return Err(self.error("expected 'tracked_sum' or 'unique'"));
                    }
                    if !self.accept_punct(",") {
                        break;
                    }
                }
                self.expect_punct("]")?;
            }
            self.expect_punct(";")?;
            columns.push(ColumnDecl {
                name,
                ty,
                tracked_sum,
                unique,
                span: self.cover(span),
            });
        }
        Ok(TableDecl {
            name,
            capacity,
            key_name,
            key_type,
            columns,
            span: self.cover(start),
        })
    }

    fn invariant(&mut self) -> Result<InvariantDecl> {
        let start = self.expect_keyword("invariant")?.span;
        let name = self.ident()?.0;
        self.expect_punct("{")?;
        let (quant_var, quant_table) = if self.accept_keyword("forall") {
            let variable = self.ident()?.0;
            self.expect_keyword("in")?;
            let table = self.ident()?.0;
            self.expect_punct(":")?;
            (Some(variable), Some(table))
        } else {
            (None, None)
        };
        let expr = self.logic_expr()?;
        self.expect_punct(";")?;
        self.expect_punct("}")?;
        Ok(InvariantDecl {
            name,
            quant_var,
            quant_table,
            expr,
            span: self.cover(start),
        })
    }

    fn params(&mut self) -> Result<Vec<Param>> {
        self.expect_punct("(")?;
        let mut params = Vec::new();
        if self.accept_punct(")") {
            return Ok(params);
        }
        loop {
            let (name, span) = self.ident()?;
            self.expect_punct(":")?;
            let ty = self.type_ref()?;
            params.push(Param {
                name,
                ty,
                span: self.cover(span),
            });
            if !self.accept_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        Ok(params)
    }

    fn table_slice(&mut self) -> Result<TableSlice> {
        let (table, start) = self.ident()?;
        let (column, row) = if self.accept_punct("[") {
            let row = if self.accept_punct("*") {
                None
            } else {
                Some(self.logic_expr()?)
            };
            self.expect_punct("]")?;
            self.expect_punct(".")?;
            (self.ident()?.0, row)
        } else {
            self.expect_punct(".")?;
            let column = self.ident()?.0;
            let row = if self.accept_punct("[") {
                let row = if self.accept_punct("*") {
                    None
                } else {
                    Some(self.logic_expr()?)
                };
                self.expect_punct("]")?;
                row
            } else {
                None
            };
            (column, row)
        };
        Ok(TableSlice {
            table,
            column,
            row,
            span: self.cover(start),
        })
    }

    fn projection(&mut self) -> Result<ProjectionDecl> {
        let start = self.expect_keyword("projection")?.span;
        let name = self.ident()?.0;
        let params = self.params()?;
        self.expect_punct("{")?;
        self.expect_keyword("allow_if")?;
        self.expect_punct(":")?;
        let allow_if = Some(self.logic_expr()?);
        self.expect_punct(";")?;
        self.expect_keyword("expose")?;
        self.expect_punct(":")?;
        self.expect_punct("[")?;
        let mut expose = Vec::new();
        if !self.accept_punct("]") {
            loop {
                expose.push(self.table_slice()?);
                if !self.accept_punct(",") {
                    break;
                }
            }
            self.expect_punct("]")?;
        }
        self.expect_punct(";")?;
        self.expect_keyword("enable_intents")?;
        self.expect_punct(":")?;
        self.expect_punct("[")?;
        let mut enable_intents = Vec::new();
        if !self.accept_punct("]") {
            loop {
                let (transition, span) = self.ident()?;
                self.expect_keyword("when")?;
                self.expect_punct("(")?;
                let condition = self.logic_expr()?;
                self.expect_punct(")")?;
                enable_intents.push(IntentGuard {
                    transition,
                    condition,
                    span: self.cover(span),
                });
                if !self.accept_punct(",") {
                    break;
                }
            }
            self.expect_punct("]")?;
        }
        self.expect_punct(";")?;
        self.expect_punct("}")?;
        Ok(ProjectionDecl {
            name,
            params,
            allow_if,
            expose,
            enable_intents,
            span: self.cover(start),
        })
    }

    fn structured_declaration(&mut self) -> Result<Decl> {
        let start = self.advance();
        let name = self.ident()?.0;
        self.expect_punct("{")?;
        let mut fields = Vec::new();
        while !self.accept_punct("}") {
            let (name, span) = self.ident()?;
            self.expect_punct(":")?;
            let ty = self.type_ref()?;
            self.expect_punct(";")?;
            fields.push(Param { name, ty, span });
        }
        let span = self.cover(start.span);
        Ok(if start.text == "record" {
            Decl::Record(RecordDecl { name, fields, span })
        } else {
            Decl::Union(UnionDecl {
                name,
                variants: fields,
                span,
            })
        })
    }
    fn structured_expression(&mut self, logic: bool) -> Result<Option<Expr>> {
        let token = self.peek().clone();
        let name = token.text.as_str();
        if name == "field"
            && self
                .tokens
                .get(self.position + 1)
                .is_some_and(|t| t.text == "(")
        {
            self.advance();
            self.expect_punct("(")?;
            let record = Box::new(self.expression(logic)?);
            self.expect_punct(",")?;
            let field = self.ident()?.0;
            self.expect_punct(")")?;
            return Ok(Some(Expr::Field(FieldExpr {
                record,
                field,
                span: self.cover(token.span),
            })));
        }
        if !matches!(name, "record" | "variant" | "is_variant" | "payload")
            || self
                .tokens
                .get(self.position + 1)
                .is_none_or(|t| t.text != "<")
        {
            return Ok(None);
        }
        self.advance();
        self.expect_punct("<")?;
        let ty = self.ident()?.0;
        if name == "record" {
            self.expect_punct(">")?;
            self.expect_punct("(")?;
            let mut fields = Vec::new();
            if !self.accept_punct(")") {
                loop {
                    let field = self.ident()?.0;
                    self.expect_punct(":")?;
                    fields.push((field, self.expression(logic)?));
                    if self.accept_punct(")") {
                        break;
                    }
                    self.expect_punct(",")?;
                }
            }
            return Ok(Some(Expr::Record(RecordExpr {
                ty,
                fields,
                span: self.cover(token.span),
            })));
        }
        self.expect_punct(",")?;
        let variant = self.ident()?.0;
        self.expect_punct(">")?;
        self.expect_punct("(")?;
        let value = Box::new(self.expression(logic)?);
        self.expect_punct(")")?;
        let span = self.cover(token.span);
        Ok(Some(match name {
            "variant" => Expr::Variant(VariantExpr {
                ty,
                variant,
                payload: value,
                span,
            }),
            "is_variant" => Expr::IsVariant(VariantAccess {
                ty,
                variant,
                value,
                span,
            }),
            _ => Expr::Payload(VariantAccess {
                ty,
                variant,
                value,
                span,
            }),
        }))
    }

    fn function(&mut self) -> Result<FunctionDecl> {
        let start = self.advance().span;
        let name = self.ident()?.0;
        let params = self.params()?;
        self.expect_punct("->")?;
        let result_type = self.type_ref()?;
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        while self.accept_keyword("require") {
            let name = self.ident()?.0;
            self.expect_punct(":")?;
            requires.push((name, self.logic_expr()?));
            self.expect_punct(";")?;
        }
        while self.accept_keyword("ensure") {
            let name = self.ident()?.0;
            self.expect_punct(":")?;
            ensures.push((name, self.logic_expr()?));
            self.expect_punct(";")?;
        }
        self.expect_keyword("body")?;
        self.expect_punct("{")?;
        self.expect_keyword("yield")?;
        let body = self.pure_expr()?;
        self.expect_punct(";")?;
        self.expect_punct("}")?;
        Ok(FunctionDecl {
            name,
            params,
            result_type,
            requires,
            ensures,
            body,
            span: self.cover(start),
        })
    }

    fn effect(&mut self) -> Result<EffectDecl> {
        let start = self.expect_keyword("effect")?.span;
        let name = self.ident()?.0;
        let params = self.params()?;
        self.expect_punct(";")?;
        Ok(EffectDecl {
            name,
            params,
            span: self.cover(start),
        })
    }

    fn access_path(&mut self) -> Result<AccessPath> {
        let (table, start) = self.ident()?;
        self.expect_punct("[")?;
        let key = if self.accept_punct("*") {
            None
        } else {
            Some(Box::new(self.logic_expr()?))
        };
        self.expect_punct("]")?;
        self.expect_punct(".")?;
        let column = self.ident()?.0;
        Ok(AccessPath {
            table,
            key,
            column,
            span: self.cover(start),
        })
    }

    fn access_list(&mut self) -> Result<Vec<AccessPath>> {
        self.expect_punct("[")?;
        let mut paths = Vec::new();
        if !self.accept_punct("]") {
            loop {
                paths.push(self.access_path()?);
                if !self.accept_punct(",") {
                    break;
                }
            }
            self.expect_punct("]")?;
        }
        Ok(paths)
    }

    fn transition(&mut self) -> Result<TransitionDecl> {
        let start = self.expect_keyword("transition")?.span;
        let name = self.ident()?.0;
        let params = self.params()?;
        self.expect_keyword("reads")?;
        let reads = self.access_list()?;
        self.expect_keyword("writes")?;
        let writes = self.access_list()?;
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        while self.accept_keyword("require") {
            let name = self.ident()?.0;
            self.expect_punct(":")?;
            requires.push((name, self.logic_expr()?));
            self.expect_punct(";")?;
        }
        while self.accept_keyword("ensure") {
            let name = self.ident()?.0;
            self.expect_punct(":")?;
            ensures.push((name, self.post_logic_expr()?));
            self.expect_punct(";")?;
        }
        self.expect_keyword("body")?;
        self.expect_punct("{")?;
        let mut body = self.statements("commit")?;
        let commit = self.expect_keyword("commit")?.span;
        self.expect_punct(";")?;
        body.push(Stmt::Commit(Commit {
            span: self.cover(commit),
        }));
        self.expect_punct("}")?;
        Ok(TransitionDecl {
            name,
            params,
            reads,
            writes,
            requires,
            ensures,
            body,
            span: self.cover(start),
        })
    }

    fn statements(&mut self, stop: &str) -> Result<Vec<Stmt>> {
        let mut statements = Vec::new();
        loop {
            if matches!(self.peek().kind, TokenKind::Eof) {
                return Err(self.error("unexpected end of file in statement list"));
            }
            if self.punct("}") || self.keyword(stop) {
                break;
            }
            statements.push(self.statement()?);
        }
        Ok(statements)
    }

    fn statement(&mut self) -> Result<Stmt> {
        let token = self.peek().clone();
        match &token.kind {
            TokenKind::Keyword(name) => match name.as_str() {
                "let" => self.let_statement(),
                "guard" => {
                    self.advance();
                    self.expect_punct("(")?;
                    let cond = self.pure_expr()?;
                    self.expect_punct(",")?;
                    let code = self.ident()?.0;
                    self.expect_punct(")")?;
                    self.expect_punct(";")?;
                    Ok(Stmt::Guard(Guard {
                        cond,
                        code,
                        span: self.cover(token.span),
                    }))
                }
                "table_set" => {
                    self.advance();
                    self.expect_punct("(")?;
                    let table = self.ident()?.0;
                    self.expect_punct(",")?;
                    let key = self.pure_expr()?;
                    self.expect_punct(",")?;
                    let column = self.ident()?.0;
                    self.expect_punct(",")?;
                    let value = self.pure_expr()?;
                    self.expect_punct(")")?;
                    self.expect_punct(";")?;
                    Ok(Stmt::TableSet(TableSet {
                        table,
                        key,
                        column,
                        value,
                        span: self.cover(token.span),
                    }))
                }
                "emit" => {
                    self.advance();
                    let effect = self.ident()?.0;
                    self.expect_punct("(")?;
                    let mut args = Vec::new();
                    if !self.accept_punct(")") {
                        loop {
                            args.push(self.pure_expr()?);
                            if !self.accept_punct(",") {
                                break;
                            }
                        }
                        self.expect_punct(")")?;
                    }
                    self.expect_punct(";")?;
                    Ok(Stmt::Emit(Emit {
                        effect,
                        args,
                        span: self.cover(token.span),
                    }))
                }
                "yield" => {
                    self.advance();
                    let value = self.pure_expr()?;
                    self.expect_punct(";")?;
                    Ok(Stmt::Yield(Yield {
                        value,
                        span: self.cover(token.span),
                    }))
                }
                _ => Err(self.error(format!("unexpected statement keyword {name:?}"))),
            },
            _ => Err(self.error(format!(
                "expected statement keyword but found {:?}",
                token.text
            ))),
        }
    }

    fn let_statement(&mut self) -> Result<Stmt> {
        let start = self.expect_keyword("let")?.span;
        let name = self.ssa()?;
        self.expect_punct(":")?;
        let ty = self.type_ref()?;
        self.expect_punct("=")?;
        if self.accept_keyword("table_alloc") {
            self.expect_punct("(")?;
            let table = self.ident()?.0;
            if ty.name != "id64" || ty.type_args.len() != 1 || ty.type_args[0].name != table {
                return Err(self.error(format!("allocation result must be declared id64<{table}>")));
            }
            self.expect_punct(",")?;
            self.expect_punct("{")?;
            let mut fields = Vec::new();
            if !self.accept_punct("}") {
                loop {
                    let name = self.ident()?.0;
                    self.expect_punct(":")?;
                    fields.push((name, self.pure_expr()?));
                    if !self.accept_punct(",") {
                        break;
                    }
                }
                self.expect_punct("}")?;
            }
            self.expect_punct(")")?;
            self.expect_punct(";")?;
            Ok(Stmt::TableAlloc(TableAlloc {
                name,
                table,
                fields,
                span: self.cover(start),
            }))
        } else {
            let value = self.pure_expr()?;
            self.expect_punct(";")?;
            Ok(Stmt::Let(Let {
                name,
                ty,
                value,
                span: self.cover(start),
            }))
        }
    }

    fn nested<T>(&mut self, parser: fn(&mut Self) -> Result<T>) -> Result<T> {
        parser(self)
    }

    fn pure_expr(&mut self) -> Result<Expr> {
        let expr = self.nested(Self::pure_expr_inner)?;
        self.checked_tree(expr)
    }

    fn effect_call(&self) -> bool {
        matches!(&self.peek().kind, TokenKind::Ident(_))
            && self
                .tokens
                .get(self.position + 1)
                .is_some_and(|t| matches!(&t.kind, TokenKind::Punct(p) if p == "(" || (p=="<" && matches!(self.peek().text.as_str(),"record"|"variant"|"is_variant"|"payload"))))
    }

    fn pure_expr_inner(&mut self) -> Result<Expr> {
        if self.effect_call() {
            return self.call(false);
        }
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Punct(ref name) if name == "-" => {
                self.advance();
                let operand = self.pure_expr()?;
                Ok(self.negated(operand, token.span))
            }
            TokenKind::Integer(value) => {
                self.advance();
                Ok(Self::literal(LiteralValue::Integer(value), token.span))
            }
            TokenKind::Decimal(value) => {
                self.advance();
                Ok(Self::literal(LiteralValue::Decimal(value), token.span))
            }
            TokenKind::String(value) => {
                self.advance();
                Ok(Self::literal(LiteralValue::String(value), token.span))
            }
            TokenKind::Ident(name) | TokenKind::Ssa(name) => {
                self.advance();
                Ok(Expr::Var(Var {
                    name,
                    span: token.span,
                }))
            }
            TokenKind::Punct(ref name) if name == "(" => {
                self.advance();
                let expr = self.pure_expr()?;
                self.expect_punct(")")?;
                Ok(expr)
            }
            TokenKind::Keyword(ref name) if name == "old" || name == "sum" || name == "old_sum" => {
                self.post_logic_expr()
            }
            TokenKind::Keyword(_) => self.call(false),
            _ => Err(self.error(format!("expected expression but found {:?}", token.text))),
        }
    }

    fn literal(value: LiteralValue, span: Span) -> Expr {
        Expr::Literal(Box::new(Literal {
            value,
            type_ref: None,
            span,
        }))
    }

    fn negated(&self, expr: Expr, start: Span) -> Expr {
        if let Expr::Literal(mut literal) = expr {
            match &mut literal.value {
                LiteralValue::Integer(value) => {
                    *value = value.negated();
                    literal.span = self.cover(start);
                    return Expr::Literal(literal);
                }
                LiteralValue::Decimal(value) => {
                    *value = value.negated();
                    literal.span = self.cover(start);
                    return Expr::Literal(literal);
                }
                _ => {}
            }
            Expr::Unary(Unary {
                op: "neg".into(),
                operand: Box::new(Expr::Literal(literal)),
                span: self.cover(start),
            })
        } else {
            Expr::Unary(Unary {
                op: "neg".into(),
                operand: Box::new(expr),
                span: self.cover(start),
            })
        }
    }

    fn expression(&mut self, logic: bool) -> Result<Expr> {
        if logic {
            self.logic_expr()
        } else {
            self.pure_expr()
        }
    }

    fn call(&mut self, logic: bool) -> Result<Expr> {
        if let Some(value) = self.structured_expression(logic)? {
            return Ok(value);
        }
        let token = self.peek().clone();
        let name = match &token.kind {
            TokenKind::Keyword(name) | TokenKind::Ident(name) => name.as_str(),
            _ => return Err(self.error("expected expression")),
        };
        if name == "fold_bounded" {
            return self.fold();
        }
        if name == "true" || name == "false" {
            self.advance();
            return Ok(Self::literal(
                LiteralValue::Bool(name == "true"),
                token.span,
            ));
        }
        if name == "not" || name == "neg" {
            self.advance();
            self.expect_punct("(")?;
            // Canonical unary operators use a canonical operand even in specs.
            let operand = Box::new(self.pure_expr()?);
            self.expect_punct(")")?;
            return Ok(Expr::Unary(Unary {
                op: name.into(),
                operand,
                span: self.cover(token.span),
            }));
        }
        if is_binary(name) {
            self.advance();
            self.expect_punct("(")?;
            let left = self.expression(logic)?;
            self.expect_punct(",")?;
            let right = self.expression(logic)?;
            self.expect_punct(")")?;
            return self.binary(name, left, right, token.span);
        }
        if name == "select" {
            self.advance();
            self.expect_punct("(")?;
            let cond = Box::new(self.expression(logic)?);
            self.expect_punct(",")?;
            let a = Box::new(self.expression(logic)?);
            self.expect_punct(",")?;
            let b = Box::new(self.expression(logic)?);
            self.expect_punct(")")?;
            return Ok(Expr::Select(Select {
                cond,
                a,
                b,
                span: self.cover(token.span),
            }));
        }
        if name == "effect_count" {
            self.advance();
            self.expect_punct("(")?;
            self.expect_punct(")")?;
            return Ok(Expr::EffectCount(EffectCount {
                span: self.cover(token.span),
            }));
        }
        if name == "effect_is" || name == "effect_arg" {
            self.advance();
            self.expect_punct("(")?;
            let effect = self.ident()?.0;
            self.expect_punct(",")?;
            let index = Box::new(self.expression(logic)?);
            let parameter = if name == "effect_arg" {
                self.expect_punct(",")?;
                Some(self.ident()?.0)
            } else {
                None
            };
            self.expect_punct(")")?;
            let span = self.cover(token.span);
            return Ok(if let Some(parameter) = parameter {
                Expr::EffectArg(EffectArg {
                    effect,
                    index,
                    parameter,
                    span,
                })
            } else {
                Expr::EffectIs(EffectIs {
                    effect,
                    index,
                    span,
                })
            });
        }
        if name == "key_of" || name == "id64" {
            self.advance();
            self.expect_punct("<")?;
            let table = self.ident()?.0;
            self.expect_punct(">")?;
            self.expect_punct("(")?;
            let expr = Box::new(self.expression(logic)?);
            self.expect_punct(")")?;
            return Ok(Expr::KeyOf(KeyOf {
                table,
                expr,
                span: self.cover(token.span),
            }));
        }
        if matches!(name, "old" | "sum" | "old_sum") {
            return self.post_logic_expr();
        }
        if matches!(
            name,
            "table_get" | "table_exists" | "table_lookup" | "table_exists_unique"
        ) {
            self.advance();
            self.expect_punct("(")?;
            let table = self.ident()?.0;
            self.expect_punct(",")?;
            if name == "table_lookup" || name == "table_exists_unique" {
                let key_column = self.ident()?.0;
                self.expect_punct(",")?;
                let key = Box::new(self.expression(logic)?);
                self.expect_punct(")")?;
                let span = self.cover(token.span);
                return if name == "table_lookup" {
                    Ok(Expr::TableLookup(TableLookup {
                        table,
                        key_column,
                        key,
                        span,
                    }))
                } else {
                    Ok(Expr::TableExistsUnique(TableExistsUnique {
                        table,
                        key_column,
                        key,
                        span,
                    }))
                };
            }
            let key = Box::new(self.expression(logic)?);
            if name == "table_exists" {
                self.expect_punct(")")?;
                return Ok(Expr::TableExists(TableExists {
                    table,
                    key,
                    span: self.cover(token.span),
                }));
            }
            self.expect_punct(",")?;
            let column = self.ident()?.0;
            self.expect_punct(")")?;
            return Ok(Expr::TableGet(TableGet {
                table,
                key,
                column,
                span: self.cover(token.span),
            }));
        }
        if matches!(&token.kind, TokenKind::Ident(_)) {
            self.advance();
            self.expect_punct("(")?;
            let mut args = Vec::new();
            if !self.accept_punct(")") {
                loop {
                    args.push(self.expression(logic)?);
                    if !self.accept_punct(",") {
                        break;
                    }
                }
                self.expect_punct(")")?;
            }
            return Ok(Expr::Call(Call {
                function: name.into(),
                args,
                span: self.cover(token.span),
            }));
        }
        Err(self.error(format!("expected expression but found {:?}", token.text)))
    }

    fn fold(&mut self) -> Result<Expr> {
        let start = self.expect_keyword("fold_bounded")?.span;
        self.expect_punct("<")?;
        let n = self.integer()?;
        self.expect_punct(">")?;
        self.expect_punct("(")?;
        let acc_name = self.binding_name("accumulator")?;
        self.expect_punct(":")?;
        let acc_type = self.type_ref()?;
        self.expect_punct("=")?;
        let init = Box::new(self.pure_expr()?);
        self.expect_punct(",")?;
        let index_name = self.binding_name("index")?;
        self.expect_keyword("in")?;
        self.expect_punct("[")?;
        let lo = Box::new(self.pure_expr()?);
        self.expect_punct("..")?;
        let hi = Box::new(self.pure_expr()?);
        self.expect_punct("]")?;
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        let body = self.statements("yield")?;
        self.expect_keyword("yield")?;
        let yield_expr = Box::new(self.pure_expr()?);
        self.expect_punct(";")?;
        self.expect_punct("}")?;
        Ok(Expr::Fold(Box::new(Fold {
            n,
            acc_name,
            acc_type,
            init,
            index_name,
            lo,
            hi,
            body,
            yield_expr,
            span: self.cover(start),
        })))
    }

    fn post_logic_expr(&mut self) -> Result<Expr> {
        let token = self.peek().clone();
        if self.accept_keyword("old") {
            self.expect_punct("(")?;
            let access = self.access_path()?;
            self.expect_punct(")")?;
            return self.checked_tree(Expr::Old(Box::new(Old {
                access,
                span: self.cover(token.span),
            })));
        }
        if self.keyword("sum") || self.keyword("old_sum") {
            let old = self.keyword("old_sum");
            self.advance();
            self.expect_punct("(")?;
            let table = self.ident()?.0;
            self.expect_punct(".")?;
            let column = self.ident()?.0;
            self.expect_punct(")")?;
            return Ok(Expr::SumAgg(SumAgg {
                table,
                column,
                old,
                span: self.cover(token.span),
            }));
        }
        self.logic_expr()
    }

    fn logic_expr(&mut self) -> Result<Expr> {
        let expr = self.nested(Self::logic_or)?;
        self.checked_tree(expr)
    }

    fn logic_or(&mut self) -> Result<Expr> {
        let mut left = self.logic_and()?;
        while self.keyword("or") || self.punct("||") {
            let span = self.advance().span;
            let right = self.logic_and()?;
            left = self.binary("or", left, right, span)?;
        }
        Ok(left)
    }

    fn logic_and(&mut self) -> Result<Expr> {
        let mut left = self.logic_not()?;
        while self.keyword("and") || self.punct("&&") {
            let span = self.advance().span;
            let right = self.logic_not()?;
            left = self.binary("and", left, right, span)?;
        }
        Ok(left)
    }

    fn logic_not(&mut self) -> Result<Expr> {
        if self.keyword("not") || self.punct("!") {
            let start = self.advance().span;
            let operand = Box::new(self.nested(Self::logic_not)?);
            return Ok(Expr::Unary(Unary {
                op: "not".into(),
                operand,
                span: self.cover(start),
            }));
        }
        self.logic_comparison()
    }

    fn logic_comparison(&mut self) -> Result<Expr> {
        let left = self.logic_additive()?;
        let op = match &self.peek().kind {
            TokenKind::Punct(symbol) => match symbol.as_str() {
                "==" => "eq",
                "!=" => "ne",
                "<" => "lt",
                "<=" => "le",
                ">" => "gt",
                ">=" => "ge",
                _ => return Ok(left),
            },
            _ => return Ok(left),
        };
        let span = self.advance().span;
        let right = self.logic_additive()?;
        self.binary(op, left, right, span)
    }

    fn logic_additive(&mut self) -> Result<Expr> {
        let mut left = self.logic_multiplicative()?;
        loop {
            let op = if self.punct("+") {
                "add"
            } else if self.punct("-") {
                "sub"
            } else {
                break;
            };
            let span = self.advance().span;
            let right = self.logic_multiplicative()?;
            left = self.binary(op, left, right, span)?;
        }
        Ok(left)
    }

    fn logic_multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.logic_unary()?;
        loop {
            let op = if self.punct("*") {
                "mul"
            } else if self.punct("/") {
                "div"
            } else if self.punct("%") {
                "rem"
            } else {
                break;
            };
            let span = self.advance().span;
            let right = self.logic_unary()?;
            left = self.binary(op, left, right, span)?;
        }
        Ok(left)
    }

    fn logic_unary(&mut self) -> Result<Expr> {
        if self.punct("!") || self.punct("-") {
            let token = self.advance();
            let operand = self.nested(Self::logic_unary)?;
            return if token.text == "-" {
                Ok(self.negated(operand, token.span))
            } else {
                Ok(Expr::Unary(Unary {
                    op: "not".into(),
                    operand: Box::new(operand),
                    span: self.cover(token.span),
                }))
            };
        }
        if self.keyword("not") || self.keyword("neg") {
            return self.call(false);
        }
        self.logic_atom()
    }

    fn logic_atom(&mut self) -> Result<Expr> {
        if self.effect_call() {
            return self.call(true);
        }
        let token = self.peek().clone();
        if self.accept_punct("(") {
            let expr = self.logic_expr()?;
            self.expect_punct(")")?;
            return Ok(expr);
        }
        match &token.kind {
            TokenKind::Integer(_) | TokenKind::Decimal(_) | TokenKind::String(_) => {
                self.pure_expr()
            }
            TokenKind::Ident(name) | TokenKind::Ssa(name) => {
                let table = name.clone();
                self.advance();
                if self.accept_punct("[") {
                    let key = Some(Box::new(self.logic_expr()?));
                    self.expect_punct("]")?;
                    self.expect_punct(".")?;
                    let column = self.ident()?.0;
                    return Ok(Expr::TableAccess(TableAccess {
                        table,
                        key,
                        column,
                        span: self.cover(token.span),
                    }));
                }
                if self.accept_punct(".") {
                    let column = self.ident()?.0;
                    let key = if self.accept_punct("[") {
                        // Preserve the v1.1 expression grammar: whole-column
                        // wildcards are accepted by projection slices, not
                        // ordinary T.column[*] specification expressions.
                        if self.punct("*") {
                            return Err(
                                self.error("wildcard column expressions are not admitted here")
                            );
                        }
                        let key = Some(Box::new(self.logic_expr()?));
                        self.expect_punct("]")?;
                        key
                    } else {
                        None
                    };
                    return Ok(Expr::TableAccess(TableAccess {
                        table,
                        key,
                        column,
                        span: self.cover(token.span),
                    }));
                }
                Ok(Expr::Var(Var {
                    name: table,
                    span: token.span,
                }))
            }
            TokenKind::Keyword(_) => self.call(true),
            _ => Err(self.error(format!(
                "expected logic expression but found {:?}",
                token.text
            ))),
        }
    }

    fn binary(&self, op: &str, left: Expr, right: Expr, span: Span) -> Result<Expr> {
        let expr = Expr::Binary(Binary {
            op: op.into(),
            left: Box::new(left),
            right: Box::new(right),
            span: self.cover(span),
        });
        self.checked_tree(expr)
    }

    fn checked_tree(&self, expr: Expr) -> Result<Expr> {
        Ok(expr)
    }
}

fn is_binary(name: &str) -> bool {
    matches!(
        name,
        "add"
            | "sub"
            | "mul"
            | "div"
            | "rem"
            | "eq"
            | "ne"
            | "lt"
            | "le"
            | "gt"
            | "ge"
            | "and"
            | "or"
            | "xor"
            | "min"
            | "max"
    )
}
