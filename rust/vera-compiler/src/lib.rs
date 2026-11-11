//! Native VERA compiler, interpreter, verifier, persistence and code generators.
//! Analysis constructs the immutable checked program consumed by every backend.
//! Verification is inductive over valid states; execution preserves atomicity.

pub mod ast;
pub mod batch;
pub mod codegen_gpu;
pub mod codegen_rust;
pub mod diagnostic;
pub mod fuzz;
pub mod ir;
pub mod lexer;
pub mod literal;
pub mod parser;
pub mod persistence;
pub mod projection;
pub mod runtime;
pub mod semantic;
pub mod types;
pub mod value;
pub mod verifier;

pub use diagnostic::{Diagnostic, Span};
pub use parser::parse_source;
pub use semantic::analyze;
