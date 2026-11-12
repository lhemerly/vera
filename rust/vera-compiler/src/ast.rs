//! Lossless syntactic representation of VERA source.
//!
//! Source spans belong to the native representation. Canonical serialization
//! deliberately excludes them so syntax can be compared across implementations.

use crate::diagnostic::Span;
use crate::literal::{DecimalLiteral, IntegerLiteral};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

macro_rules! serialize_node {
    ($ty:ty, $node:literal, $( $field:ident => $name:literal ),* $(,)?) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("node", $node)?;
                $(map.serialize_entry($name, &self.$field)?;)*
                map.end()
            }
        }
    };
}

macro_rules! node_enum {
    ($name:ident { $( $variant:ident($ty:ty) ),* $(,)? }) => {
        #[derive(Clone, Debug)]
        pub enum $name { $( $variant($ty) ),* }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self { $(Self::$variant(value) => value.serialize(serializer)),* }
            }
        }
    };
}

#[derive(Clone, Debug)]
pub struct Module {
    pub name: Option<String>,
    pub decls: Vec<Decl>,
    pub span: Span,
}

impl Module {
    pub fn canonical_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("VERA syntax consists of JSON-serializable values")
    }
}

serialize_node!(Module, "Module", name => "name", decls => "decls");

node_enum!(Decl {
    Table(TableDecl),
    Invariant(InvariantDecl),
    Projection(ProjectionDecl),
    Effect(EffectDecl),
    Transition(TransitionDecl),
    Function(FunctionDecl),
});

#[derive(Clone, Debug)]
pub struct TypeRef {
    pub name: String,
    pub type_args: Vec<TypeRef>,
    pub enum_tags: Vec<String>,
    pub span: Span,
}
serialize_node!(TypeRef, "TypeRef", name => "name", type_args => "type_args", enum_tags => "enum_tags");

#[derive(Clone, Debug)]
pub struct ColumnDecl {
    pub name: String,
    pub ty: TypeRef,
    pub tracked_sum: bool,
    pub unique: bool,
    pub span: Span,
}
serialize_node!(ColumnDecl, "ColumnDecl", name => "name", ty => "type", tracked_sum => "tracked_sum", unique => "unique");

#[derive(Clone, Debug)]
pub struct TableDecl {
    pub name: String,
    pub capacity: IntegerLiteral,
    pub key_name: String,
    pub key_type: TypeRef,
    pub columns: Vec<ColumnDecl>,
    pub span: Span,
}
serialize_node!(TableDecl, "TableDecl", name => "name", capacity => "capacity", key_name => "key_name", key_type => "key_type", columns => "columns");

#[derive(Clone, Debug)]
pub struct InvariantDecl {
    pub name: String,
    pub quant_var: Option<String>,
    pub quant_table: Option<String>,
    pub expr: Expr,
    pub span: Span,
}
serialize_node!(InvariantDecl, "InvariantDecl", name => "name", quant_var => "quant_var", quant_table => "quant_table", expr => "expr");

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
    pub span: Span,
}
serialize_node!(Param, "Param", name => "name", ty => "type");

#[derive(Clone, Debug)]
pub struct TableSlice {
    pub table: String,
    pub column: String,
    pub row: Option<Expr>,
    pub span: Span,
}
serialize_node!(TableSlice, "TableSlice", table => "table", column => "column", row => "row");

#[derive(Clone, Debug)]
pub struct IntentGuard {
    pub transition: String,
    pub condition: Expr,
    pub span: Span,
}
serialize_node!(IntentGuard, "IntentGuard", transition => "transition", condition => "condition");

#[derive(Clone, Debug)]
pub struct ProjectionDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub allow_if: Option<Expr>,
    pub expose: Vec<TableSlice>,
    pub enable_intents: Vec<IntentGuard>,
    pub span: Span,
}
serialize_node!(ProjectionDecl, "ProjectionDecl", name => "name", params => "params", allow_if => "allow_if", expose => "expose", enable_intents => "enable_intents");

#[derive(Clone, Debug)]
pub struct EffectDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub span: Span,
}
serialize_node!(EffectDecl, "EffectDecl", name => "name", params => "params");

#[derive(Clone, Debug)]
pub struct AccessPath {
    pub table: String,
    pub key: Option<Box<Expr>>,
    pub column: String,
    pub span: Span,
}
serialize_node!(AccessPath, "AccessPath", table => "table", key => "key", column => "column");

