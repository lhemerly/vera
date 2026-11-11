use std::fmt;

use serde::{Deserialize, Serialize};

/// Half-open UTF-8 byte range, with one-based source line and character column.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn parse(message: impl Into<String>, span: Span) -> Self {
        Self {
            code: "D_PARSE".into(),
            message: message.into(),
            span,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.code, self.span.line, self.span.column, self.message
        )
    }
}

impl std::error::Error for Diagnostic {}
