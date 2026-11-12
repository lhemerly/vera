//! Inductive verification over the complete finite state and checked expressions.
//!
//! The universal queries use an exact finite table model. Unique indexes are
//! abstracted by their logical row relation: this admits additional committing
//! executions when a physical index would saturate, so UNSAT remains sound.
//! Row-local invariants use a frame rule over the complete mutation footprint;
//! global and cross-row specifications still use the complete finite state.
//! Every SAT answer is admitted only after concrete state validation and replay.

#[path = "smt/codecs.rs"]
mod codecs;
#[path = "smt/terms.rs"]
mod terms;

use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{catch_unwind, AssertUnwindSafe},
    time::Instant,
};

use num_bigint::BigInt;
use num_traits::{One, ToPrimitive};
use serde::Serialize;
use z3::{
    ast::{Ast, Bool, Dynamic, BV},
    SatResult, Solver,
};

use crate::{
    diagnostic::{Diagnostic, Span},
    ir::*,
    runtime::{
        eval_expression, rebuild_indexes, recompute_aggregates, validate_state, FailureKind,
        Machine, RuntimeError, State, TraceStep,
    },
    types::{Domain, ExprType, ScalarType, ValueType},
    value::{coerce_value, value_to_json, zero_value, Value},
};
use terms::{
    and, arithmetic, boolean, bv, cmp, eq, extend, fits, implies, ite, not, or, truncate, Path,
    Script, Sort, Term,
};

/// Optional, explicitly selected diagnostic budgets. None imposes no policy cap.
/// Physical/numeric representation limits still apply; capacities are never reduced.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VerifierConfig {
    pub timeout_ms: Option<u32>,
    pub max_state_cells: Option<usize>,
    pub max_unroll: Option<u32>,
    pub witness_attempts: Option<usize>,
    /// Maximum expression visits during symbolic lowering, independent of capacity.
    pub max_expression_steps: Option<usize>,
    /// Maximum UTF-8 bytes retained in one generated SMT query.
    pub max_smt_bytes: Option<usize>,
    /// Maximum candidate effects, including inactive unrolled emissions.
    pub max_effect_events: Option<usize>,
}

/// Compatibility spelling for callers configuring SMT verification.
pub type SmtConfig = VerifierConfig;

#[derive(Clone, Debug, Serialize)]
pub struct ObligationResult {
    pub name: String,
    pub status: String,
    pub elapsed_ms: f64,
    pub reason: Option<String>,
}

impl ObligationResult {
    fn new(name: &str, status: &str, elapsed_ms: f64, reason: Option<String>) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
            elapsed_ms,
            reason,
        }
    }
}

/// A concrete, independently replayed witness. The full pre-state preserves
/// generations and physical values even for unoccupied slots.
#[derive(Clone, Debug, Serialize)]
pub struct Counterexample {
    pub target_kind: String,
    pub return_value: Option<Value>,
    /// Source-named canonical JSON for an independently replayed function return.
    pub typed_return_value: Option<EffectArgument>,
    pub transition: String,
    pub obligation: String,
    pub law: String,
    pub params: BTreeMap<String, Value>,
    pub pre_state: State,
    pub trace: Vec<TraceStep>,
    pub failure: Option<RuntimeError>,
    pub violated: Vec<String>,
    /// Only effects committed by this replay, in transaction-local order.
    pub emitted_effects: Vec<EmittedEffect>,
    /// Evaluated failures, not merely symbolic claims. Liveness witnesses can
    /// also fail ensures: committing alone is not a correctness guarantee.
    pub failed_ensures: Vec<FailedEnsure>,
    pub directive: String,
}

