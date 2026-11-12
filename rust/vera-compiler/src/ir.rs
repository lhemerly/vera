//! Resolved, typed compiler contract. Only semantic analysis constructs programs.
use crate::{
    ast,
    diagnostic::Span,
    types::{Domain, ScalarType, TableLayout},
    value::Value,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

macro_rules! id {
    ($($name:ident),*) => {$ (
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        pub struct $name(pub usize);
    )*};
}
id!(
    TableId,
    ExprId,
    BindingId,
    TransitionId,
    EffectId,
    ProjectionId
);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct ColumnId {
    pub table: TableId,
    pub index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Snapshot {
    Pre,
    Current,
}

#[derive(Clone, Debug)]
pub struct Column {
    pub id: ColumnId,
    pub name: String,
    pub ty: ScalarType,
    pub tracked_sum: bool,
    pub unique: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub id: TableId,
    pub name: String,
    pub key_name: String,
    pub capacity: u64,
    pub columns: Vec<Column>,
    pub layout: TableLayout,
    pub span: Span,
}
impl Table {
    pub fn key_bits(&self) -> u32 {
        self.capacity.trailing_zeros()
    }
    pub fn generation_bits(&self) -> u32 {
        (64 - self.key_bits()).min(32)
    }
    pub fn column_id(&self, name: &str) -> Option<ColumnId> {
        self.columns.iter().find(|c| c.name == name).map(|c| c.id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingOrigin {
    Parameter,
    Local,
    FoldAccumulator,
    FoldIndex,
    Quantifier,
    Allocation,
}
#[derive(Clone, Debug)]
pub struct Binding {
    pub id: BindingId,
    pub name: String,
    pub ty: ScalarType,
    pub origin: BindingOrigin,
    pub state_dependent: bool,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub binding: BindingId,
    pub ty: ScalarType,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
    Neg,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Min,
    Max,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Xor,
}
impl BinaryOp {
    pub fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Mul => "mul",
            Self::Div => "div",
            Self::Rem => "rem",
            Self::Min => "min",
            Self::Max => "max",
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
            Self::And => "and",
            Self::Or => "or",
            Self::Xor => "xor",
        }
    }
    pub fn comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge
        )
    }
    pub fn boolean(self) -> bool {
        matches!(self, Self::And | Self::Or | Self::Xor)
    }
}

#[derive(Clone, Debug)]
pub struct TypedExpr {
    pub domain: Domain,
    pub kind: ExprKind,
    pub state_dependent: bool,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub enum ExprKind {
    Constant(Value),
    Binding(BindingId),
    Unary {
        op: UnaryOp,
        operand: ExprId,
    },
    Binary {
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    },
    Select {
        condition: ExprId,
        yes: ExprId,
        no: ExprId,
    },
    TableGet {
        column: ColumnId,
        key: ExprId,
        state: Snapshot,
    },
    SlotRead {
        column: ColumnId,
        key: ExprId,
        state: Snapshot,
    },
    TableExists {
        table: TableId,
        key: ExprId,
        state: Snapshot,
    },
    UniqueLookup {
        column: ColumnId,
        value: ExprId,
        state: Snapshot,
        exists_only: bool,
    },
    Sum {
        column: ColumnId,
        state: Snapshot,
    },
    KeyOf {
        table: TableId,
        value: ExprId,
    },
    Fold(Box<Fold>),
    EffectCount,
    EffectIs {
        effect: EffectId,
        index: ExprId,
    },
    EffectArg {
        effect: EffectId,
        index: ExprId,
        parameter: usize,
    },
}
#[derive(Clone, Debug)]
pub struct Fold {
    pub limit: u32,
    pub accumulator: BindingId,
    pub index: BindingId,
    pub init: ExprId,
    pub lo: ExprId,
    pub hi: ExprId,
    pub body: Vec<Stmt>,
    pub yield_value: ExprId,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub enum StmtKind {
    Let {
        binding: BindingId,
        value: ExprId,
    },
    Guard {
        condition: ExprId,
        code: String,
    },
    TableSet {
        column: ColumnId,
        key: ExprId,
        value: ExprId,
    },
    Allocate {
        table: TableId,
        binding: BindingId,
        fields: Vec<(ColumnId, ExprId)>,
    },
    Emit {
        effect: EffectId,
        args: Vec<ExprId>,
    },
    Eval(ExprId),
    Commit,
}

#[derive(Clone, Debug)]
pub struct NamedExpr {
    pub name: String,
    pub value: ExprId,
}
#[derive(Clone, Debug)]
pub struct Access {
    pub column: ColumnId,
    pub key: Option<ExprId>,
    pub tier: u8,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Transition {
    pub id: TransitionId,
    pub name: String,
    pub params: Vec<Param>,
    pub reads: Vec<Access>,
    pub writes: Vec<Access>,
    pub requires: Vec<NamedExpr>,
    pub ensures: Vec<NamedExpr>,
    pub body: Vec<Stmt>,
    pub wcet_bound: u64,
    pub fold_depth: u32,
    pub max_fold_product: u32,
    pub tier: u8,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Invariant {
    pub name: String,
    pub quantifier: Option<(TableId, BindingId)>,
    pub condition: ExprId,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Effect {
    pub id: EffectId,
    pub name: String,
    pub params: Vec<Param>,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Slice {
    pub column: ColumnId,
    pub row: Option<ExprId>,
}
#[derive(Clone, Debug)]
pub struct IntentGuard {
    pub transition: TransitionId,
    pub condition: ExprId,
}
#[derive(Clone, Debug)]
pub struct Projection {
    pub id: ProjectionId,
    pub name: String,
    pub params: Vec<Param>,
    pub allow_if: ExprId,
    pub expose: Vec<Slice>,
    pub enable_intents: Vec<IntentGuard>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct CheckedProgram {
    pub(crate) syntax: ast::Module,
    pub(crate) tables: Vec<Table>,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) expressions: Vec<TypedExpr>,
    pub(crate) transitions: Vec<Transition>,
    pub(crate) invariants: Vec<Invariant>,
    pub(crate) effects: Vec<Effect>,
    pub(crate) projections: Vec<Projection>,
    pub(crate) symbols: BTreeMap<u32, String>,
}
impl CheckedProgram {
    pub(crate) fn empty(syntax: ast::Module) -> Self {
        Self {
            syntax,
            tables: Vec::new(),
            bindings: Vec::new(),
            expressions: Vec::new(),
            transitions: Vec::new(),
            invariants: Vec::new(),
            effects: Vec::new(),
            projections: Vec::new(),
            symbols: BTreeMap::new(),
        }
    }
    pub fn syntax(&self) -> &ast::Module {
        &self.syntax
    }
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }
    pub fn transitions(&self) -> &[Transition] {
        &self.transitions
    }
    pub fn invariants(&self) -> &[Invariant] {
        &self.invariants
    }
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
    pub fn projections(&self) -> &[Projection] {
        &self.projections
    }
    pub fn expressions(&self) -> &[TypedExpr] {
        &self.expressions
    }
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
    pub fn table(&self, id: TableId) -> &Table {
        &self.tables[id.0]
    }
    pub fn column(&self, id: ColumnId) -> &Column {
        &self.tables[id.table.0].columns[id.index]
    }
    pub fn binding(&self, id: BindingId) -> &Binding {
        &self.bindings[id.0]
    }
    pub fn expr(&self, id: ExprId) -> &TypedExpr {
        &self.expressions[id.0]
    }
    pub fn transition(&self, id: TransitionId) -> &Transition {
        &self.transitions[id.0]
    }
    pub fn effect(&self, id: EffectId) -> &Effect {
        &self.effects[id.0]
    }
    pub fn projection(&self, id: ProjectionId) -> &Projection {
        &self.projections[id.0]
    }
    pub fn table_id(&self, name: &str) -> Option<TableId> {
        self.tables.iter().find(|t| t.name == name).map(|t| t.id)
    }
    pub fn transition_id(&self, name: &str) -> Option<TransitionId> {
        self.transitions
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.id)
    }
    pub fn effect_id(&self, name: &str) -> Option<EffectId> {
        self.effects.iter().find(|t| t.name == name).map(|t| t.id)
    }
    pub fn projection_id(&self, name: &str) -> Option<ProjectionId> {
        self.projections
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.id)
    }
}
