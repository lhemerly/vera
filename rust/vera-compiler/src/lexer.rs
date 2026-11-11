//! Tokenization owns source offsets and literal spelling, without numeric loss.

use crate::diagnostic::{Diagnostic, Span};
use crate::literal::{DecimalLiteral, IntegerLiteral};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Ssa(String),
    Integer(IntegerLiteral),
    Decimal(DecimalLiteral),
    String(String),
    Keyword(String),
    Punct(String),
    Eof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub text: String,
}

pub fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "module"
            | "table"
            | "capacity"
            | "key"
            | "col"
            | "tracked_sum"
            | "invariant"
            | "forall"
            | "in"
            | "projection"
            | "allow_if"
            | "expose"
            | "enable_intents"
            | "when"
            | "effect"
            | "transition"
            | "reads"
            | "writes"
            | "require"
            | "ensure"
            | "body"
            | "commit"
            | "let"
            | "guard"
            | "table_set"
            | "table_alloc"
            | "table_get"
            | "table_exists"
            | "table_lookup"
            | "table_exists_unique"
            | "unique"
            | "select"
            | "fold_bounded"
            | "yield"
            | "emit"
            | "key_of"
            | "bool"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "i64"
            | "dec64"
            | "dec128"
            | "sym32"
            | "bytes32"
            | "id64"
            | "enum"
            | "true"
            | "false"
            | "and"
            | "or"
            | "not"
            | "xor"
            | "add"
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
            | "min"
            | "max"
            | "neg"
            | "old"
            | "sum"
            | "old_sum"
    )
}

fn ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn ident_cont(ch: char) -> bool {
    ident_start(ch) || ch.is_ascii_digit()
}

struct Lexer<'a> {
    source: &'a str,
    position: usize,
    line: usize,
    column: usize,
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<char> {
        self.source[self.position..].chars().next()
    }

    fn starts(&self, text: &str) -> bool {
        self.source[self.position..].starts_with(text)
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.position += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn span(&self) -> Span {
        Span {
            start: self.position,
            end: self.position,
            line: self.line,
            column: self.column,
        }
    }

    fn error(&self, message: impl Into<String>, start: Span) -> Diagnostic {
        Diagnostic::parse(
            message,
            Span {
                end: self.position,
                ..start
            },
        )
    }

    fn token(&self, kind: TokenKind, start: Span) -> Token {
        Token {
            kind,
            span: Span {
                end: self.position,
                ..start
            },
            text: self.source[start.start..self.position].into(),
        }
    }

    fn trivia(&mut self) -> Result<(), Diagnostic> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\n') => {
                    self.advance();
                }
                Some('#') => {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.advance();
                    }
                }
                Some('/') if self.starts("//") => {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.advance();
                    }
                }
                Some('/') if self.starts("/*") => {
                    let start = self.span();
                    self.advance();
                    self.advance();
                    while self.peek().is_some() && !self.starts("*/") {
                        self.advance();
                    }
                    if self.peek().is_none() {
                        return Err(self.error("unterminated block comment", start));
                    }
                    self.advance();
                    self.advance();
                }
                _ => return Ok(()),
            }
        }
    }

    fn number(&mut self, start: Span) -> Result<Token, Diagnostic> {
        if self.starts("0x") || self.starts("0X") {
            self.advance();
            self.advance();
            let digits = self.position;
            while self.peek().is_some_and(|ch| ch.is_ascii_hexdigit()) {
                self.advance();
            }
            if digits == self.position {
                return Err(self.error("empty hexadecimal literal", start));
            }
            let value = IntegerLiteral::parse(&self.source[start.start..self.position])
                .map_err(|e| self.error(e, start))?;
            return Ok(self.token(TokenKind::Integer(value), start));
        }
        while self
            .peek()
            .is_some_and(|ch| ch.is_ascii_digit() || ch == '_')
        {
            self.advance();
        }
        let decimal = self.peek() == Some('.')
            && self.source[self.position + 1..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_digit());
        if decimal {
            self.advance();
            while self
                .peek()
                .is_some_and(|ch| ch.is_ascii_digit() || ch == '_')
            {
                self.advance();
            }
        }
        let text = &self.source[start.start..self.position];
        let kind = if decimal {
            TokenKind::Decimal(DecimalLiteral::parse(text).map_err(|e| self.error(e, start))?)
        } else {
            TokenKind::Integer(IntegerLiteral::parse(text).map_err(|e| self.error(e, start))?)
        };
        Ok(self.token(kind, start))
    }

    fn string(&mut self, start: Span) -> Result<Token, Diagnostic> {
        self.advance();
        let mut value = String::new();
        loop {
            match self.advance() {
                None => return Err(self.error("unterminated string literal", start)),
                Some('"') => return Ok(self.token(TokenKind::String(value), start)),
                Some('\\') => {
                    let Some(escaped) = self.advance() else {
                        return Err(self.error("unterminated string literal", start));
                    };
                    value.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '0' => '\0',
                        other => other,
                    });
                }
                Some(ch) => value.push(ch),
            }
        }
    }

    fn next(&mut self) -> Result<Token, Diagnostic> {
        self.trivia()?;
        let start = self.span();
        let Some(ch) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: start,
                text: "<eof>".into(),
            });
        };
        let next = self.source[self.position..].chars().nth(1);
        if ch == '%' && next.is_some_and(ident_start) {
            self.advance();
            while self.peek().is_some_and(ident_cont) {
                self.advance();
            }
            return Ok(self.token(
                TokenKind::Ssa(self.source[start.start..self.position].into()),
                start,
            ));
        }
        if ident_start(ch) {
            while self.peek().is_some_and(ident_cont) {
                self.advance();
            }
            let text = &self.source[start.start..self.position];
            let kind = if is_keyword(text) {
                TokenKind::Keyword(text.into())
            } else {
                TokenKind::Ident(text.into())
            };
            return Ok(self.token(kind, start));
        }
        if ch.is_ascii_digit() || (ch == '.' && next.is_some_and(|n| n.is_ascii_digit())) {
            return self.number(start);
        }
        if ch == '"' {
            return self.string(start);
        }
        for punct in ["..", "==", "!=", "<=", ">=", "&&", "||", "->"] {
            if self.starts(punct) {
                self.advance();
                self.advance();
                return Ok(self.token(TokenKind::Punct(punct.into()), start));
            }
        }
        if "{}()[];:,.<>=+-*/%!".contains(ch) {
            self.advance();
            return Ok(self.token(TokenKind::Punct(ch.to_string()), start));
        }
        self.advance();
        Err(self.error(format!("unexpected character {ch:?}"), start))
    }
}