/// A payload value with source-level type identity. Numeric/decimal values use
/// exact raw/scaled JSON integers; bytes32 uses canonical little-endian hex.
#[derive(Clone, Debug, Serialize)]
pub struct EffectArgument {
    #[serde(rename = "type")]
    pub scalar_type: String,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct EmittedEffect {
    pub index: usize,
    pub effect: String,
    pub arguments: BTreeMap<String, EffectArgument>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnsureOutcome {
    Value { value: Value },
    Error { error: RuntimeError },
}

#[derive(Clone, Debug, Serialize)]
pub struct FailedEnsure {
    pub name: String,
    pub span: Span,
    pub outcome: EnsureOutcome,
}

/// Observed encoding sizes, not process-memory or solver-time bounds.
#[derive(Clone, Debug, Serialize)]
pub struct EncodingMetrics {
    /// Complete finite model: occupancy, generation and column cells.
    pub state_cells: usize,
    /// Actual symbolic expression visits, including repeated fold visits.
    pub expression_steps: usize,
    /// Candidate emissions, including inactive unrolled paths.
    pub effect_events: usize,
    /// Retained declarations and assertions, including SMT wrappers/newlines.
    pub retained_smt_bytes: usize,
    /// Largest exported obligation query, including witness-blocking clauses.
    pub max_query_bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerificationResult {
    pub target_kind: String,
    /// Flat dependency evidence; never recursively duplicate a proof DAG.
    pub contract_dependencies: Vec<ContractDependency>,
    pub transition: String,
    pub config: VerifierConfig,
    /// None when a complete encoding could not be constructed.
    pub encoding_metrics: Option<EncodingMetrics>,
    pub safety: ObligationResult,
    pub panic: ObligationResult,
    pub liveness: ObligationResult,
    pub smt_safety: String,
    pub smt_panic: String,
    pub smt_liveness: String,
    pub diagnostics: Vec<Diagnostic>,
    pub counterexample: Option<Counterexample>,
    pub witness: Option<Counterexample>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContractDependency {
    pub function: String,
    pub safety: String,
    pub panic: String,
    pub liveness: String,
    pub ok: bool,
}

impl VerificationResult {
    pub fn ok(&self) -> bool {
        self.diagnostics.is_empty()
            && self.safety.status == "unsat"
            && self.panic.status == "unsat"
            && self.liveness.status == "sat"
    }

    pub fn summary(&self) -> String {
        let mut out = format!("{} {}:", self.target_kind, self.transition);
        for (index, result) in [&self.safety, &self.panic, &self.liveness]
            .into_iter()
            .enumerate()
        {
            out.push_str(&format!(
                "\n  Obligation {} ({}): {} [{:.1} ms]",
                index + 1,
                result.name,
                result.status.to_uppercase(),
                result.elapsed_ms
            ));
            if let Some(reason) = &result.reason {
                out.push_str(&format!(" ({reason})"));
            }
        }
        for diagnostic in &self.diagnostics {
            out.push_str(&format!("\n  {}: {}", diagnostic.code, diagnostic.message));
        }
        out
    }
}

#[derive(Clone)]
struct Row {
    occupied: Term,
    generation: Term,
    columns: Vec<Term>,
}

#[derive(Clone)]
struct SymbolicState {
    tables: Vec<Vec<Row>>,
    aggregates: Vec<Vec<Option<Term>>>,
    writes: Vec<Write>,
    touched: BTreeMap<TableId, Vec<(Term, String)>>,
}

#[derive(Clone)]
struct Write {
    column: ColumnId,
    key: Term,
    value: Term,
    active: String,
}

#[derive(Clone)]
enum Location {
    Parameter(String, ValueType),
    Occupied(TableId, usize),
    Generation(TableId, usize),
    Column(ColumnId, usize, ValueType),
}

#[derive(Clone)]
struct Projected {
    term: Term,
    location: Location,
}

struct Encoding<'a> {
    program: &'a CheckedProgram,
    transition: Transition,
    target_function: Option<FunctionId>,
    verified_functions: &'a BTreeSet<FunctionId>,
    declared_functions: BTreeSet<FunctionId>,
    summary_calls: usize,
    config: &'a VerifierConfig,
    script: Script,
    pre: SymbolicState,
    current: SymbolicState,
    env: BTreeMap<BindingId, Term>,
    path: Path,
    pre_context: bool,
    projections: Vec<Projected>,
    safety: String,
    panic: String,
    liveness: String,
    work: usize,
    state_cells: usize,
    /// A quantified key constructed from an enumerated physical slot.
    known_slot: Option<(TableId, String, usize)>,
    effect_events: usize,
    effect_tags: String,
    effect_args: BTreeMap<(EffectId, usize), String>,
    effect_count: Term,
    codecs: BTreeMap<ValueType, usize>,
    codec_encoders: BTreeSet<usize>,
    codec_decoders: BTreeSet<usize>,
    codec_padding_helpers: BTreeSet<usize>,
    zero_padding_declared: bool,
}

type LowerResult<T> = Result<T, Diagnostic>;

fn diagnostic(code: &str, message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        message: message.into(),
        span,
    }
}

fn sort(domain: &ExprType) -> Sort {
    match domain {
        ExprType::Bytes { .. } => Sort::Bytes,
        ExprType::Named(id) => Sort::Named(*id),
        ExprType::Scalar(domain) if domain.scalar.is_bool() => Sort::Bool,
        ExprType::Scalar(domain) => Sort::Bits(u32::from(domain.width)),
    }
}
fn value_sort(ty: &ValueType) -> Sort {
    match ty {
        ValueType::Bytes { .. } => Sort::Bytes,
        ValueType::Named(id) => Sort::Named(*id),
        ValueType::Scalar(ty) if ty.is_bool() => Sort::Bool,
        ValueType::Scalar(ty) => Sort::Bits(u32::from(ty.bits())),
    }
}
fn expr_signed(domain: &ExprType) -> bool {
    domain.as_scalar().is_some_and(ScalarType::signed)
}
fn value_signed(ty: &ValueType) -> bool {
    ty.as_scalar().is_some_and(ScalarType::signed)
}
fn scalar_domain(domain: &ExprType) -> &Domain {
    domain.scalar_domain().expect("resolved scalar operation")
}
fn constructor(ty: TypeId, variant: Option<VariantId>) -> String {
    variant.map_or_else(
        || format!("record_{}", ty.0),
        |v| format!("variant_{}_{}", ty.0, v.0),
    )
}
fn selector(ty: TypeId, field: usize) -> String {
    format!("field_{}_{}", ty.0, field)
}
fn payload_selector(ty: TypeId, variant: VariantId) -> String {
    format!("payload_{}_{}", ty.0, variant.0)
}
fn application(name: &str, args: &[String]) -> String {
    if args.is_empty() {
        name.to_owned()
    } else {
        format!("({name} {})", args.join(" "))
    }
}
fn valid_value(ty: &ValueType, text: &str) -> String {
    match ty {
        ValueType::Bytes { capacity } => format!("(<= (seq.len {text}) {capacity})"),
        ValueType::Named(id) => format!("(valid_{} {text})", id.0),
        ValueType::Scalar(ScalarType::Enum(tags)) if tags.len() < 256 => {
            cmp("lt", text, &bv(tags.len(), 8), false)
        }
        _ => "true".into(),
    }
}
fn zero_term(ty: &ValueType) -> String {
    match ty {
        ValueType::Bytes { .. } => codecs::empty(),
        ValueType::Named(id) => format!("zero_{}", id.0),
        ValueType::Scalar(ty) if ty.is_bool() => "false".into(),
        ValueType::Scalar(ty) => bv(0, u32::from(ty.bits())),
    }
}
fn value_literal(program: &CheckedProgram, value: &Value, ty: &ValueType) -> String {
    match (value, ty) {
        (Value::Bytes(value), _) => codecs::concat(
            value
                .iter()
                .map(|b| format!("(seq.unit {})", bv(*b, 8)))
                .collect(),
        ),
        (Value::Bool(value), _) => boolean(*value),
        (Value::Int(value), ValueType::Scalar(ty)) => bv(value.clone(), u32::from(ty.bits())),
        (Value::Record { ty, fields }, _) => {
            let TypeDefinitionKind::Record(defs) = &program.type_definition(*ty).kind else {
                unreachable!("typed record")
            };
            application(
                &constructor(*ty, None),
                &fields
                    .iter()
                    .zip(defs)
                    .map(|(v, d)| value_literal(program, v, &d.ty))
                    .collect::<Vec<_>>(),
            )
        }
        (Value::Variant { ty, tag, payload }, _) => {
            let TypeDefinitionKind::Union(defs) = &program.type_definition(*ty).kind else {
                unreachable!("typed variant")
            };
            application(
                &constructor(*ty, Some(*tag)),
                &[value_literal(program, payload, &defs[tag.0].payload)],
            )
        }
        _ => unreachable!("typed scalar literal"),
    }
}

impl<'a> Encoding<'a> {
    fn new(
        program: &'a CheckedProgram,
        transition: &Transition,
        config: &'a VerifierConfig,
        verified_functions: &'a BTreeSet<FunctionId>,
        target_function: Option<FunctionId>,
    ) -> LowerResult<Self> {
        let tables = if target_function.is_some() {
            &[][..]
        } else {
            program.tables()
        };
        let cells = tables
            .iter()
            .try_fold(0_usize, |total, table| {
                let capacity = usize::try_from(table.capacity).ok()?;
                total.checked_add(capacity.checked_mul(table.columns.len().checked_add(2)?)?)
            })
            .ok_or_else(|| {
                diagnostic(
                    "D_VERIFY_RESOURCE",
                    "state model size exceeds the host address space",
                    transition.span,
                )
            })?;
        if let Some(limit) = config.max_state_cells.filter(|limit| cells > *limit) {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                format!(
                    "complete state requires {cells} cells; max_state_cells is {}",
                    limit
                ),
                transition.span,
            ));
        }
        if let Some(limit) = config
            .max_unroll
            .filter(|limit| transition.max_fold_product > *limit)
        {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                format!(
                    "fold product {} exceeds max_unroll {}",
                    transition.max_fold_product, limit
                ),
                transition.span,
            ));
        }
        let mut this = Self {
            program,
            transition: transition.clone(),
            target_function,
            verified_functions,
            declared_functions: BTreeSet::new(),
            summary_calls: 0,
            config,
            script: Script::with_budget(config.max_smt_bytes),
            pre: SymbolicState {
                tables: Vec::new(),
                aggregates: Vec::new(),
                writes: Vec::new(),
                touched: BTreeMap::new(),
            },
            current: SymbolicState {
                tables: Vec::new(),
                aggregates: Vec::new(),
                writes: Vec::new(),
                touched: BTreeMap::new(),
            },
            env: BTreeMap::new(),
            path: Path::default(),
            pre_context: false,
            projections: Vec::new(),
            safety: String::new(),
            panic: String::new(),
            liveness: String::new(),
            work: 0,
            state_cells: cells,
            known_slot: None,
            effect_events: 0,
            effect_tags: format!(
                "((as const (Array (_ BitVec 64) (_ BitVec 64))) {})",
                bv(0, 64)
            ),
            effect_args: BTreeMap::new(),
            effect_count: Term::bits(bv(0, 64), 64),
            codecs: BTreeMap::new(),
            codec_encoders: BTreeSet::new(),
            codec_decoders: BTreeSet::new(),
            codec_padding_helpers: BTreeSet::new(),
            zero_padding_declared: false,
        };
        this.declare_types()?;
        // Sequence operations and their integer lengths require a general logic.
        if program
            .expressions()
            .iter()
            .any(|e| matches!(e.domain, ExprType::Bytes { .. }))
        {
            this.script.enable_datatypes();
        }
        for table in tables {
            let mut rows = Vec::new();
            for slot in 0..table.capacity as usize {
                this.check_script_budget(table.span)?;
                let occupied = this.input(
                    &format!("pre_t{}_s{slot}_occupied", table.id.0),
                    Sort::Bool,
                    Location::Occupied(table.id, slot),
                );
                let generation = this.input(
                    &format!("pre_t{}_s{slot}_generation", table.id.0),
                    Sort::Bits(table.generation_bits()),
                    Location::Generation(table.id, slot),
                );
                let mut columns = Vec::new();
                for column in &table.columns {
                    let term = this.input(
                        &format!("pre_t{}_s{slot}_c{}", table.id.0, column.id.index),
                        value_sort(&column.ty),
                        Location::Column(column.id, slot, column.ty.clone()),
                    );
                    this.constrain_value(&term, &column.ty);
                    let zero = zero_term(&column.ty);
                    this.script
                        .assert(implies(&not(&occupied.text), &eq(&term.text, &zero)));
                    columns.push(term);
                }
                rows.push(Row {
                    occupied,
                    generation,
                    columns,
                });
            }
            for column in table.columns.iter().filter(|column| column.unique) {
                // An inverse map enforces distinct live values with one clause
                // per row, avoiding quadratic pairwise inequality expansion.
                let inverse = format!("pre_t{}_c{}_inverse", table.id.0, column.id.index);
                this.script.declaration(format!(
                    "(declare-const {inverse} (Array {} (_ BitVec 64)))",
                    value_sort(&column.ty).smt()
                ));
                for (slot, row) in rows.iter().enumerate() {
                    this.check_script_budget(column.span)?;
                    this.script.assert(implies(
                        &row.occupied.text,
                        &eq(
                            &format!("(select {inverse} {})", row.columns[column.id.index].text),
                            &bv(slot, 64),
                        ),
                    ));
                }
            }
            let mut aggregates = Vec::new();
            for column in &table.columns {
                aggregates.push(if column.tracked_sum {
                    let width = u32::from(
                        column
                            .ty
                            .as_scalar()
                            .expect("tracked scalar")
                            .aggregate_width(),
                    );
                    let mut total = bv(0, width);
                    // Unoccupied physical columns are constrained to zero by
                    // the state schema, so this is precisely the live-row sum.
                    for row in &rows {
                        let value = &row.columns[column.id.index];
                        this.check_script_budget(column.span)?;
                        total = this
                            .script
                            .bind(
                                "sum_prefix",
                                format!(
                                    "(bvadd {total} {})",
                                    extend(
                                        &value.text,
                                        value.width(),
                                        width,
                                        value_signed(&column.ty)
                                    )
                                ),
                                Sort::Bits(width),
                            )
                            .text;
                    }
                    Some(this.script.bind("aggregate", total, Sort::Bits(width)))
                } else {
                    None
                });
            }
            this.pre.tables.push(rows);
            this.pre.aggregates.push(aggregates);
        }
        this.current = this.pre.clone();
        for parameter in &transition.params {
            let term = this.input(
                &format!("p_{}", parameter.binding.0),
                value_sort(&parameter.ty),
                Location::Parameter(parameter.name.clone(), parameter.ty.clone()),
            );
            this.constrain_value(&term, &parameter.ty);
            this.env.insert(parameter.binding, term);
        }
        let pre_invariants = if target_function.is_some() {
            "true".into()
        } else {
            this.invariants(true)?
        };
        this.script.assert(pre_invariants);
        for requirement in &transition.requires {
            let condition = this.expr(requirement.value)?;
            this.script.guard(&mut this.path, &condition.text);
        }
        this.statements(&transition.body)?;
        let mut postconditions = vec![if target_function.is_some() {
            "true".into()
        } else {
            this.invariants(false)?
        }];
        for ensure in &transition.ensures {
            postconditions.push(this.spec(ensure.value, false)?);
        }
        this.safety = and([this.path.live.clone(), not(&and(postconditions))]);
        this.panic = this.path.trapped.clone();
        this.liveness = this.path.live.clone();
        this.check_script_budget(transition.span)?;
        // Reserve the wrapper and final obligation before constructing a query.
        let predicate_bytes = [&this.safety, &this.panic, &this.liveness]
            .into_iter()
            .map(|value| value.len())
            .max()
            .unwrap_or(0);
        if config.max_smt_bytes.is_some_and(|limit| {
            this.script
                .bytes()
                .saturating_add(predicate_bytes)
                .saturating_add(128)
                > limit
        }) {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                "SMT query exceeds max_smt_bytes",
                transition.span,
            ));
        }
        Ok(this)
    }

    fn check_script_budget(&self, span: Span) -> LowerResult<()> {
        if self.script.exceeded() {
            Err(diagnostic(
                "D_VERIFY_RESOURCE",
                "SMT encoding exceeds max_smt_bytes",
                span,
            ))
        } else {
            Ok(())
        }
    }

    fn input(&mut self, name: &str, kind: Sort, location: Location) -> Term {
        if kind == Sort::Bytes {
            self.script.enable_datatypes();
        }
        let term = self.script.declare(name, kind);
        self.projections.push(Projected {
            term: term.clone(),
            location,
        });
        term
    }

    /// Datatypes encode products/sums directly. Shared validity predicates
    /// constrain enum leaves without Cartesian expansion of variants.
    fn declare_types(&mut self) -> LowerResult<()> {
        if self.program.types().is_empty() {
            return Ok(());
        }
        self.script.enable_datatypes();
        let headers = self
            .program
            .types()
            .iter()
            .map(|d| format!("(type_{} 0)", d.id.0))
            .collect::<Vec<_>>()
            .join(" ");
        let mut definitions = Vec::new();
        for definition in self.program.types() {
            let id = definition.id;
            let constructors = match &definition.kind {
                TypeDefinitionKind::Record(fields) => vec![format!(
                    "({} {})",
                    constructor(id, None),
                    fields
                        .iter()
                        .map(|f| format!("({} {})", selector(id, f.id.0), value_sort(&f.ty).smt()))
                        .collect::<Vec<_>>()
                        .join(" ")
                )],
                TypeDefinitionKind::Union(variants) => variants
                    .iter()
                    .map(|v| {
                        format!(
                            "({} ({} {}))",
                            constructor(id, Some(v.id)),
                            payload_selector(id, v.id),
                            value_sort(&v.payload).smt()
                        )
                    })
                    .collect(),
            };
            definitions.push(format!("({})", constructors.join(" ")));
        }
        self.script.declaration(format!(
            "(declare-datatypes ({headers}) ({}))",
            definitions.join(" ")
        ));
        // The semantic arena rejects by-value cycles. Emit helpers in dependency
        // order even when declarations refer forward in the source file.
        let mut done = BTreeSet::new();
        while done.len() < self.program.types().len() {
            let before = done.len();
            for definition in self.program.types() {
                if done.contains(&definition.id) {
                    continue;
                }
                let children = match &definition.kind {
                    TypeDefinitionKind::Record(fields) => {
                        fields.iter().map(|f| &f.ty).collect::<Vec<_>>()
                    }
                    TypeDefinitionKind::Union(variants) => {
                        variants.iter().map(|v| &v.payload).collect()
                    }
                };
                if children
                    .iter()
                    .any(|ty| matches!(ty,ValueType::Named(id) if !done.contains(id)))
                {
                    continue;
                }
                let id = definition.id;
                let (zero, valid) = match &definition.kind {
                    TypeDefinitionKind::Record(fields) => (
                        application(
                            &constructor(id, None),
                            &fields.iter().map(|f| zero_term(&f.ty)).collect::<Vec<_>>(),
                        ),
                        and(fields
                            .iter()
                            .map(|f| valid_value(&f.ty, &format!("({} x)", selector(id, f.id.0))))),
                    ),
                    TypeDefinitionKind::Union(variants) => {
                        let first = variants.first().expect("nonempty checked union");
                        (
                            application(
                                &constructor(id, Some(first.id)),
                                &[zero_term(&first.payload)],
                            ),
                            or(variants.iter().map(|v| {
                                and([
                                    format!("((_ is {}) x)", constructor(id, Some(v.id))),
                                    valid_value(
                                        &v.payload,
                                        &format!("({} x)", payload_selector(id, v.id)),
                                    ),
                                ])
                            })),
                        )
                    }
                };
                self.script.declaration(format!(
                    "(define-fun zero_{} () type_{} {zero})",
                    id.0, id.0
                ));
                self.script.declaration(format!(
                    "(define-fun valid_{} ((x type_{})) Bool {valid})",
                    id.0, id.0
                ));
                done.insert(id);
                self.check_script_budget(definition.span)?;
            }
            if done.len() == before {
                return Err(diagnostic(
                    "D_VERIFY_LOWER",
                    "cyclic structured type reached verifier",
                    self.transition.span,
                ));
            }
        }
        Ok(())
    }

    fn constrain_value(&mut self, term: &Term, ty: &ValueType) {
        self.script.assert(valid_value(ty, &term.text));
    }

    fn state(&self, snapshot: Snapshot) -> &SymbolicState {
        if snapshot == Snapshot::Pre || self.pre_context {
            &self.pre
        } else {
            &self.current
        }
    }

    fn coerce(&self, value: &Term, domain: &ExprType, signed: bool) -> Term {
        if value.sort == Sort::Bool || matches!(domain, ExprType::Named(_) | ExprType::Bytes { .. })
        {
            return value.clone();
        }
        let width = u32::from(scalar_domain(domain).width);
        Term::bits(
            if value.width() > width {
                truncate(&value.text, value.width(), width)
            } else {
                extend(&value.text, value.width(), width, signed)
            },
            width,
        )
    }

    fn bind(&mut self, value: Term) -> Term {
        self.script.bind("value", value.text, value.sort)
    }

    fn narrow(&mut self, value: Term, target: &ValueType) -> Term {
        if matches!(target, ValueType::Named(_) | ValueType::Bytes { .. }) {
            self.script
                .trap_if(&mut self.path, &not(&valid_value(target, &value.text)));
            return value;
        }
        let ty = target.as_scalar().expect("scalar narrowing");
        if ty.is_bool() {
            return value;
        }
        let width = u32::from(ty.bits());
        self.script.trap_if(
            &mut self.path,
            &not(&fits(&value.text, value.width(), width, ty.signed())),
        );
        self.coerce(
            &value,
            &ExprType::Scalar(Domain::scalar(ty.clone())),
            ty.signed(),
        )
    }

    fn expr_at(&mut self, id: ExprId, snapshot: Snapshot) -> LowerResult<Term> {
        let saved = self.pre_context;
        self.pre_context = saved || snapshot == Snapshot::Pre;
        let value = self.expr(id);
        self.pre_context = saved;
        value
    }

    fn key(&self, table: TableId, slot: usize, generation: &Term) -> String {
        let bits = self.program.table(table).key_bits();
        let shifted = if bits == 0 {
            extend(&generation.text, generation.width(), 64, false)
        } else {
            format!(
                "(bvshl {} {})",
                extend(&generation.text, generation.width(), 64, false),
                bv(bits, 64)
            )
        };
        format!("(bvor {shifted} {})", bv(slot, 64))
    }

    fn known_slot(&self, table: TableId, key: &str) -> Option<usize> {
        self.known_slot
            .as_ref()
            .and_then(|(known_table, known_key, slot)| {
                (*known_table == table && known_key == key).then_some(*slot)
            })
    }

    fn slot(&self, table: TableId, key: &str) -> String {
        if let Some(slot) = self.known_slot(table, key) {
            return bv(slot, 64);
        }
        let bits = self.program.table(table).key_bits();
        if bits == 0 {
            bv(0, 64)
        } else {
            format!(
                "(bvand {key} {})",
                bv(self.program.table(table).capacity - 1, 64)
            )
        }
    }

    fn read_column(&self, column: ColumnId, key: &str, snapshot: Snapshot) -> Term {
        let slot = self.slot(column.table, key);
        let rows = &self.pre.tables[column.table.0];
        let mut value = if let Some(index) = self.known_slot(column.table, key) {
            rows[index].columns[column.index].clone()
        } else {
            let mut value = rows[0].columns[column.index].clone();
            for (index, row) in rows.iter().enumerate().skip(1).rev() {
                value.text = ite(
                    &eq(&slot, &bv(index, 64)),
                    &row.columns[column.index].text,
                    &value.text,
                );
            }
            value
        };
        for write in self
            .state(snapshot)
            .writes
            .iter()
            .filter(|write| write.column == column)
        {
            let hit = and([
                write.active.clone(),
                eq(&slot, &self.slot(column.table, &write.key.text)),
            ]);
            value.text = ite(&hit, &write.value.text, &value.text);
        }
        value
    }

    fn exists(&self, table: TableId, key: &str, snapshot: Snapshot) -> String {
        if let Some(slot) = self.known_slot(table, key) {
            let row = &self.state(snapshot).tables[table.0][slot];
            // A known slot is not proof of existence: retain full generation
            // equality and occupancy, including when an old snapshot is read.
            return and([
                row.occupied.text.clone(),
                eq(key, &self.key(table, slot, &row.generation)),
            ]);
        }
        or(self.state(snapshot).tables[table.0]
            .iter()
            .enumerate()
            .map(|(slot, row)| {
                and([
                    row.occupied.text.clone(),
                    eq(key, &self.key(table, slot, &row.generation)),
                ])
            }))
    }

    fn sum(&mut self, column: ColumnId, snapshot: Snapshot, domain: &ExprType) -> Term {
        let total = self.state(snapshot).aggregates[column.table.0][column.index]
            .as_ref()
            .expect("resolved tracked sum");
        self.coerce(total, domain, expr_signed(domain))
    }

    fn update_aggregate(
        &mut self,
        column: ColumnId,
        old: Option<&Term>,
        value: &Term,
        active: &str,
    ) {
        let Some(previous) = &self.current.aggregates[column.table.0][column.index] else {
            return;
        };
        // Carriers cover capacity times the full scalar range. Removing one
        // live scalar and adding its replacement cannot overflow that carrier.
        let width = previous.width();
        let signed = value_signed(&self.program.column(column).ty);
        let previous_text = previous.text.clone();
        let base = old.map_or_else(
            || previous_text.clone(),
            |old| {
                format!(
                    "(bvsub {previous_text} {})",
                    extend(&old.text, old.width(), width, signed)
                )
            },
        );
        let value = extend(&value.text, value.width(), width, signed);
        let next = format!("(bvadd {base} {value})");
        let next = self.script.bind(
            "aggregate",
            ite(active, &next, &previous_text),
            Sort::Bits(width),
        );
        self.current.aggregates[column.table.0][column.index] = Some(next);
    }

    fn expr(&mut self, id: ExprId) -> LowerResult<Term> {
        self.check_script_budget(self.program.expr(id).span)?;
        if let Some(limit) = self
            .config
            .max_expression_steps
            .filter(|limit| self.work >= *limit)
        {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                format!(
                    "SMT expression expansion exceeds max_expression_steps {}",
                    limit
                ),
                self.program.expr(id).span,
            ));
        }
        self.work += 1;
        let expression = self.program.expr(id).clone();
        let direct = matches!(
            &expression.kind,
            ExprKind::Constant(_) | ExprKind::Binding(_)
        );
        let domain = &expression.domain;
        let value = match expression.kind {
            ExprKind::Constant(value) => {
                let text = match &value {
                    Value::Int(v) => bv(v.clone(), u32::from(scalar_domain(domain).width)),
                    _ => value_literal(self.program, &value, &domain.value_type()),
                };
                Term {
                    text,
                    sort: sort(domain),
                }
            }
            ExprKind::BytesLen { value } => {
                let value = self.expr(value)?;
                Term::bits(format!("((_ int_to_bv 64) (seq.len {}))", value.text), 64)
            }
            ExprKind::BytesGet { value, index } => {
                let value = self.expr(value)?;
                let index = self.expr(index)?;
                let index = format!("(ubv_to_int {})", index.text);
                self.script.trap_if(
                    &mut self.path,
                    &not(&format!("(< {index} (seq.len {}))", value.text)),
                );
                Term::bits(format!("(seq.nth {} {index})", value.text), 8)
            }
            ExprKind::BytesSlice {
                value,
                start,
                count,
                capacity,
            } => {
                let value = self.expr(value)?;
                let start = self.expr(start)?;
                let count = self.expr(count)?;
                let start = format!("(ubv_to_int {})", start.text);
                let count = format!("(ubv_to_int {})", count.text);
                self.script.trap_if(
                    &mut self.path,
                    &not(&and([
                        format!("(<= (+ {start} {count}) (seq.len {}))", value.text),
                        format!("(<= {count} {capacity})"),
                    ])),
                );
                Term {
                    text: format!("(seq.extract {} {start} {count})", value.text),
                    sort: Sort::Bytes,
                }
            }
            ExprKind::BytesConcat {
                left,
                right,
                capacity,
            } => {
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                self.script.trap_if(
                    &mut self.path,
                    &not(&format!(
                        "(<= (+ (seq.len {}) (seq.len {})) {capacity})",
                        left.text, right.text
                    )),
                );
                Term {
                    text: codecs::concat(vec![left.text, right.text]),
                    sort: Sort::Bytes,
                }
            }
            ExprKind::BytesResize { value, capacity } => {
                let value = self.expr(value)?;
                self.script.trap_if(
                    &mut self.path,
                    &not(&valid_value(&ValueType::Bytes { capacity }, &value.text)),
                );
                value
            }
            ExprKind::Encode { value, ty } => {
                let value = self.expr(value)?;
                let value = self.narrow(value, &ty);
                let codec = self.encoder(&ty)?;
                self.encoder_padding_facts(&ty, &value.text)?;
                Term {
                    text: format!("(encode_{codec} {})", value.text),
                    sort: Sort::Bytes,
                }
            }
            ExprKind::CanDecode { value, ty } => {
                let value = self.expr(value)?;
                let codec = self.codec(&ty)?;
                Term::boolean(format!("(decode_valid_{codec} {})", value.text))
            }
            ExprKind::Decode { value, ty } => {
                let value = self.expr(value)?;
                let codec = self.codec(&ty)?;
                self.script.trap_if(
                    &mut self.path,
                    &not(&format!("(decode_valid_{codec} {})", value.text)),
                );
                Term {
                    text: format!("(decode_{codec} {})", value.text),
                    sort: value_sort(&ty),
                }
            }
            ExprKind::Record { ty, fields } => {
                let TypeDefinitionKind::Record(definitions) =
                    &self.program.type_definition(ty).kind
                else {
                    unreachable!("record IR")
                };
                let mut values = BTreeMap::new();
                for (field, expression) in fields {
                    let value = self.expr(expression)?;
                    values.insert(field, self.narrow(value, &definitions[field.0].ty));
                }
                Term {
                    text: application(
                        &constructor(ty, None),
                        &definitions
                            .iter()
                            .map(|f| values[&f.id].text.clone())
                            .collect::<Vec<_>>(),
                    ),
                    sort: Sort::Named(ty),
                }
            }
            ExprKind::Field { record, field } => {
                let value = self.expr(record)?;
                let Sort::Named(ty) = value.sort else {
                    unreachable!("record receiver")
                };
                let TypeDefinitionKind::Record(definitions) =
                    &self.program.type_definition(ty).kind
                else {
                    unreachable!("record receiver")
                };
                Term {
                    text: format!("({} {})", selector(ty, field.0), value.text),
                    sort: value_sort(&definitions[field.0].ty),
                }
            }
            ExprKind::Variant {
                ty,
                variant,
                payload,
            } => {
                let TypeDefinitionKind::Union(definitions) = &self.program.type_definition(ty).kind
                else {
                    unreachable!("union IR")
                };
                let value = self.expr(payload)?;
                let value = self.narrow(value, &definitions[variant.0].payload);
                Term {
                    text: application(&constructor(ty, Some(variant)), &[value.text]),
                    sort: Sort::Named(ty),
                }
            }
            ExprKind::IsVariant { value, variant } => {
                let value = self.expr(value)?;
                let Sort::Named(ty) = value.sort else {
                    unreachable!("union receiver")
                };
                Term::boolean(format!(
                    "((_ is {}) {})",
                    constructor(ty, Some(variant)),
                    value.text
                ))
            }
            ExprKind::Payload { value, variant } => {
                let value = self.expr(value)?;
                let Sort::Named(ty) = value.sort else {
                    unreachable!("union receiver")
                };
                self.script.trap_if(
                    &mut self.path,
                    &not(&format!(
                        "((_ is {}) {})",
                        constructor(ty, Some(variant)),
                        value.text
                    )),
                );
                let TypeDefinitionKind::Union(definitions) = &self.program.type_definition(ty).kind
                else {
                    unreachable!("union receiver")
                };
                Term {
                    text: format!("({} {})", payload_selector(ty, variant), value.text),
                    sort: value_sort(&definitions[variant.0].payload),
                }
            }
            ExprKind::Binding(binding) => self.env.get(&binding).cloned().ok_or_else(|| {
                diagnostic(
                    "D_VERIFY_LOWER",
                    format!("unbound resolved binding {}", binding.0),
                    expression.span,
                )
            })?,
            ExprKind::Unary { op, operand } => {
                let value = self.expr(operand)?;
                match op {
                    UnaryOp::Not => Term::boolean(not(&value.text)),
                    UnaryOp::Neg => {
                        let value = self.coerce(
                            &value,
                            domain,
                            expr_signed(&self.program.expr(operand).domain),
                        );
                        let wide = value.width() + 1;
                        let result = format!(
                            "(bvneg {})",
                            extend(&value.text, value.width(), wide, expr_signed(domain))
                        );
                        let okay = fits(&result, wide, value.width(), expr_signed(domain));
                        self.script.trap_if(&mut self.path, &not(&okay));
                        Term::bits(truncate(&result, wide, value.width()), value.width())
                    }
                }
            }
            ExprKind::Binary { op, left, right } => {
                let a = self.expr(left)?;
                let b = self.expr(right)?;
                if op.boolean() {
                    let text = match op {
                        BinaryOp::And => and([a.text, b.text]),
                        BinaryOp::Or => or([a.text, b.text]),
                        _ => format!("(xor {} {})", a.text, b.text),
                    };
                    Term::boolean(text)
                } else if op.comparison() {
                    let left_domain = &self.program.expr(left).domain;
                    let right_domain = &self.program.expr(right).domain;
                    if a.sort == Sort::Bool || matches!(a.sort, Sort::Named(_) | Sort::Bytes) {
                        Term::boolean(if op == BinaryOp::Eq {
                            eq(&a.text, &b.text)
                        } else {
                            not(&eq(&a.text, &b.text))
                        })
                    } else {
                        let carrier = Domain {
                            scalar: scalar_domain(left_domain).scalar.clone(),
                            width: scalar_domain(left_domain)
                                .width
                                .max(scalar_domain(right_domain).width),
                        };
                        let a = self.coerce(
                            &a,
                            &ExprType::Scalar(carrier.clone()),
                            expr_signed(left_domain),
                        );
                        let b = self.coerce(
                            &b,
                            &ExprType::Scalar(carrier.clone()),
                            expr_signed(right_domain),
                        );
                        Term::boolean(cmp(op.name(), &a.text, &b.text, carrier.signed()))
                    }
                } else {
                    let a = self.coerce(&a, domain, expr_signed(&self.program.expr(left).domain));
                    let b = self.coerce(&b, domain, expr_signed(&self.program.expr(right).domain));
                    let (value, okay) = arithmetic(
                        op.name(),
                        &a.text,
                        &b.text,
                        u32::from(scalar_domain(domain).width),
                        expr_signed(domain),
                        scalar_domain(domain).scale(),
                    );
                    self.script.trap_if(&mut self.path, &not(&okay));
                    Term::bits(value, u32::from(scalar_domain(domain).width))
                }
            }
            ExprKind::Select { condition, yes, no } => {
                let condition = self.expr(condition)?;
                let outer = self.path.active.clone();
                self.path.active = and([outer.clone(), condition.text.clone()]);
                let yes_value = self.expr(yes)?;
                self.path.active = and([outer.clone(), not(&condition.text)]);
                let no_value = self.expr(no)?;
                self.path.active = outer;
                let yes_value = self.coerce(
                    &yes_value,
                    domain,
                    expr_signed(&self.program.expr(yes).domain),
                );
                let no_value = self.coerce(
                    &no_value,
                    domain,
                    expr_signed(&self.program.expr(no).domain),
                );
                Term {
                    text: ite(&condition.text, &yes_value.text, &no_value.text),
                    sort: sort(domain),
                }
            }
            ExprKind::TableGet { column, key, state } => {
                let key = self.expr_at(key, state)?;
                let exists = self.exists(column.table, &key.text, state);
                self.script.trap_if(&mut self.path, &not(&exists));
                self.read_column(column, &key.text, state)
            }
            ExprKind::SlotRead { column, key, state } => {
                let key = self.expr_at(key, state)?;
                self.read_column(column, &key.text, state)
            }
            ExprKind::TableExists { table, key, state } => {
                let key = self.expr_at(key, state)?;
                Term::boolean(self.exists(table, &key.text, state))
            }
            ExprKind::UniqueLookup {
                column,
                value,
                state,
                exists_only,
            } => {
                let value_domain = self.program.expr(value).domain.clone();
                let value = self.expr_at(value, state)?;
                let rows = &self.state(state).tables[column.table.0];
                let mut cases = Vec::new();
                let mut key = bv(0, 64);
                for (slot, row) in rows.iter().enumerate().rev() {
                    let resident = self.coerce(
                        &row.columns[column.index],
                        &value_domain,
                        value_signed(&self.program.column(column).ty),
                    );
                    let found = and([row.occupied.text.clone(), eq(&value.text, &resident.text)]);
                    key = ite(&found, &self.key(column.table, slot, &row.generation), &key);
                    cases.push(found);
                }
                let found = or(cases);
                if exists_only {
                    Term::boolean(found)
                } else {
                    self.script.trap_if(&mut self.path, &not(&found));
                    Term::bits(key, 64)
                }
            }
            ExprKind::Sum { column, state } => self.sum(column, state, domain),
            ExprKind::KeyOf { table: _, value } => {
                let input = self.expr(value)?;
                let input_domain = &self.program.expr(value).domain;
                self.coerce(&input, domain, expr_signed(input_domain))
            }
            ExprKind::Call { function, args } => {
                self.function_call(function, &args, expression.span)?
            }
            ExprKind::EffectCount => self.effect_count.clone(),
            ExprKind::EffectIs { effect, index } => {
                let index = self.expr(index)?;
                Term::boolean(self.effect_hit(effect, &index))
            }
            ExprKind::EffectArg {
                effect,
                index,
                parameter,
            } => {
                let index = self.expr(index)?;
                let hit = self.effect_hit(effect, &index);
                self.script.trap_if(&mut self.path, &not(&hit));
                let parameter_type = &self.program.effect(effect).params[parameter].ty;
                let element = value_sort(parameter_type);
                let array = self
                    .effect_args
                    .get(&(effect, parameter))
                    .cloned()
                    .unwrap_or_else(|| Self::empty_effect_array(element));
                let value = Term {
                    text: format!(
                        "(select {array} {})",
                        truncate(&index.text, index.width(), 64)
                    ),
                    sort: element,
                };
                self.coerce(&value, domain, expr_signed(domain))
            }
            ExprKind::Fold(fold) => self.fold(&fold)?,
        };
        let value = self.coerce(&value, domain, expr_signed(domain));
        Ok(if direct { value } else { self.bind(value) })
    }

    fn function_call(&mut self, id: FunctionId, args: &[ExprId], span: Span) -> LowerResult<Term> {
        if !self.verified_functions.contains(&id) {
            return Err(diagnostic(
                "D_VERIFY_DEPENDENCY",
                format!(
                    "function {:?} has no independently verified contract",
                    self.program.function(id).name
                ),
                span,
            ));
        }
        let function = self.program.function(id).clone();
        let mut values = Vec::new();
        for (argument, parameter) in args.iter().zip(&function.params) {
            let value = self.expr(*argument)?;
            values.push(self.narrow(value, &parameter.ty));
        }
        let saved_env = self.env.clone();
        for (parameter, value) in function.params.iter().zip(&values) {
            self.env.insert(parameter.binding, value.clone());
        }
        // A call's precondition is an obligation of its caller, not an assumption.
        for requirement in &function.requires {
            let condition = self.expr(requirement.value)?;
            self.script.trap_if(&mut self.path, &not(&condition.text));
        }
        let symbol = format!("function_{}", id.0);
        if self.declared_functions.insert(id) {
            let parameters = function
                .params
                .iter()
                .map(|p| value_sort(&p.ty).smt())
                .collect::<Vec<_>>()
                .join(" ");
            self.script.declaration(format!(
                "(declare-fun {symbol} ({parameters}) {})",
                value_sort(&function.result_type).smt()
            ));
        }
        let application = if values.is_empty() {
            symbol
        } else {
            format!(
                "({symbol} {})",
                values
                    .iter()
                    .map(|v| v.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        let result = self.bind(Term {
            text: application,
            sort: value_sort(&function.result_type),
        });
        let active = and([self.path.live.clone(), self.path.active.clone()]);
        self.script.assert(implies(
            &active,
            &valid_value(&function.result_type, &result.text),
        ));
        self.env.insert(function.result, result.clone());
        for ensure in &function.ensures {
            let condition = self.spec(ensure.value, false)?;
            self.script.assert(implies(&active, &condition));
        }
        self.env = saved_env;
        self.summary_calls += 1;
        Ok(result)
    }

    fn empty_effect_array(element: Sort) -> String {
        let zero = match element {
            Sort::Bool => "false".into(),
            Sort::Bits(width) => bv(0, width),
            Sort::Named(id) => format!("zero_{}", id.0),
            Sort::Bytes => codecs::empty(),
        };
        format!(
            "((as const (Array (_ BitVec 64) {})) {zero})",
            element.smt()
        )
    }

    fn effect_hit(&self, effect: EffectId, index: &Term) -> String {
        let key = truncate(&index.text, index.width(), 64);
        and([
            fits(&index.text, index.width(), 64, false),
            cmp("lt", &key, &self.effect_count.text, false),
            eq(
                &format!("(select {} {key})", self.effect_tags),
                &bv(effect.0 + 1, 64),
            ),
        ])
    }

    fn statements(&mut self, statements: &[Stmt]) -> LowerResult<()> {
        for statement in statements {
            match &statement.kind {
                StmtKind::Let { binding, value } => {
                    let value = self.expr(*value)?;
                    let value = self.narrow(value, &self.program.binding(*binding).ty);
                    self.env.insert(*binding, value);
                }
                StmtKind::Guard { condition, .. } => {
                    let condition = self.expr(*condition)?;
                    self.script.guard(&mut self.path, &condition.text);
                }
                StmtKind::TableSet { column, key, value } => {
                    let key = self.expr(*key)?;
                    let value = self.expr(*value)?;
                    let value = self.narrow(value, &self.program.column(*column).ty);
                    let exists = self.exists(column.table, &key.text, Snapshot::Current);
                    self.script.trap_if(&mut self.path, &not(&exists));
                    let active = and([self.path.live.clone(), self.path.active.clone()]);
                    let old = self.read_column(*column, &key.text, Snapshot::Current);
                    self.update_aggregate(*column, Some(&old), &value, &active);
                    for slot in 0..self.current.tables[column.table.0].len() {
                        let row = &self.current.tables[column.table.0][slot];
                        let hit = and([
                            active.clone(),
                            eq(&key.text, &self.key(column.table, slot, &row.generation)),
                        ]);
                        let new_value = self.script.bind(
                            "write",
                            ite(&hit, &value.text, &row.columns[column.index].text),
                            value.sort,
                        );
                        self.current.tables[column.table.0][slot].columns[column.index] = new_value;
                    }
                    self.current.writes.push(Write {
                        column: *column,
                        key: key.clone(),
                        value,
                        active: active.clone(),
                    });
                    self.current
                        .touched
                        .entry(column.table)
                        .or_default()
                        .push((key, active));
                }
                StmtKind::Allocate {
                    table,
                    binding,
                    fields,
                } => self.allocate(*table, *binding, fields)?,
                StmtKind::Emit { effect, args } => {
                    if self
                        .config
                        .max_effect_events
                        .is_some_and(|limit| self.effect_events >= limit)
                    {
                        return Err(diagnostic(
                            "D_VERIFY_RESOURCE",
                            "symbolic outbox exceeds max_effect_events",
                            statement.span,
                        ));
                    }
                    let mut values = Vec::new();
                    for (argument, parameter) in
                        args.iter().zip(&self.program.effect(*effect).params)
                    {
                        let value = self.expr(*argument)?;
                        values.push(self.narrow(value, &parameter.ty));
                    }
                    // Arguments may themselves emit. Check again at the actual
                    // append boundary, not only on entry to the outer emit.
                    if self
                        .config
                        .max_effect_events
                        .is_some_and(|limit| self.effect_events >= limit)
                    {
                        return Err(diagnostic(
                            "D_VERIFY_RESOURCE",
                            "symbolic outbox exceeds max_effect_events",
                            statement.span,
                        ));
                    }
                    let active = self
                        .script
                        .bind(
                            "effect_active",
                            and([self.path.live.clone(), self.path.active.clone()]),
                            Sort::Bool,
                        )
                        .text;
                    let ordinal = self.effect_count.clone();
                    let store = format!(
                        "(store {} {} {})",
                        self.effect_tags,
                        ordinal.text,
                        bv(effect.0 + 1, 64)
                    );
                    self.effect_tags = self.script.bind_array(
                        "effect_tags",
                        &ite(&active, &store, &self.effect_tags),
                        Sort::Bits(64),
                    );
                    for (parameter, value) in values.into_iter().enumerate() {
                        let previous = self
                            .effect_args
                            .get(&(*effect, parameter))
                            .cloned()
                            .unwrap_or_else(|| Self::empty_effect_array(value.sort));
                        let store = format!("(store {previous} {} {})", ordinal.text, value.text);
                        let next = self.script.bind_array(
                            "effect_arg",
                            &ite(&active, &store, &previous),
                            value.sort,
                        );
                        self.effect_args.insert((*effect, parameter), next);
                    }
                    self.effect_count = self.script.bind(
                        "effect_count",
                        format!(
                            "(bvadd {} {})",
                            ordinal.text,
                            ite(&active, &bv(1, 64), &bv(0, 64))
                        ),
                        Sort::Bits(64),
                    );
                    self.effect_events += 1;
                    self.check_script_budget(statement.span)?;
                }
                StmtKind::Eval(value) => {
                    self.expr(*value)?;
                }
                StmtKind::Commit => {}
            }
        }
        Ok(())
    }

    fn allocate(
        &mut self,
        table: TableId,
        binding: BindingId,
        fields: &[(ColumnId, ExprId)],
    ) -> LowerResult<()> {
        // Field expressions run before resource checks, including when full.
        let mut values = BTreeMap::new();
        for (column, expression) in fields {
            let value = self.expr(*expression)?;
            values.insert(
                column.index,
                self.narrow(value, &self.program.column(*column).ty),
            );
        }
        let rows = self.current.tables[table.0].clone();
        let mut previous_occupied = "true".to_string();
        let mut choices = Vec::new();
        let mut generation_okay = Vec::new();
        let mut key = bv(0, 64);
        let generation_bits = self.program.table(table).generation_bits();
        for (slot, row) in rows.iter().enumerate() {
            let chosen = self
                .script
                .bind(
                    "allocated",
                    and([previous_occupied.clone(), not(&row.occupied.text)]),
                    Sort::Bool,
                )
                .text;
            previous_occupied = self
                .script
                .bind(
                    "occupied_prefix",
                    and([previous_occupied, row.occupied.text.clone()]),
                    Sort::Bool,
                )
                .text;
            generation_okay.push(implies(
                &chosen,
                &not(&eq(
                    &row.generation.text,
                    &bv((BigInt::one() << generation_bits) - 1, generation_bits),
                )),
            ));
            let next_generation = Term::bits(
                format!("(bvadd {} {})", row.generation.text, bv(1, generation_bits)),
                generation_bits,
            );
            key = ite(&chosen, &self.key(table, slot, &next_generation), &key);
            choices.push(chosen);
        }
        self.script.guard(
            &mut self.path,
            &and([or(choices.iter().cloned()), and(generation_okay)]),
        );
        for column in self
            .program
            .table(table)
            .columns
            .iter()
            .filter(|column| column.unique)
        {
            let value = &values[&column.id.index];
            let duplicate = or(rows.iter().map(|row| {
                and([
                    row.occupied.text.clone(),
                    eq(&row.columns[column.id.index].text, &value.text),
                ])
            }));
            self.script.guard(&mut self.path, &not(&duplicate));
        }
        let active = and([self.path.live.clone(), self.path.active.clone()]);
        for column in &self.program.table(table).columns {
            self.update_aggregate(column.id, None, &values[&column.id.index], &active);
        }
        for (slot, row) in rows.iter().enumerate() {
            let hit = and([active.clone(), choices[slot].clone()]);
            let occupied = self.script.bind(
                "occupancy",
                or([row.occupied.text.clone(), hit.clone()]),
                Sort::Bool,
            );
            let generation = self.script.bind(
                "generation",
                ite(
                    &hit,
                    &format!("(bvadd {} {})", row.generation.text, bv(1, generation_bits)),
                    &row.generation.text,
                ),
                row.generation.sort,
            );
            let mut columns = Vec::new();
            for (index, old) in row.columns.iter().enumerate() {
                let value = &values[&index];
                columns.push(self.script.bind(
                    "initialized",
                    ite(&hit, &value.text, &old.text),
                    old.sort,
                ));
            }
            self.current.tables[table.0][slot] = Row {
                occupied,
                generation,
                columns,
            };
        }
        let key = self.bind(Term::bits(key, 64));
        for column in &self.program.table(table).columns {
            self.current.writes.push(Write {
                column: column.id,
                key: key.clone(),
                value: values[&column.id.index].clone(),
                active: active.clone(),
            });
        }
        self.current
            .touched
            .entry(table)
            .or_default()
            .push((key.clone(), active));
        self.env.insert(binding, key);
        Ok(())
    }

    fn fold(&mut self, fold: &Fold) -> LowerResult<Term> {
        if self
            .config
            .max_unroll
            .is_some_and(|limit| fold.limit > limit)
        {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                "fold exceeds max_unroll",
                self.program.binding(fold.index).span,
            ));
        }
        let initial = self.expr(fold.init)?;
        let initial = self.narrow(initial, &self.program.binding(fold.accumulator).ty);
        let lo = self.expr(fold.lo)?;
        let lo = self.narrow(lo, &self.program.binding(fold.index).ty);
        let hi = self.expr(fold.hi)?;
        let hi = self.narrow(hi, &self.program.binding(fold.index).ty);
        let outer_env = self.env.clone();
        let outer_active = self.path.active.clone();
        let index_domain = Domain::scalar(
            self.program
                .binding(fold.index)
                .ty
                .as_scalar()
                .expect("integer index")
                .clone(),
        );
        let index_width = lo.width();
        let extra = (u32::BITS - fold.limit.max(1).leading_zeros()) + 1;
        let wide = index_width + extra;
        let lower = extend(&lo.text, index_width, wide, index_domain.signed());
        let upper = extend(&hi.text, hi.width(), wide, index_domain.signed());
        let max = bv(index_domain.max_value(), wide);
        let mut accumulator = initial;
        for iteration in 0..fold.limit {
            self.env = outer_env.clone();
            let index = self.script.bind(
                "fold_index",
                format!("(bvadd {lower} {})", bv(iteration, wide)),
                Sort::Bits(wide),
            );
            let active = and([
                outer_active.clone(),
                cmp("le", &index.text, &upper, index_domain.signed()),
                cmp("le", &index.text, &max, index_domain.signed()),
            ]);
            self.path.active = self.script.bind("fold_active", active, Sort::Bool).text;
            self.env.insert(fold.accumulator, accumulator.clone());
            self.env.insert(
                fold.index,
                Term::bits(truncate(&index.text, wide, index_width), index_width),
            );
            self.statements(&fold.body)?;
            let value = self.expr(fold.yield_value)?;
            let value = self.narrow(value, &self.program.binding(fold.accumulator).ty);
            accumulator = self.bind(Term {
                text: ite(&self.path.active, &value.text, &accumulator.text),
                sort: accumulator.sort,
            });
        }
        self.env = outer_env;
        self.path.active = outer_active;
        Ok(accumulator)
    }

    fn spec(&mut self, expression: ExprId, pre: bool) -> LowerResult<String> {
        let saved_path = self.path.clone();
        let saved_pre = self.pre_context;
        let saved_state = self.current.clone();
        let saved_env = self.env.clone();
        self.path = Path::default();
        self.pre_context = pre;
        let value = self.expr(expression);
        let result =
            value.map(|value| and([self.path.live.clone(), not(&self.path.trapped), value.text]));
        self.path = saved_path;
        self.pre_context = saved_pre;
        self.current = saved_state;
        self.env = saved_env;
        result
    }

    fn invariants(&mut self, pre: bool) -> LowerResult<String> {
        let mut conditions = Vec::new();
        let stable_occupancy = self
            .program
            .tables()
            .iter()
            .filter(|table| {
                self.pre.tables[table.id.0]
                    .iter()
                    .zip(&self.current.tables[table.id.0])
                    .all(|(pre, current)| {
                        pre.occupied.text == current.occupied.text
                            && pre.generation.text == current.generation.text
                    })
            })
            .map(|table| table.id)
            .collect::<Vec<_>>();
        for invariant in self.program.invariants() {
            if let Some((table, binding)) = invariant.quantifier {
                if !pre
                    && row_local(
                        self.program,
                        invariant.condition,
                        table,
                        binding,
                        &stable_occupancy,
                    )
                {
                    // No deletion exists. Every unchanged live row keeps its
                    // exact pre-state key and local values; only handles in the
                    // mutation footprint need a fresh local preservation check.
                    let touched = self
                        .current
                        .touched
                        .get(&table)
                        .cloned()
                        .unwrap_or_default();
                    let saved = self.env.get(&binding).cloned();
                    for (key, active) in touched {
                        self.env.insert(binding, key);
                        conditions.push(implies(&active, &self.spec(invariant.condition, false)?));
                    }
                    if let Some(saved) = saved {
                        self.env.insert(binding, saved);
                    } else {
                        self.env.remove(&binding);
                    }
                    continue;
                }
                let rows = if pre {
                    self.pre.tables[table.0].clone()
                } else {
                    self.current.tables[table.0].clone()
                };
                let saved = self.env.get(&binding).cloned();
                for (slot, row) in rows.iter().enumerate() {
                    let key = self.key(table, slot, &row.generation);
                    self.env.insert(binding, Term::bits(key.clone(), 64));
                    let saved_slot = self.known_slot.replace((table, key, slot));
                    let condition = self.spec(invariant.condition, pre);
                    self.known_slot = saved_slot;
                    conditions.push(implies(&row.occupied.text, &condition?));
                }
                if let Some(saved) = saved {
                    self.env.insert(binding, saved);
                } else {
                    self.env.remove(&binding);
                }
            } else {
                conditions.push(self.spec(invariant.condition, pre)?);
            }
        }
        Ok(and(conditions))
    }
}

