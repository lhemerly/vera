//! Native VERA frontend. Syntax parsing does not establish semantic validity.
//!
//! The Python-free compiler migration begins with a complete syntax boundary;
//! later analysis will construct the resolved program consumed by all backends.

pub mod ast;
pub mod diagnostic;
pub mod lexer;
pub mod literal;
pub mod parser;

pub use diagnostic::{Diagnostic, Span};
pub use parser::parse_source;
