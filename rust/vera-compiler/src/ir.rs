//! Resolved, typed compiler contract. Only semantic analysis constructs programs.
use crate::{
    ast,
    diagnostic::Span,
    types::{ExprType, TableLayout, ValueType},
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
    ComponentId,
    OperationId,
    TableId,
    ExprId,
    BindingId,
    TransitionId,
    EffectId,
    ProjectionId,
    FunctionId,
    TypeId,
    FieldId,
    VariantId
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
    pub ty: ValueType,
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
    pub ty: ValueType,
    pub origin: BindingOrigin,
    pub state_dependent: bool,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub binding: BindingId,
    pub ty: ValueType,
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
    pub domain: ExprType,
    pub kind: ExprKind,
    pub state_dependent: bool,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub enum ExprKind {
    BytesLen {
        value: ExprId,
    },
    BytesGet {
        value: ExprId,
        index: ExprId,
    },
    BytesSlice {
        value: ExprId,
        start: ExprId,
        count: ExprId,
        capacity: u64,
    },
    BytesConcat {
        left: ExprId,
        right: ExprId,
        capacity: u64,
    },
    BytesResize {
        value: ExprId,
        capacity: u64,
    },
    Encode {
        value: ExprId,
        ty: ValueType,
    },
    CanDecode {
        value: ExprId,
        ty: ValueType,
    },
    Decode {
        value: ExprId,
        ty: ValueType,
    },
    Record {
        ty: TypeId,
        fields: Vec<(FieldId, ExprId)>,
    },
    Field {
        record: ExprId,
        field: FieldId,
    },
    Variant {
        ty: TypeId,
        variant: VariantId,
        payload: ExprId,
    },
    IsVariant {
        value: ExprId,
        variant: VariantId,
    },
    Payload {
        value: ExprId,
        variant: VariantId,
    },
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
    Call {
        function: FunctionId,
        args: Vec<ExprId>,
    },
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
    OperationCall {
        operation: OperationId,
        args: Vec<ExprId>,
        reads: Vec<Access>,
        writes: Vec<Access>,
    },
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
    pub owner: Option<ComponentId>,
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

/// Exclusive state ownership and the invariants justified at its public boundary.
#[derive(Clone, Debug)]
pub struct Component {
    pub id: ComponentId,
    pub name: String,
    pub tables: Vec<TableId>,
    pub invariants: Vec<usize>,
}
/// Stateful leaf helper. Declared frames are retained independently of actual
/// accesses: caller abstraction must never use body details as a contract.
#[derive(Clone, Debug)]
pub struct Operation {
    pub id: OperationId,
    pub component: ComponentId,
    pub transition: Transition,
    pub reads: Vec<Access>,
    pub writes: Vec<Access>,
    pub effects: Vec<EffectId>,
}

#[derive(Clone, Debug)]
pub struct CheckedProgram {
    pub(crate) syntax: ast::Module,
    pub(crate) components: Vec<Component>,
    pub(crate) operations: Vec<Operation>,
    pub(crate) tables: Vec<Table>,
    pub(crate) types: Vec<TypeDefinition>,
    pub(crate) type_layouts: BTreeMap<TypeId, (usize, usize, u64)>,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) expressions: Vec<TypedExpr>,
    pub(crate) transitions: Vec<Transition>,
    pub(crate) functions: Vec<Function>,
    pub(crate) invariants: Vec<Invariant>,
    pub(crate) effects: Vec<Effect>,
    pub(crate) projections: Vec<Projection>,
    pub(crate) symbols: BTreeMap<u32, String>,
}
impl CheckedProgram {
    pub(crate) fn empty(syntax: ast::Module) -> Self {
        Self {
            syntax,
            components: Vec::new(),
            operations: Vec::new(),
            tables: Vec::new(),
            types: Vec::new(),
            type_layouts: BTreeMap::new(),
            bindings: Vec::new(),
            expressions: Vec::new(),
            transitions: Vec::new(),
            functions: Vec::new(),
            invariants: Vec::new(),
            effects: Vec::new(),
            projections: Vec::new(),
            symbols: BTreeMap::new(),
        }
    }
    pub fn syntax(&self) -> &ast::Module {
        &self.syntax
    }
    pub fn components(&self) -> &[Component] {
        &self.components
    }
    pub fn component(&self, id: ComponentId) -> &Component {
        &self.components[id.0]
    }
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }
    pub fn operation(&self, id: OperationId) -> &Operation {
        &self.operations[id.0]
    }
    pub fn operation_id(&self, name: &str) -> Option<OperationId> {
        self.operations
            .iter()
            .find(|o| o.transition.name == name)
            .map(|o| o.id)
    }
    pub fn table_owner(&self, id: TableId) -> Option<ComponentId> {
        self.components
            .iter()
            .find(|c| c.tables.contains(&id))
            .map(|c| c.id)
    }
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }
    pub fn transitions(&self) -> &[Transition] {
        &self.transitions
    }
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }
    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id.0]
    }
    pub fn function_id(&self, name: &str) -> Option<FunctionId> {
        self.functions.iter().find(|f| f.name == name).map(|f| f.id)
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

/// Pure, acyclic functions appear in dependency-topological order.
#[derive(Clone, Debug)]
pub struct Function {
    pub id: FunctionId,
    pub name: String,
    pub params: Vec<Param>,
    pub result: BindingId,
    pub result_type: ValueType,
    pub requires: Vec<NamedExpr>,
    pub ensures: Vec<NamedExpr>,
    pub body: ExprId,
    pub dependencies: Vec<FunctionId>,
    pub wcet_bound: u64,
    pub fold_depth: u32,
    pub max_fold_product: u32,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeDefinition {
    pub id: TypeId,
    pub name: String,
    pub kind: TypeDefinitionKind,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub enum TypeDefinitionKind {
    Record(Vec<FieldDefinition>),
    Union(Vec<VariantDefinition>),
}
#[derive(Clone, Debug)]
pub struct FieldDefinition {
    pub id: FieldId,
    pub name: String,
    pub ty: ValueType,
    pub span: Span,
}
#[derive(Clone, Debug)]
pub struct VariantDefinition {
    pub id: VariantId,
    pub name: String,
    pub payload: ValueType,
    pub span: Span,
}
impl CheckedProgram {
    pub fn types(&self) -> &[TypeDefinition] {
        &self.types
    }
    pub fn type_definition(&self, id: TypeId) -> &TypeDefinition {
        &self.types[id.0]
    }
    pub fn type_id(&self, name: &str) -> Option<TypeId> {
        self.types.iter().find(|t| t.name == name).map(|t| t.id)
    }
    pub fn value_type_name(&self, ty: &ValueType) -> String {
        match ty {
            ValueType::Scalar(t) => t.to_string(),
            ValueType::Bytes { capacity } => format!("bytes<{capacity}>"),
            ValueType::Named(id) => self.type_definition(*id).name.clone(),
        }
    }
    pub fn value_wire_width(&self, ty: &ValueType) -> Result<usize, String> {
        if let ValueType::Named(id) = ty {
            if let Some(layout) = self.type_layouts.get(id) {
                return Ok(layout.0);
            }
        }
        match ty {
            ValueType::Scalar(t) => Ok(t.byte_width()),
            ValueType::Bytes { capacity } => usize::try_from(*capacity)
                .ok()
                .and_then(|n| n.checked_add(8))
                .ok_or_else(|| "bytes canonical wire width exceeds usize".into()),
            ValueType::Named(id) => match &self.type_definition(*id).kind {
                TypeDefinitionKind::Record(fields) => fields.iter().try_fold(0usize, |n, f| {
                    n.checked_add(self.value_wire_width(&f.ty)?)
                        .ok_or_else(|| "record wire width exceeds usize".into())
                }),
                TypeDefinitionKind::Union(variants) => variants
                    .iter()
                    .map(|v| self.value_wire_width(&v.payload))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .max()
                    .unwrap_or(0)
                    .checked_add(4)
                    .ok_or_else(|| "union wire width exceeds usize".into()),
            },
        }
    }
    pub fn value_alignment(&self, ty: &ValueType) -> Result<u64, String> {
        if let ValueType::Named(id) = ty {
            if let Some(layout) = self.type_layouts.get(id) {
                return Ok(layout.2);
            }
        }
        match ty {
            ValueType::Bytes { .. } => Ok(8),
            ValueType::Scalar(crate::types::ScalarType::Bytes32) => Ok(1),
            ValueType::Scalar(t) => Ok(t.align()),
            ValueType::Named(id) => match &self.type_definition(*id).kind {
                TypeDefinitionKind::Record(fields) => Ok(fields
                    .iter()
                    .map(|f| self.value_alignment(&f.ty))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .max()
                    .unwrap_or(1)),
                TypeDefinitionKind::Union(variants) => Ok(variants
                    .iter()
                    .map(|v| self.value_alignment(&v.payload))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .max()
                    .unwrap_or(1)
                    .max(4)),
            },
        }
    }
    pub fn value_storage_width(&self, ty: &ValueType) -> Result<usize, String> {
        if let ValueType::Named(id) = ty {
            if let Some(layout) = self.type_layouts.get(id) {
                return Ok(layout.1);
            }
        }
        fn align(n: usize, a: usize) -> Result<usize, String> {
            n.checked_add(a - 1)
                .map(|n| n / a * a)
                .ok_or_else(|| "type layout exceeds usize".into())
        }
        match ty {
            ValueType::Bytes { .. } => align(self.value_wire_width(ty)?, 8),
            ValueType::Scalar(t) => Ok(t.byte_width()),
            ValueType::Named(id) => {
                let alignment = usize::try_from(self.value_alignment(ty)?)
                    .map_err(|_| "alignment exceeds usize")?;
                let width = match &self.type_definition(*id).kind {
                    TypeDefinitionKind::Record(fields) => {
                        fields.iter().try_fold(0usize, |n, f| {
                            align(n, self.value_alignment(&f.ty)? as usize)?
                                .checked_add(self.value_storage_width(&f.ty)?)
                                .ok_or_else(|| "record layout exceeds usize".to_string())
                        })?
                    }
                    TypeDefinitionKind::Union(variants) => align(4, alignment)?
                        .checked_add(
                            variants
                                .iter()
                                .map(|v| self.value_storage_width(&v.payload))
                                .collect::<Result<Vec<_>, _>>()?
                                .into_iter()
                                .max()
                                .unwrap_or(0),
                        )
                        .ok_or("union layout exceeds usize")?,
                };
                align(width, alignment)
            }
        }
    }
}