#[derive(Clone, Debug)]
pub struct TransitionDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub reads: Vec<AccessPath>,
    pub writes: Vec<AccessPath>,
    pub requires: Vec<(String, Expr)>,
    pub ensures: Vec<(String, Expr)>,
    pub body: Vec<Stmt>,
    pub span: Span,
}
serialize_node!(TransitionDecl, "TransitionDecl", name => "name", params => "params", reads => "reads", writes => "writes", requires => "requires", ensures => "ensures", body => "body");

node_enum!(Expr {
    Var(Var),
    Literal(Box<Literal>),
    Unary(Unary),
    Binary(Binary),
    Select(Select),
    TableGet(TableGet),
    TableExists(TableExists),
    TableExistsUnique(TableExistsUnique),
    TableLookup(TableLookup),
    TableAccess(TableAccess),
    Old(Box<Old>),
    SumAgg(SumAgg),
    KeyOf(KeyOf),
    Fold(Box<Fold>),
    Call(Call),
    EffectCount(EffectCount),
    EffectIs(EffectIs),
    EffectArg(EffectArg),
});

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Self::Var(v) => v.span,
            Self::Literal(v) => v.span,
            Self::Unary(v) => v.span,
            Self::Binary(v) => v.span,
            Self::Select(v) => v.span,
            Self::TableGet(v) => v.span,
            Self::TableExists(v) => v.span,
            Self::TableExistsUnique(v) => v.span,
            Self::TableLookup(v) => v.span,
            Self::TableAccess(v) => v.span,
            Self::Old(v) => v.span,
            Self::SumAgg(v) => v.span,
            Self::KeyOf(v) => v.span,
            Self::Fold(v) => v.span,
            Self::Call(v) => v.span,
            Self::EffectCount(v) => v.span,
            Self::EffectIs(v) => v.span,
            Self::EffectArg(v) => v.span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    pub span: Span,
}
serialize_node!(Var, "Var", name => "name");

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum LiteralValue {
    Bool(bool),
    Integer(IntegerLiteral),
    Decimal(DecimalLiteral),
    String(String),
}

#[derive(Clone, Debug)]
pub struct Literal {
    pub value: LiteralValue,
    pub type_ref: Option<TypeRef>,
    pub span: Span,
}
serialize_node!(Literal, "Literal", value => "value", type_ref => "type_ref");

#[derive(Clone, Debug)]
pub struct Unary {
    pub op: String,
    pub operand: Box<Expr>,
    pub span: Span,
}
serialize_node!(Unary, "Unary", op => "op", operand => "operand");

#[derive(Clone, Debug)]
pub struct Binary {
    pub op: String,
    pub left: Box<Expr>,
    pub right: Box<Expr>,
    pub span: Span,
}
serialize_node!(Binary, "Binary", op => "op", left => "left", right => "right");

#[derive(Clone, Debug)]
pub struct Select {
    pub cond: Box<Expr>,
    pub a: Box<Expr>,
    pub b: Box<Expr>,
    pub span: Span,
}
serialize_node!(Select, "Select", cond => "cond", a => "a", b => "b");

#[derive(Clone, Debug)]
pub struct TableGet {
    pub table: String,
    pub key: Box<Expr>,
    pub column: String,
    pub span: Span,
}
serialize_node!(TableGet, "TableGet", table => "table", key => "key", column => "column");

#[derive(Clone, Debug)]
pub struct TableExists {
    pub table: String,
    pub key: Box<Expr>,
    pub span: Span,
}
serialize_node!(TableExists, "TableExists", table => "table", key => "key");

#[derive(Clone, Debug)]
pub struct TableExistsUnique {
    pub table: String,
    pub key_column: String,
    pub key: Box<Expr>,
    pub span: Span,
}
serialize_node!(TableExistsUnique, "TableExistsUnique", table => "table", key_column => "key_column", key => "key");

#[derive(Clone, Debug)]
pub struct TableLookup {
    pub table: String,
    pub key_column: String,
    pub key: Box<Expr>,
    pub span: Span,
}
serialize_node!(TableLookup, "TableLookup", table => "table", key_column => "key_column", key => "key");

#[derive(Clone, Debug)]
pub struct TableAccess {
    pub table: String,
    pub key: Option<Box<Expr>>,
    pub column: String,
    pub span: Span,
}
serialize_node!(TableAccess, "TableAccess", table => "table", key => "key", column => "column");