fn row_local(
    program: &CheckedProgram,
    expression: ExprId,
    table: TableId,
    quantifier: BindingId,
    stable_occupancy: &[TableId],
) -> bool {
    let own_key =
        |key| matches!(program.expr(key).kind, ExprKind::Binding(binding) if binding == quantifier);
    match &program.expr(expression).kind {
        ExprKind::BytesLen { value }
        | ExprKind::BytesResize { value, .. }
        | ExprKind::Encode { value, .. }
        | ExprKind::CanDecode { value, .. }
        | ExprKind::Decode { value, .. } => {
            row_local(program, *value, table, quantifier, stable_occupancy)
        }
        ExprKind::BytesGet { value, index } => [value, index]
            .into_iter()
            .all(|e| row_local(program, *e, table, quantifier, stable_occupancy)),
        ExprKind::BytesSlice {
            value,
            start,
            count,
            ..
        } => [value, start, count]
            .into_iter()
            .all(|e| row_local(program, *e, table, quantifier, stable_occupancy)),
        ExprKind::BytesConcat { left, right, .. } => [left, right]
            .into_iter()
            .all(|e| row_local(program, *e, table, quantifier, stable_occupancy)),
        ExprKind::Record { fields, .. } => fields
            .iter()
            .all(|(_, e)| row_local(program, *e, table, quantifier, stable_occupancy)),
        ExprKind::Field { record, .. } => {
            row_local(program, *record, table, quantifier, stable_occupancy)
        }
        ExprKind::Variant { payload, .. } => {
            row_local(program, *payload, table, quantifier, stable_occupancy)
        }
        ExprKind::IsVariant { value, .. } | ExprKind::Payload { value, .. } => {
            row_local(program, *value, table, quantifier, stable_occupancy)
        }
        ExprKind::Constant(_) => true,
        ExprKind::Binding(binding) => *binding == quantifier,
        ExprKind::Unary { operand, .. } => {
            row_local(program, *operand, table, quantifier, stable_occupancy)
        }
        ExprKind::Binary { left, right, .. } => {
            row_local(program, *left, table, quantifier, stable_occupancy)
                && row_local(program, *right, table, quantifier, stable_occupancy)
        }
        ExprKind::Select { condition, yes, no } => [condition, yes, no]
            .into_iter()
            .all(|expr| row_local(program, *expr, table, quantifier, stable_occupancy)),
        ExprKind::TableGet { column, key, .. } | ExprKind::SlotRead { column, key, .. } => {
            column.table == table && own_key(*key)
        }
        ExprKind::TableExists {
            table: target, key, ..
        } => {
            (*target == table && own_key(*key))
                || (stable_occupancy.contains(target)
                    && row_local(program, *key, table, quantifier, stable_occupancy))
        }
        ExprKind::Call { .. }
        | ExprKind::EffectCount
        | ExprKind::EffectIs { .. }
        | ExprKind::EffectArg { .. } => false,
        ExprKind::UniqueLookup { .. }
        | ExprKind::Sum { .. }
        | ExprKind::KeyOf { .. }
        | ExprKind::Fold(_) => false,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Obligation {
    Safety,
    Panic,
    Liveness,
}

impl Obligation {
    fn name(self) -> &'static str {
        match self {
            Self::Safety => "safety / partial correctness",
            Self::Panic => "no unhandled arithmetic traps",
            Self::Liveness => "non-vacuity / liveness",
        }
    }

    fn predicate<'a>(self, encoding: &'a Encoding<'_>) -> &'a str {
        match self {
            Self::Safety => &encoding.safety,
            Self::Panic => &encoding.panic,
            Self::Liveness => &encoding.liveness,
        }
    }
}

struct Candidate {
    params: BTreeMap<String, Value>,
    state: State,
}

fn parse_bv(text: &str) -> Result<BigInt, String> {
    if let Some(text) = text.strip_prefix("#x") {
        return BigInt::parse_bytes(text.as_bytes(), 16)
            .ok_or_else(|| "invalid hexadecimal Z3 model value".into());
    }
    if let Some(text) = text.strip_prefix("#b") {
        return BigInt::parse_bytes(text.as_bytes(), 2)
            .ok_or_else(|| "invalid binary Z3 model value".into());
    }
    if let Some(text) = text.strip_prefix("(_ bv") {
        let digits = text.split_whitespace().next().ok_or("missing Z3 numeral")?;
        return BigInt::parse_bytes(digits.as_bytes(), 10)
            .ok_or_else(|| "invalid decimal Z3 model value".into());
    }
    BigInt::parse_bytes(text.as_bytes(), 10)
        .ok_or_else(|| format!("unsupported Z3 model numeral {text}"))
}

/// Read native constructor ASTs, never parse printed datatype expressions.
fn model_value(
    program: &CheckedProgram,
    model: &z3::Model,
    expression: &Dynamic,
    ty: &ValueType,
) -> Result<Value, String> {
    let value = model
        .eval(expression, true)
        .ok_or("missing structured model value")?;
    let result = match ty {
        ValueType::Bytes { capacity } => {
            let seq = value.as_seq().ok_or("model byte sequence sort mismatch")?;
            let length = model
                .eval(&seq.length(), true)
                .and_then(|n| n.as_u64())
                .ok_or("invalid model byte sequence length")?;
            if length > *capacity {
                return Err("model byte sequence exceeds capacity".into());
            }
            let length = usize::try_from(length)
                .map_err(|_| "model byte sequence exceeds host address space")?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(|_| "model byte sequence exceeds available memory")?;
            for i in 0..length {
                let byte = model
                    .eval(&seq.nth(z3::ast::Int::from_u64(i as u64)), true)
                    .and_then(|v| v.as_bv())
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or("invalid model byte sequence element")?;
                bytes.push(byte);
            }
            Value::Bytes(bytes)
        }
        ValueType::Scalar(ScalarType::Bool) => Value::Bool(
            value
                .as_bool()
                .and_then(|v| v.as_bool())
                .ok_or("model boolean sort mismatch")?,
        ),
        ValueType::Scalar(scalar) => {
            let bits = value.as_bv().ok_or("model scalar sort mismatch")?;
            if bits.get_size() != u32::from(scalar.bits()) {
                return Err("model scalar width mismatch".into());
            }
            scalar_signed_value(Value::Int(parse_bv(&bits.to_string())?), scalar)
        }
        ValueType::Named(id) => {
            if value.as_datatype().is_none() {
                return Err("model datatype sort mismatch".into());
            }
            let name = value.decl().name();
            match &program.type_definition(*id).kind {
                TypeDefinitionKind::Record(fields) => {
                    if name != constructor(*id, None) || value.num_children() != fields.len() {
                        return Err("model record constructor mismatch".into());
                    }
                    let mut values = Vec::new();
                    for field in fields {
                        values.push(model_value(
                            program,
                            model,
                            &value.nth_child(field.id.0).ok_or("missing record child")?,
                            &field.ty,
                        )?);
                    }
                    Value::Record {
                        ty: *id,
                        fields: values,
                    }
                }
                TypeDefinitionKind::Union(variants) => {
                    let variant = variants
                        .iter()
                        .find(|v| name == constructor(*id, Some(v.id)))
                        .ok_or("model union constructor mismatch")?;
                    if value.num_children() != 1 {
                        return Err("model union payload arity mismatch".into());
                    }
                    let payload = model_value(
                        program,
                        model,
                        &value.nth_child(0).ok_or("missing union payload")?,
                        &variant.payload,
                    )?;
                    Value::Variant {
                        ty: *id,
                        tag: variant.id,
                        payload: Box::new(payload),
                    }
                }
            }
        }
    };
    coerce_value(program, &result, ty)
}
fn model_named_constant(model: &z3::Model, name: &str) -> Option<Dynamic> {
    model
        .iter()
        .find(|decl| decl.arity() == 0 && decl.name() == name)
        .map(|decl| decl.apply(&[]))
}
fn typed_signed_value(value: Value, ty: &ValueType) -> Value {
    match ty {
        ValueType::Scalar(ty) => scalar_signed_value(value, ty),
        ValueType::Named(_) | ValueType::Bytes { .. } => value,
    }
}

fn model_term(program: &CheckedProgram, model: &z3::Model, term: &Term) -> Result<Value, String> {
    match term.sort {
        Sort::Bytes => {
            let expression = z3::ast::Seq::new_const(term.text.as_str(), &z3::Sort::bitvector(8));
            model_value(
                program,
                model,
                &Dynamic::from_ast(&expression),
                &ValueType::Bytes { capacity: u64::MAX },
            )
        }
        Sort::Named(id) => {
            let Some(expression) = model_named_constant(model, &term.text) else {
                // Z3 omits unconstrained constants; canonical zero is one valid
                // completion, subsequently independently checked by replay.
                return Ok(zero_value(program, &ValueType::Named(id)));
            };
            model_value(program, model, &expression, &ValueType::Named(id))
        }
        Sort::Bool => {
            let expression = Bool::new_const(term.text.as_str());
            model
                .eval(&expression, true)
                .and_then(|value| value.as_bool())
                .map(Value::Bool)
                .ok_or_else(|| format!("missing boolean model value {}", term.text))
        }
        Sort::Bits(width) => {
            let expression = BV::new_const(term.text.as_str(), width);
            let value = model
                .eval(&expression, true)
                .ok_or_else(|| format!("missing bit-vector model value {}", term.text))?;
            Ok(Value::Int(parse_bv(&value.to_string())?))
        }
    }
}

fn model_equality(model: &z3::Model, term: &Term) -> Result<Bool, String> {
    match term.sort {
        Sort::Named(_) | Sort::Bytes => {
            let expression = model_named_constant(model, &term.text)
                .ok_or("unconstrained datatype has no blocking declaration")?;
            model
                .eval(&expression, true)
                .map(|value| expression.eq(&value))
                .ok_or_else(|| format!("missing datatype model value {}", term.text))
        }
        Sort::Bool => {
            let expression = Bool::new_const(term.text.as_str());
            model
                .eval(&expression, true)
                .map(|value| expression.eq(&value))
                .ok_or_else(|| format!("missing boolean model value {}", term.text))
        }
        Sort::Bits(width) => {
            let expression = BV::new_const(term.text.as_str(), width);
            model
                .eval(&expression, true)
                .map(|value| expression.eq(&value))
                .ok_or_else(|| format!("missing bit-vector model value {}", term.text))
        }
    }
}

fn scalar_signed_value(value: Value, ty: &ScalarType) -> Value {
    match value {
        Value::Int(value)
            if ty.signed() && value >= (BigInt::one() << (u32::from(ty.bits()) - 1)) =>
        {
            Value::Int(value - (BigInt::one() << u32::from(ty.bits())))
        }
        value => value,
    }
}

fn project(encoding: &Encoding<'_>, model: &z3::Model) -> Result<Candidate, String> {
    let mut state = if encoding.target_function.is_some() {
        State {
            tables: Vec::new(),
            outbox: Vec::new(),
        }
    } else {
        State::empty(encoding.program)
            .map_err(|error| format!("state allocation: {}", error.code))?
    };
    let mut params = BTreeMap::new();
    for variable in &encoding.projections {
        let value = model_term(encoding.program, model, &variable.term)?;
        match &variable.location {
            Location::Parameter(name, ty) => {
                params.insert(name.clone(), typed_signed_value(value, ty));
            }
            Location::Occupied(table, slot) => {
                state.tables[table.0].occupied[*slot] = value.boolean()?;
            }
            Location::Generation(table, slot) => {
                state.tables[table.0].generations[*slot] = value
                    .int()?
                    .to_u32()
                    .ok_or("generation model exceeds u32")?;
            }
            Location::Column(column, slot, ty) => {
                state.tables[column.table.0].columns[column.index][*slot] =
                    typed_signed_value(value, ty);
            }
        }
    }
    if encoding.target_function.is_some() {
        return Ok(Candidate { params, state });
    }
    recompute_aggregates(encoding.program, &mut state)
        .map_err(|error| format!("aggregate materialization: {}", error.code))?;
    rebuild_indexes(encoding.program, &mut state)
        .map_err(|error| format!("index materialization: {}", error.code))?;
    validate_state(encoding.program, &state)
        .map_err(|error| format!("pre-state validation: {}", error.code))?;
    Ok(Candidate { params, state })
}

fn failed_ensures(
    program: &CheckedProgram,
    transition: &Transition,
    pre: &State,
    current: &State,
    bindings: &BTreeMap<BindingId, Value>,
) -> Vec<FailedEnsure> {
    transition
        .ensures
        .iter()
        .filter_map(|ensure| {
            let outcome = match eval_expression(program, current, pre, bindings, ensure.value) {
                Ok(Value::Bool(true)) => return None,
                Ok(value) => EnsureOutcome::Value { value },
                Err(error) => EnsureOutcome::Error { error },
            };
            Some(FailedEnsure {
                name: ensure.name.clone(),
                span: program.expr(ensure.value).span,
                outcome,
            })
        })
        .collect()
}

fn emitted_effects(program: &CheckedProgram, pre: &State, current: &State) -> Vec<EmittedEffect> {
    current
        .outbox
        .iter()
        .skip(pre.outbox.len())
        .enumerate()
        .map(|(index, record)| {
            let effect = program.effect(record.effect);
            let arguments = effect
                .params
                .iter()
                .zip(&record.args)
                .map(|(parameter, value)| {
                    let scalar_type = match &parameter.ty {
                        ValueType::Scalar(ScalarType::Id64(table)) => {
                            format!("id64<{}>", program.table(*table).name)
                        }
                        ty => program.value_type_name(ty),
                    };
                    (
                        parameter.name.clone(),
                        EffectArgument {
                            scalar_type,
                            value: value_to_json(program, value, &parameter.ty),
                        },
                    )
                })
                .collect();
            EmittedEffect {
                index,
                effect: effect.name.clone(),
                arguments,
            }
        })
        .collect()
}

/// Evaluate specifications independently of the encoder, including each live
/// quantified row and the actual post-state rather than a model's claim.
fn violated_postconditions(
    program: &CheckedProgram,
    transition: &Transition,
    pre: &State,
    current: &State,
    bindings: &BTreeMap<BindingId, Value>,
) -> (Vec<String>, Vec<FailedEnsure>) {
    let failed_ensures = failed_ensures(program, transition, pre, current, bindings);
    let mut violated = failed_ensures
        .iter()
        .map(|ensure| format!("ensure {}", ensure.name))
        .collect::<Vec<_>>();
    for invariant in program.invariants() {
        if let Some((table, binding)) = invariant.quantifier {
            let table_info = program.table(table);
            let table_state = &current.tables[table.0];
            for slot in 0..table_info.capacity as usize {
                if !table_state.occupied[slot] {
                    continue;
                }
                let key = (u64::from(table_state.generations[slot]) << table_info.key_bits())
                    | slot as u64;
                let mut environment = bindings.clone();
                environment.insert(binding, Value::Int(BigInt::from(key)));
                if eval_expression(program, current, pre, &environment, invariant.condition).ok()
                    != Some(Value::Bool(true))
                {
                    violated.push(format!(
                        "invariant {} at {}[{key}]",
                        invariant.name, table_info.name
                    ));
                    break;
                }
            }
        } else if eval_expression(program, current, pre, bindings, invariant.condition).ok()
            != Some(Value::Bool(true))
        {
            violated.push(format!("invariant {}", invariant.name));
        }
    }
    (violated, failed_ensures)
}

fn replay_function(
    encoding: &Encoding<'_>,
    candidate: &Candidate,
    obligation: Obligation,
    id: FunctionId,
) -> Result<Counterexample, String> {
    let function = encoding.program.function(id);
    let args = function
        .params
        .iter()
        .map(|p| candidate.params[&p.name].clone())
        .collect::<Vec<_>>();
    let executed = crate::runtime::eval_function(encoding.program, id, &args);
    let mut bindings = function
        .params
        .iter()
        .zip(&args)
        .map(|(p, v)| (p.binding, v.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut return_value = None;
    let mut failure = None;
    let mut details = Vec::new();
    let mut violated = Vec::new();
    let mut trace = Vec::new();
    match (obligation, executed) {
        (Obligation::Panic, Err(error)) if error.kind == FailureKind::Trap => {
            violated.push(error.code.clone());
            trace.push(TraceStep {
                span: error.span.unwrap_or(function.span),
                kind: "trap".into(),
                text: error.code.clone(),
                value: None,
            });
            failure = Some(error);
        }
        (Obligation::Panic, _) => {
            return Err("function trap candidate does not replay as a trap".into())
        }
        (_, Ok(value)) => {
            bindings.insert(function.result, value.clone());
            trace.push(TraceStep {
                span: encoding.program.expr(function.body).span,
                kind: "return".into(),
                text: format!("return {}", function.name),
                value: Some(value.clone()),
            });
            return_value = Some(value);
            details = failed_ensures(
                encoding.program,
                &encoding.transition,
                &candidate.state,
                &candidate.state,
                &bindings,
            );
            if matches!(obligation, Obligation::Safety) {
                violated = details
                    .iter()
                    .map(|detail| format!("ensure {}", detail.name))
                    .collect();
                if violated.is_empty() {
                    return Err(
                        "function postcondition candidate is not reproduced by concrete evaluation"
                            .into(),
                    );
                }
            }
        }
        (_, Err(error)) => {
            return Err(format!(
                "function committing candidate replays as {}",
                error.code
            ))
        }
    }
    let law = violated
        .first()
        .cloned()
        .unwrap_or_else(|| "concrete returning function execution".into());
    let typed_return_value = return_value.as_ref().map(|value| EffectArgument {
        scalar_type: encoding.program.value_type_name(&function.result_type),
        value: value_to_json(encoding.program, value, &function.result_type),
    });
    Ok(Counterexample {target_kind:"function".into(),return_value,typed_return_value,transition:function.name.clone(),obligation:obligation.name().into(),law,params:candidate.params.clone(),pre_state:candidate.state.clone(),trace,failure,violated,emitted_effects:Vec::new(),failed_ensures:details,directive:"Correct the function body or its independently justified contract; callers cannot assume an unproved summary.".into()})
}

fn replay(
    encoding: &Encoding<'_>,
    candidate: &Candidate,
    obligation: Obligation,
) -> Result<Counterexample, String> {
    if let Some(function) = encoding.target_function {
        return replay_function(encoding, candidate, obligation, function);
    }
    let mut machine = Machine::with_state(encoding.program, candidate.state.clone())
        .map_err(|error| format!("native state admission: {}", error.code))?;
    let execution = machine.execute(&encoding.transition.name, &candidate.params, true);
    let mut ensure_failures = Vec::new();
    let (trace, violated, law, directive, failure) = match (obligation, execution) {
        (Obligation::Panic, Err(error)) if error.kind == FailureKind::Trap => {
            let law = format!("reachable trap {}", error.code);
            (
                machine.last_trace.clone(),
                vec![error.code.clone()],
                law,
                "Guard the failing checked operation or missing-row access before evaluating it."
                    .to_string(),
                Some(error),
            )
        }
        (Obligation::Panic, Err(error)) => {
            return Err(format!(
                "symbolic trap replays as a handled {:?}: {}",
                error.kind, error.code
            ))
        }
        (Obligation::Panic, Ok(_)) => {
            return Err("symbolic trap candidate commits during native replay".into())
        }
        (Obligation::Safety, Ok(execution)) => {
            let (violated, details) = violated_postconditions(
                encoding.program,
                &encoding.transition,
                &candidate.state,
                &machine.state,
                &execution.bindings,
            );
            if violated.is_empty() {
                return Err(
                    "symbolic postcondition failure is not reproduced by native replay".into(),
                );
            }
            ensure_failures = details;
            let law = violated[0].clone();
            (execution.trace, violated, law, "Correct the transition to satisfy the declared law; for effect contracts, check emission count, type, order and payload.".to_string(), None)
        }
        (Obligation::Safety, Err(error)) => {
            return Err(format!(
                "symbolic committing candidate reverts during native replay: {}",
                error.code
            ))
        }
        (Obligation::Liveness, Ok(execution)) => {
            ensure_failures = failed_ensures(
                encoding.program,
                &encoding.transition,
                &candidate.state,
                &machine.state,
                &execution.bindings,
            );
            (
                execution.trace,
                Vec::new(),
                "concrete committing execution".into(),
                String::new(),
                None,
            )
        }
        (Obligation::Liveness, Err(error)) => {
            return Err(format!(
                "symbolic committing candidate reverts during native replay: {}",
                error.code
            ))
        }
    };
    Ok(Counterexample {
        target_kind: "transition".into(),
        return_value: None,
        typed_return_value: None,
        transition: encoding.transition.name.clone(),
        obligation: obligation.name().into(),
        law,
        params: candidate.params.clone(),
        pre_state: candidate.state.clone(),
        trace,
        failure,
        violated,
        emitted_effects: emitted_effects(encoding.program, &candidate.state, &machine.state),
        failed_ensures: ensure_failures,
        directive,
    })
}

fn solve(
    encoding: &Encoding<'_>,
    obligation: Obligation,
) -> (ObligationResult, String, Option<Counterexample>) {
    let started = Instant::now();
    let predicate = obligation.predicate(encoding);
    let base_script = encoding.script.query(predicate, encoding.config.timeout_ms);
    let mut exported_script = base_script.clone() + "(check-sat)\n";
    let mut last_spurious = None;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut blocks = Vec::new();
        let solver = Solver::new();
        solver.from_string(base_script.clone());
        let mut params = z3::Params::new();
        let mut attempt = 0_usize;
        loop {
            if encoding
                .config
                .witness_attempts
                .is_some_and(|limit| attempt >= limit)
            {
                return (
                    "unknown",
                    Some(format!(
                        "no realizable witness after {attempt} candidates: {}",
                        last_spurious.unwrap_or_else(|| "candidate budget exhausted".into())
                    )),
                    None,
                );
            }
            if let Some(timeout_ms) = encoding.config.timeout_ms {
                let remaining =
                    u128::from(timeout_ms).saturating_sub(started.elapsed().as_millis());
                if remaining == 0 {
                    return ("unknown", Some("verification timeout".into()), None);
                }
                params.set_u32("timeout", remaining as u32);
                solver.set_params(&params);
            }
            exported_script = base_script.clone() + &blocks.join("\n") + "\n(check-sat)\n";
            match solver.check() {
                SatResult::Unsat if attempt == 0 => return ("unsat", None, None),
                SatResult::Unsat => {
                    return (
                        "unknown",
                        Some(
                            last_spurious
                                .clone()
                                .unwrap_or_else(|| "no realizable candidate was found".into()),
                        ),
                        None,
                    )
                }
                SatResult::Unknown => {
                    return (
                        "unknown",
                        solver
                            .get_reason_unknown()
                            .or_else(|| Some("solver returned unknown".into())),
                        None,
                    )
                }
                SatResult::Sat => {
                    let Some(model) = solver.get_model() else {
                        return (
                            "unknown",
                            Some("solver returned SAT without a model".into()),
                            None,
                        );
                    };
                    // Extract the full assignment even if materialization fails,
                    // so a different finite candidate can be considered.
                    let equalities = encoding
                        .projections
                        .iter()
                        .map(|variable| {
                            model_term(encoding.program, &model, &variable.term).map(|value| {
                                let literal = match value {
                                    Value::Bytes(bytes) => value_literal(
                                        encoding.program,
                                        &Value::Bytes(bytes),
                                        &ValueType::Bytes { capacity: 0 },
                                    ),
                                    Value::Bool(value) => boolean(value),
                                    Value::Int(value) => bv(value, variable.term.width()),
                                    value @ (Value::Record { ty, .. }
                                    | Value::Variant { ty, .. }) => value_literal(
                                        encoding.program,
                                        &value,
                                        &ValueType::Named(ty),
                                    ),
                                };
                                eq(&variable.term.text, &literal)
                            })
                        })
                        .collect::<Result<Vec<_>, _>>();
                    match project(encoding, &model)
                        .map(|candidate| replay(encoding, &candidate, obligation))
                    {
                        Ok(Ok(witness)) => return ("sat", None, Some(witness)),
                        Ok(Err(reason)) if encoding.summary_calls > 0 => {
                            return (
                                "unknown",
                                Some(format!(
                                    "contract summary candidate could not replay: {reason}"
                                )),
                                None,
                            );
                        }
                        // Index/state materialization failures are independent
                        // of summary precision; continue physical candidates.
                        Err(reason) | Ok(Err(reason)) => {
                            last_spurious = Some(reason);
                        }
                    }
                    let equalities = match equalities {
                        Ok(equalities) => equalities,
                        Err(reason) => return ("unknown", Some(reason), None),
                    };
                    let blocked = not(&and(equalities));
                    let clause = format!("(assert {blocked})");
                    let next_bytes = exported_script
                        .len()
                        .saturating_add(clause.len())
                        .saturating_add(1);
                    if encoding
                        .config
                        .max_smt_bytes
                        .is_some_and(|limit| next_bytes > limit)
                    {
                        return (
                            "unknown",
                            Some("witness search exceeds max_smt_bytes".into()),
                            None,
                        );
                    }
                    let native_equalities = encoding
                        .projections
                        .iter()
                        .map(|variable| model_equality(&model, &variable.term))
                        .collect::<Result<Vec<_>, _>>();
                    let native_equalities = match native_equalities {
                        Ok(equalities) => equalities,
                        Err(reason) => return ("unknown", Some(reason), None),
                    };
                    solver.assert(Bool::and(&native_equalities).not());
                    blocks.push(clause);
                }
            }
            let Some(next_attempt) = attempt.checked_add(1) else {
                return (
                    "unknown",
                    Some("witness attempt count exceeds host address space".into()),
                    None,
                );
            };
            attempt = next_attempt;
        }
    }));
    let (status, reason, witness) = result.unwrap_or_else(|_| {
        (
            "error",
            Some("native solver rejected the generated encoding".into()),
            None,
        )
    });
    (
        ObligationResult::new(
            obligation.name(),
            status,
            started.elapsed().as_secs_f64() * 1000.0,
            reason,
        ),
        exported_script,
        witness,
    )
}