pub fn tokenize(source: &str) -> Result<Vec<Token>, Diagnostic> {
    let mut lexer = Lexer {
        source,
        position: 0,
        line: 1,
        column: 1,
    };
    let mut out = Vec::new();
    loop {
        let token = lexer.next()?;
        let done = token.kind == TokenKind::Eof;
        out.push(token);
        if done {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_ssa_remainder_ranges_and_generics() {
        let tokens = tokenize("%x % 2 [0..7] id64<T>=0").unwrap();
        assert!(matches!(&tokens[0].kind, TokenKind::Ssa(s) if s == "%x"));
        assert!(matches!(&tokens[1].kind, TokenKind::Punct(s) if s == "%"));
        assert!(tokens.iter().any(|t| t.text == ".."));
        assert!(tokens.iter().any(|t| t.text == ">="));
    }

    #[test]
    fn comments_and_utf8_strings_keep_byte_and_character_positions() {
        let source = "# heading\n/* comment */ \"Olá 🦀\\n\" // tail\n%value";
        let tokens = tokenize(source).unwrap();
        assert!(matches!(&tokens[0].kind, TokenKind::String(s) if s == "Olá 🦀\n"));
        assert_eq!(tokens[0].span.line, 2);
        assert_eq!(tokens[0].span.column, 15);
        assert_eq!(tokens[1].span.line, 3);
        assert_eq!(tokens[1].span.column, 1);
        assert_eq!(&source[tokens[1].span.start..tokens[1].span.end], "%value");
    }

    #[test]
    fn malformed_tokens_have_source_diagnostics() {
        for source in ["0x", "/* never closed", "\"never closed", "\"\\", "@"] {
            let err = tokenize(source).unwrap_err();
            assert_eq!(err.code, "D_PARSE");
            assert_eq!(err.span.line, 1);
            assert_eq!(err.span.column, 1);
        }
    }

    #[test]
    fn lexer_does_not_round_decimal_literals() {
        let tokens =
            tokenize("1.000000000000000001 0xffffffffffffffffffffffffffffffff .5").unwrap();
        assert!(
            matches!(&tokens[0].kind, TokenKind::Decimal(d) if d.digits == "1000000000000000001" && d.scale == 18)
        );
        assert!(
            matches!(&tokens[1].kind, TokenKind::Integer(n) if n.as_decimal() == "340282366920938463463374607431768211455")
        );
        assert!(
            matches!(&tokens[2].kind, TokenKind::Decimal(d) if d.digits == "5" && d.scale == 1)
        );
    }
}