#[derive(Clone, Debug)]
pub struct Old {
    pub access: AccessPath,
    pub span: Span,
}
serialize_node!(Old, "Old", access => "access");

#[derive(Clone, Debug)]
pub struct SumAgg {
    pub table: String,
    pub column: String,
    pub old: bool,
    pub span: Span,
}
serialize_node!(SumAgg, "SumAgg", table => "table", column => "column", old => "old");

#[derive(Clone, Debug)]
pub struct KeyOf {
    pub table: String,
    pub expr: Box<Expr>,
    pub span: Span,
}
serialize_node!(KeyOf, "KeyOf", table => "table", expr => "expr");

#[derive(Clone, Debug)]
pub struct Fold {
    pub n: IntegerLiteral,
    pub acc_name: String,
    pub acc_type: TypeRef,
    pub init: Box<Expr>,
    pub index_name: String,
    pub lo: Box<Expr>,
    pub hi: Box<Expr>,
    pub body: Vec<Stmt>,
    pub yield_expr: Box<Expr>,
    pub span: Span,
}
serialize_node!(Fold, "Fold", n => "n", acc_name => "acc_name", acc_type => "acc_type", init => "init", index_name => "index_name", lo => "lo", hi => "hi", body => "body", yield_expr => "yield_expr");

node_enum!(Stmt {
    Let(Let),
    Guard(Guard),
    TableSet(TableSet),
    TableAlloc(TableAlloc),
    Emit(Emit),
    Yield(Yield),
    Commit(Commit),
});

#[derive(Clone, Debug)]
pub struct Let {
    pub name: String,
    pub ty: TypeRef,
    pub value: Expr,
    pub span: Span,
}
serialize_node!(Let, "Let", name => "name", ty => "type", value => "value");

#[derive(Clone, Debug)]
pub struct Guard {
    pub cond: Expr,
    pub code: String,
    pub span: Span,
}
serialize_node!(Guard, "Guard", cond => "cond", code => "code");

#[derive(Clone, Debug)]
pub struct TableSet {
    pub table: String,
    pub key: Expr,
    pub column: String,
    pub value: Expr,
    pub span: Span,
}
serialize_node!(TableSet, "TableSet", table => "table", key => "key", column => "column", value => "value");

#[derive(Clone, Debug)]
pub struct TableAlloc {
    pub name: String,
    pub table: String,
    pub fields: Vec<(String, Expr)>,
    pub span: Span,
}
serialize_node!(TableAlloc, "TableAlloc", name => "name", table => "table", fields => "fields");

#[derive(Clone, Debug)]
pub struct Emit {
    pub effect: String,
    pub args: Vec<Expr>,
    pub span: Span,
}
serialize_node!(Emit, "Emit", effect => "effect", args => "args");

#[derive(Clone, Debug)]
pub struct Yield {
    pub value: Expr,
    pub span: Span,
}
serialize_node!(Yield, "Yield", value => "value");

#[derive(Clone, Debug)]
pub struct Commit {
    pub span: Span,
}
serialize_node!(Commit, "Commit",);

/// Transaction-local outbox observations, admitted only in postconditions.
#[derive(Clone, Debug)]
pub struct EffectCount {
    pub span: Span,
}
serialize_node!(EffectCount, "EffectCount",);
#[derive(Clone, Debug)]
pub struct EffectIs {
    pub effect: String,
    pub index: Box<Expr>,
    pub span: Span,
}
serialize_node!(EffectIs, "EffectIs", effect => "effect", index => "index");
#[derive(Clone, Debug)]
pub struct EffectArg {
    pub effect: String,
    pub index: Box<Expr>,
    pub parameter: String,
    pub span: Span,
}
serialize_node!(EffectArg, "EffectArg", effect => "effect", index => "index", parameter => "parameter");

#[derive(Clone, Debug)]
pub struct FunctionDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub result_type: TypeRef,
    pub requires: Vec<(String, Expr)>,
    pub ensures: Vec<(String, Expr)>,
    pub body: Expr,
    pub span: Span,
}
serialize_node!(FunctionDecl, "FunctionDecl", name => "name", params => "params", result_type => "result_type", requires => "requires", ensures => "ensures", body => "body");
#[derive(Clone, Debug)]
pub struct Call {
    pub function: String,
    pub args: Vec<Expr>,
    pub span: Span,
}
serialize_node!(Call, "Call", function => "function", args => "args");