fn failed(transition: &str, diagnostic: Diagnostic, config: &VerifierConfig) -> VerificationResult {
    let reason = Some(diagnostic.message.clone());
    VerificationResult {
        target_kind: "transition".into(),
        contract_dependencies: Vec::new(),
        transition: transition.into(),
        config: config.clone(),
        encoding_metrics: None,
        safety: ObligationResult::new(Obligation::Safety.name(), "error", 0.0, reason.clone()),
        panic: ObligationResult::new(Obligation::Panic.name(), "error", 0.0, reason.clone()),
        liveness: ObligationResult::new(Obligation::Liveness.name(), "error", 0.0, reason),
        smt_safety: String::new(),
        smt_panic: String::new(),
        smt_liveness: String::new(),
        diagnostics: vec![diagnostic],
        counterexample: None,
        witness: None,
    }
}

/// A private invocation-scoped cache is the only authority to instantiate a
/// function contract. CheckedProgram alone grants no such authority.
struct VerificationSession<'a> {
    program: &'a CheckedProgram,
    config: &'a VerifierConfig,
    results: BTreeMap<FunctionId, VerificationResult>,
    verified: BTreeSet<FunctionId>,
}

impl<'a> VerificationSession<'a> {
    fn new(program: &'a CheckedProgram, config: &'a VerifierConfig) -> Self {
        Self {
            program,
            config,
            results: BTreeMap::new(),
            verified: BTreeSet::new(),
        }
    }
    fn function(&mut self, id: FunctionId) {
        let mut needed = BTreeSet::new();
        let mut todo = vec![id];
        while let Some(current) = todo.pop() {
            if !self.results.contains_key(&current) && needed.insert(current) {
                todo.extend(self.program.function(current).dependencies.iter().copied());
            }
        }
        // Semantic analysis assigns IDs in dependency-topological order. This
        // avoids recursive proof traversal and verifies each DAG node once.
        for id in needed {
            let function = self.program.function(id).clone();
            let wrapper = function_transition(&function);
            let mut result = if function
                .dependencies
                .iter()
                .any(|id| !self.verified.contains(id))
            {
                failed(
                    &function.name,
                    diagnostic(
                        "D_VERIFY_DEPENDENCY",
                        "a function dependency failed verification; its contract cannot be assumed",
                        function.span,
                    ),
                    self.config,
                )
            } else {
                verify_target(
                    self.program,
                    &wrapper,
                    self.config,
                    &self.verified,
                    Some(id),
                )
            };
            result.target_kind = "function".into();
            result.contract_dependencies = self.dependency_evidence(&function.dependencies);
            if result.ok() {
                self.verified.insert(id);
            }
            self.results.insert(id, result);
        }
    }
    fn dependency_evidence(&self, roots: &[FunctionId]) -> Vec<ContractDependency> {
        // Retain only direct DAG edges. Transitive evidence is shared by the
        // separately emitted function proofs, not copied into every caller.
        let ids = roots.iter().copied().collect::<BTreeSet<_>>();
        ids.into_iter()
            .filter_map(|id| {
                self.results.get(&id).map(|result| ContractDependency {
                    function: self.program.function(id).name.clone(),
                    safety: result.safety.status.clone(),
                    panic: result.panic.status.clone(),
                    liveness: result.liveness.status.clone(),
                    ok: result.ok(),
                })
            })
            .collect()
    }
    fn transition(&mut self, transition: &Transition) -> VerificationResult {
        let dependencies = transition_dependencies(self.program, transition);
        for dependency in &dependencies {
            self.function(*dependency);
        }
        let mut result = if dependencies.iter().any(|id| !self.verified.contains(id)) {
            failed(&transition.name,diagnostic("D_VERIFY_DEPENDENCY","a called function failed verification; no caller proof may assume its contract",transition.span),self.config)
        } else {
            verify_target(self.program, transition, self.config, &self.verified, None)
        };
        result.contract_dependencies = self.dependency_evidence(&dependencies);
        result
    }
}

fn function_transition(function: &Function) -> Transition {
    Transition {
        id: TransitionId(usize::MAX),
        name: function.name.clone(),
        params: function.params.clone(),
        reads: Vec::new(),
        writes: Vec::new(),
        requires: function.requires.clone(),
        ensures: function.ensures.clone(),
        body: vec![
            Stmt {
                kind: StmtKind::Let {
                    binding: function.result,
                    value: function.body,
                },
                span: function.span,
            },
            Stmt {
                kind: StmtKind::Commit,
                span: function.span,
            },
        ],
        wcet_bound: function.wcet_bound,
        fold_depth: function.fold_depth,
        max_fold_product: function.max_fold_product,
        tier: 1,
        span: function.span,
    }
}

fn statement_dependencies(
    program: &CheckedProgram,
    statements: &[Stmt],
    out: &mut BTreeSet<FunctionId>,
) {
    for statement in statements {
        let expressions = match &statement.kind {
            StmtKind::Let { value, .. } | StmtKind::Eval(value) => vec![*value],
            StmtKind::Guard { condition, .. } => vec![*condition],
            StmtKind::TableSet { key, value, .. } => vec![*key, *value],
            StmtKind::Allocate { fields, .. } => fields.iter().map(|(_, value)| *value).collect(),
            StmtKind::Emit { args, .. } => args.clone(),
            StmtKind::Commit => Vec::new(),
        };
        for expression in expressions {
            expression_dependencies(program, expression, out);
        }
    }
}
fn expression_dependencies(program: &CheckedProgram, id: ExprId, out: &mut BTreeSet<FunctionId>) {
    let mut children = Vec::new();
    match &program.expr(id).kind {
        ExprKind::BytesLen { value }
        | ExprKind::BytesResize { value, .. }
        | ExprKind::Encode { value, .. }
        | ExprKind::CanDecode { value, .. }
        | ExprKind::Decode { value, .. } => children.push(*value),
        ExprKind::BytesGet { value, index } => children.extend([*value, *index]),
        ExprKind::BytesSlice {
            value,
            start,
            count,
            ..
        } => children.extend([*value, *start, *count]),
        ExprKind::BytesConcat { left, right, .. } => children.extend([*left, *right]),
        ExprKind::Record { fields, .. } => children.extend(fields.iter().map(|(_, e)| *e)),
        ExprKind::Field { record, .. } => children.push(*record),
        ExprKind::Variant { payload, .. } => children.push(*payload),
        ExprKind::IsVariant { value, .. } | ExprKind::Payload { value, .. } => {
            children.push(*value)
        }
        ExprKind::Call { function, args } => {
            out.insert(*function);
            children.extend(args);
        }
        ExprKind::Unary { operand, .. } => children.push(*operand),
        ExprKind::Binary { left, right, .. } => children.extend([*left, *right]),
        ExprKind::Select { condition, yes, no } => children.extend([*condition, *yes, *no]),
        ExprKind::TableGet { key, .. }
        | ExprKind::SlotRead { key, .. }
        | ExprKind::TableExists { key, .. } => children.push(*key),
        ExprKind::UniqueLookup { value, .. } | ExprKind::KeyOf { value, .. } => {
            children.push(*value)
        }
        ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => {
            children.push(*index)
        }
        ExprKind::Fold(fold) => {
            children.extend([fold.init, fold.lo, fold.hi, fold.yield_value]);
            statement_dependencies(program, &fold.body, out);
        }
        ExprKind::Constant(_)
        | ExprKind::Binding(_)
        | ExprKind::Sum { .. }
        | ExprKind::EffectCount => {}
    }
    for child in children {
        expression_dependencies(program, child, out);
    }
}
fn transition_dependencies(program: &CheckedProgram, transition: &Transition) -> Vec<FunctionId> {
    let mut out = BTreeSet::new();
    for invariant in program.invariants() {
        expression_dependencies(program, invariant.condition, &mut out);
    }
    for access in transition.reads.iter().chain(&transition.writes) {
        if let Some(key) = access.key {
            expression_dependencies(program, key, &mut out);
        }
    }
    for expression in transition.requires.iter().chain(&transition.ensures) {
        expression_dependencies(program, expression.value, &mut out);
    }
    statement_dependencies(program, &transition.body, &mut out);
    out.into_iter().collect()
}

fn verify_target(
    program: &CheckedProgram,
    transition: &Transition,
    config: &VerifierConfig,
    verified: &BTreeSet<FunctionId>,
    function: Option<FunctionId>,
) -> VerificationResult {
    let encoding = match Encoding::new(program, transition, config, verified, function) {
        Ok(encoding) => encoding,
        Err(diagnostic) => return failed(&transition.name, diagnostic, config),
    };
    let (safety, smt_safety, safety_witness) = solve(&encoding, Obligation::Safety);
    let (panic, smt_panic, panic_witness) = solve(&encoding, Obligation::Panic);
    let (liveness, smt_liveness, witness) = solve(&encoding, Obligation::Liveness);
    let mut diagnostics = Vec::new();
    for result in [&safety, &panic, &liveness] {
        if result.status == "error" {
            diagnostics.push(diagnostic(
                "D_SOLVER",
                result
                    .reason
                    .clone()
                    .unwrap_or_else(|| "native solver error".into()),
                transition.span,
            ));
        }
    }
    let encoding_metrics = Some(EncodingMetrics {
        state_cells: encoding.state_cells,
        expression_steps: encoding.work,
        effect_events: encoding.effect_events,
        retained_smt_bytes: encoding.script.bytes(),
        max_query_bytes: [&smt_safety, &smt_panic, &smt_liveness]
            .into_iter()
            .map(|query| query.len())
            .max()
            .unwrap_or(0),
    });
    VerificationResult {
        target_kind: if function.is_some() {
            "function"
        } else {
            "transition"
        }
        .into(),
        contract_dependencies: Vec::new(),
        transition: transition.name.clone(),
        config: config.clone(),
        encoding_metrics,
        safety,
        panic,
        liveness,
        smt_safety,
        smt_panic,
        smt_liveness,
        diagnostics,
        counterexample: safety_witness.or(panic_witness),
        witness,
    }
}

/// Prove a transition and each reachable pure function independently. A private
/// per-invocation session reuses dependencies; no persistent or supplied proof
/// token is trusted. Solver UNKNOWN/error and unvalidated SAT fail closed.
pub fn verify_transition(
    program: &CheckedProgram,
    name: &str,
    config: &VerifierConfig,
) -> VerificationResult {
    let Some(id) = program.transition_id(name) else {
        return failed(
            name,
            diagnostic(
                "D_UNKNOWN_TRANSITION",
                format!("unknown transition {name:?}"),
                Span::default(),
            ),
            config,
        );
    };
    VerificationSession::new(program, config).transition(program.transition(id))
}
/// Independently verify one pure function and its reachable dependencies.
pub fn verify_function(
    program: &CheckedProgram,
    name: &str,
    config: &VerifierConfig,
) -> VerificationResult {
    let Some(id) = program.function_id(name) else {
        let mut result = failed(
            name,
            diagnostic(
                "D_UNKNOWN_FUNCTION",
                format!("unknown function {name:?}"),
                Span::default(),
            ),
            config,
        );
        result.target_kind = "function".into();
        return result;
    };
    let mut session = VerificationSession::new(program, config);
    session.function(id);
    session.results[&id].clone()
}
pub fn verify_transition_dual(
    program: &CheckedProgram,
    name: &str,
    config: &VerifierConfig,
) -> VerificationResult {
    verify_transition(program, name, config)
}
pub fn verify_all(program: &CheckedProgram, config: &VerifierConfig) -> Vec<VerificationResult> {
    let mut session = VerificationSession::new(program, config);
    for function in program.functions() {
        session.function(function.id);
    }
    let mut results = program
        .functions()
        .iter()
        .map(|f| session.results[&f.id].clone())
        .collect::<Vec<_>>();
    results.extend(program.transitions().iter().map(|t| session.transition(t)));
    results
}
