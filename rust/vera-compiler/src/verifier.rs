//! Inductive verification over the complete finite state and checked expressions.
//!
//! The universal queries use an exact finite table model. Unique indexes are
//! abstracted by their logical row relation: this admits additional committing
//! executions when a physical index would saturate, so UNSAT remains sound.
//! Row-local invariants use a frame rule over the complete mutation footprint;
//! global and cross-row specifications still use the complete finite state.
//! Every SAT answer is admitted only after concrete state validation and replay.

#[path = "smt/terms.rs"]
mod terms;

use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    time::Instant,
};

use num_bigint::BigInt;
use num_traits::{One, ToPrimitive};
use serde::Serialize;
use z3::{
    ast::{Bool, BV},
    SatResult, Solver,
};

use crate::{
    diagnostic::{Diagnostic, Span},
    ir::*,
    runtime::{
        eval_expression, rebuild_indexes, recompute_aggregates, validate_state, FailureKind,
        Machine, RuntimeError, State, TraceStep,
    },
    types::{Domain, ScalarType},
    value::Value,
};
use terms::{
    and, arithmetic, boolean, bv, cmp, eq, extend, fits, implies, ite, not, or, truncate, Path,
    Script, Sort, Term,
};

/// Explicit resource bounds; no table capacity is ever reduced to fit a proof.
#[derive(Clone, Debug, Serialize)]
pub struct VerifierConfig {
    pub timeout_ms: u32,
    pub max_state_cells: usize,
    pub max_unroll: u32,
    pub witness_attempts: usize,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            max_state_cells: 4096,
            max_unroll: 1024,
            witness_attempts: 8,
        }
    }
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
    pub transition: String,
    pub obligation: String,
    pub law: String,
    pub params: BTreeMap<String, Value>,
    pub pre_state: State,
    pub trace: Vec<TraceStep>,
    pub failure: Option<RuntimeError>,
    pub violated: Vec<String>,
    pub directive: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerificationResult {
    pub transition: String,
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

impl VerificationResult {
    pub fn ok(&self) -> bool {
        self.diagnostics.is_empty()
            && self.safety.status == "unsat"
            && self.panic.status == "unsat"
            && self.liveness.status == "sat"
    }

    pub fn summary(&self) -> String {
        let mut out = format!("transition {}:", self.transition);
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
    Parameter(String, ScalarType),
    Occupied(TableId, usize),
    Generation(TableId, usize),
    Column(ColumnId, usize, ScalarType),
}

#[derive(Clone)]
struct Projected {
    term: Term,
    location: Location,
}

struct Encoding<'a> {
    program: &'a CheckedProgram,
    transition: &'a Transition,
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
}

type LowerResult<T> = Result<T, Diagnostic>;

fn diagnostic(code: &str, message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        message: message.into(),
        span,
    }
}

fn sort(domain: &Domain) -> Sort {
    if domain.scalar.is_bool() {
        Sort::Bool
    } else {
        Sort::Bits(u32::from(domain.width))
    }
}

fn scalar_sort(ty: &ScalarType) -> Sort {
    sort(&Domain::scalar(ty.clone()))
}

impl<'a> Encoding<'a> {
    fn new(
        program: &'a CheckedProgram,
        transition: &'a Transition,
        config: &'a VerifierConfig,
    ) -> LowerResult<Self> {
        let cells = program
            .tables()
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
        if cells > config.max_state_cells {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                format!(
                    "complete state requires {cells} cells; max_state_cells is {}",
                    config.max_state_cells
                ),
                transition.span,
            ));
        }
        if transition.max_fold_product > config.max_unroll {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                format!(
                    "fold product {} exceeds max_unroll {}",
                    transition.max_fold_product, config.max_unroll
                ),
                transition.span,
            ));
        }
        let mut this = Self {
            program,
            transition,
            config,
            script: Script::default(),
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
        };
        for table in program.tables() {
            let mut rows = Vec::new();
            for slot in 0..table.capacity as usize {
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
                        scalar_sort(&column.ty),
                        Location::Column(column.id, slot, column.ty.clone()),
                    );
                    this.constrain_domain(&term, &Domain::scalar(column.ty.clone()));
                    let zero = if column.ty.is_bool() {
                        "false".into()
                    } else {
                        bv(0, u32::from(column.ty.bits()))
                    };
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
                this.script.declarations.push(format!(
                    "(declare-const {inverse} (Array {} (_ BitVec 64)))",
                    scalar_sort(&column.ty).smt()
                ));
                for (slot, row) in rows.iter().enumerate() {
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
                    let width = u32::from(column.ty.aggregate_width());
                    let mut total = bv(0, width);
                    // Unoccupied physical columns are constrained to zero by
                    // the state schema, so this is precisely the live-row sum.
                    for row in &rows {
                        let value = &row.columns[column.id.index];
                        total = format!(
                            "(bvadd {total} {})",
                            extend(&value.text, value.width(), width, column.ty.signed())
                        );
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
                scalar_sort(&parameter.ty),
                Location::Parameter(parameter.name.clone(), parameter.ty.clone()),
            );
            this.constrain_domain(&term, &Domain::scalar(parameter.ty.clone()));
            this.env.insert(parameter.binding, term);
        }
        let pre_invariants = this.invariants(true)?;
        this.script.assert(pre_invariants);
        for requirement in &transition.requires {
            let condition = this.expr(requirement.value)?;
            this.script.guard(&mut this.path, &condition.text);
        }
        this.statements(&transition.body)?;
        let mut postconditions = vec![this.invariants(false)?];
        for ensure in &transition.ensures {
            postconditions.push(this.spec(ensure.value, false)?);
        }
        this.safety = and([this.path.live.clone(), not(&and(postconditions))]);
        this.panic = this.path.trapped.clone();
        this.liveness = this.path.live.clone();
        Ok(this)
    }

    fn input(&mut self, name: &str, kind: Sort, location: Location) -> Term {
        let term = self.script.declare(name, kind);
        self.projections.push(Projected {
            term: term.clone(),
            location,
        });
        term
    }

    fn constrain_domain(&mut self, term: &Term, domain: &Domain) {
        if let ScalarType::Enum(tags) = &domain.scalar {
            if tags.len() < 256 {
                self.script
                    .assert(cmp("lt", &term.text, &bv(tags.len(), term.width()), false));
            }
        }
    }

    fn state(&self, snapshot: Snapshot) -> &SymbolicState {
        if snapshot == Snapshot::Pre || self.pre_context {
            &self.pre
        } else {
            &self.current
        }
    }

    fn coerce(&self, value: &Term, domain: &Domain, signed: bool) -> Term {
        if value.sort == Sort::Bool {
            return value.clone();
        }
        let width = u32::from(domain.width);
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

    fn narrow(&mut self, value: Term, target: &ScalarType) -> Term {
        if target.is_bool() {
            return value;
        }
        let width = u32::from(target.bits());
        let okay = fits(&value.text, value.width(), width, target.signed());
        self.script.trap_if(&mut self.path, &not(&okay));
        self.coerce(&value, &Domain::scalar(target.clone()), target.signed())
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

    fn slot(&self, table: TableId, key: &str) -> String {
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
        let mut value = rows[0].columns[column.index].clone();
        for (index, row) in rows.iter().enumerate().skip(1).rev() {
            value.text = ite(
                &eq(&slot, &bv(index, 64)),
                &row.columns[column.index].text,
                &value.text,
            );
        }
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

    fn sum(&mut self, column: ColumnId, snapshot: Snapshot, domain: &Domain) -> Term {
        let total = self.state(snapshot).aggregates[column.table.0][column.index]
            .as_ref()
            .expect("resolved tracked sum");
        self.coerce(total, domain, domain.signed())
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
        let signed = self.program.column(column).ty.signed();
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
        self.work += 1;
        // A resource error is explicit and fails closed rather than exhausting
        // memory on an intentionally enormous specification fold.
        if self.work
            > self
                .config
                .max_state_cells
                .saturating_mul(self.config.max_unroll as usize)
                .max(100_000)
        {
            return Err(diagnostic(
                "D_VERIFY_RESOURCE",
                "SMT expression expansion exceeded the configured resource budget",
                self.program.expr(id).span,
            ));
        }
        let expression = self.program.expr(id).clone();
        let direct = matches!(
            &expression.kind,
            ExprKind::Constant(_) | ExprKind::Binding(_)
        );
        let domain = &expression.domain;
        let value = match expression.kind {
            ExprKind::Constant(value) => match value {
                Value::Bool(value) => Term::boolean(boolean(value)),
                Value::Int(value) => {
                    Term::bits(bv(value, u32::from(domain.width)), u32::from(domain.width))
                }
            },
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
                        let value =
                            self.coerce(&value, domain, self.program.expr(operand).domain.signed());
                        let wide = value.width() + 1;
                        let result = format!(
                            "(bvneg {})",
                            extend(&value.text, value.width(), wide, domain.signed())
                        );
                        let okay = fits(&result, wide, value.width(), domain.signed());
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
                    if a.sort == Sort::Bool {
                        Term::boolean(if op == BinaryOp::Eq {
                            eq(&a.text, &b.text)
                        } else {
                            not(&eq(&a.text, &b.text))
                        })
                    } else {
                        let carrier = Domain {
                            scalar: left_domain.scalar.clone(),
                            width: left_domain.width.max(right_domain.width),
                        };
                        let a = self.coerce(&a, &carrier, left_domain.signed());
                        let b = self.coerce(&b, &carrier, right_domain.signed());
                        Term::boolean(cmp(op.name(), &a.text, &b.text, carrier.signed()))
                    }
                } else {
                    let a = self.coerce(&a, domain, self.program.expr(left).domain.signed());
                    let b = self.coerce(&b, domain, self.program.expr(right).domain.signed());
                    let (value, okay) = arithmetic(
                        op.name(),
                        &a.text,
                        &b.text,
                        u32::from(domain.width),
                        domain.signed(),
                        domain.scale(),
                    );
                    self.script.trap_if(&mut self.path, &not(&okay));
                    Term::bits(value, u32::from(domain.width))
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
                let yes_value =
                    self.coerce(&yes_value, domain, self.program.expr(yes).domain.signed());
                let no_value =
                    self.coerce(&no_value, domain, self.program.expr(no).domain.signed());
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
                        self.program.column(column).ty.signed(),
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
                self.coerce(&input, domain, input_domain.signed())
            }
            ExprKind::Fold(fold) => self.fold(&fold)?,
        };
        let value = self.coerce(&value, domain, domain.signed());
        Ok(if direct { value } else { self.bind(value) })
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
                    for (argument, parameter) in
                        args.iter().zip(&self.program.effect(*effect).params)
                    {
                        let value = self.expr(*argument)?;
                        self.narrow(value, &parameter.ty);
                    }
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
        if fold.limit > self.config.max_unroll {
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
        let index_domain = Domain::scalar(self.program.binding(fold.index).ty.clone());
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
                    self.env.insert(
                        binding,
                        Term::bits(self.key(table, slot, &row.generation), 64),
                    );
                    conditions.push(implies(
                        &row.occupied.text,
                        &self.spec(invariant.condition, pre)?,
                    ));
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

fn model_term(model: &z3::Model, term: &Term) -> Result<Value, String> {
    match term.sort {
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

fn signed_value(value: Value, ty: &ScalarType) -> Value {
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
    let mut state = State::empty(encoding.program)
        .map_err(|error| format!("state allocation: {}", error.code))?;
    let mut params = BTreeMap::new();
    for variable in &encoding.projections {
        let value = model_term(model, &variable.term)?;
        match &variable.location {
            Location::Parameter(name, ty) => {
                params.insert(name.clone(), signed_value(value, ty));
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
                state.tables[column.table.0].columns[column.index][*slot] = signed_value(value, ty);
            }
        }
    }
    recompute_aggregates(encoding.program, &mut state)
        .map_err(|error| format!("aggregate materialization: {}", error.code))?;
    rebuild_indexes(encoding.program, &mut state)
        .map_err(|error| format!("index materialization: {}", error.code))?;
    validate_state(encoding.program, &state)
        .map_err(|error| format!("pre-state validation: {}", error.code))?;
    Ok(Candidate { params, state })
}

/// Evaluate specifications independently of the encoder, including each live
/// quantified row and the actual post-state rather than a model's claim.
fn violated_postconditions(
    program: &CheckedProgram,
    transition: &Transition,
    pre: &State,
    current: &State,
    bindings: &BTreeMap<BindingId, Value>,
) -> Vec<String> {
    let mut violated = Vec::new();
    for ensure in &transition.ensures {
        if eval_expression(program, current, pre, bindings, ensure.value).ok()
            != Some(Value::Bool(true))
        {
            violated.push(format!("ensure {}", ensure.name));
        }
    }
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
    violated
}

fn replay(
    encoding: &Encoding<'_>,
    candidate: &Candidate,
    obligation: Obligation,
) -> Result<Counterexample, String> {
    let mut machine = Machine::with_state(encoding.program, candidate.state.clone())
        .map_err(|error| format!("native state admission: {}", error.code))?;
    let execution = machine.execute(&encoding.transition.name, &candidate.params, true);
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
            let violated = violated_postconditions(
                encoding.program,
                encoding.transition,
                &candidate.state,
                &machine.state,
                &execution.bindings,
            );
            if violated.is_empty() {
                return Err(
                    "symbolic postcondition failure is not reproduced by native replay".into(),
                );
            }
            let law = violated[0].clone();
            (execution.trace, violated, law, "Strengthen the transition's guards or state updates so every committed execution preserves this law.".to_string(), None)
        }
        (Obligation::Safety, Err(error)) => {
            return Err(format!(
                "symbolic committing candidate reverts during native replay: {}",
                error.code
            ))
        }
        (Obligation::Liveness, Ok(execution)) => (
            execution.trace,
            Vec::new(),
            "concrete committing execution".into(),
            String::new(),
            None,
        ),
        (Obligation::Liveness, Err(error)) => {
            return Err(format!(
                "symbolic committing candidate reverts during native replay: {}",
                error.code
            ))
        }
    };
    Ok(Counterexample {
        transition: encoding.transition.name.clone(),
        obligation: obligation.name().into(),
        law,
        params: candidate.params.clone(),
        pre_state: candidate.state.clone(),
        trace,
        failure,
        violated,
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
        let mut params = z3::Params::new();
        for attempt in 0..encoding.config.witness_attempts.max(1) {
            let remaining = u128::from(encoding.config.timeout_ms)
                .saturating_sub(started.elapsed().as_millis());
            if remaining == 0 {
                return ("unknown", Some("verification timeout".into()), None);
            }
            let solver = Solver::new();
            let candidate_script = base_script.clone() + &blocks.join("\n");
            solver.from_string(candidate_script.clone());
            exported_script = candidate_script + "\n(check-sat)\n";
            params.set_u32("timeout", remaining.min(u128::from(u32::MAX)) as u32);
            solver.set_params(&params);
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
                            model_term(&model, &variable.term).map(|value| {
                                let literal = match value {
                                    Value::Bool(value) => boolean(value),
                                    Value::Int(value) => bv(value, variable.term.width()),
                                };
                                eq(&variable.term.text, &literal)
                            })
                        })
                        .collect::<Result<Vec<_>, _>>();
                    match project(encoding, &model)
                        .and_then(|candidate| replay(encoding, &candidate, obligation))
                    {
                        Ok(witness) => return ("sat", None, Some(witness)),
                        Err(reason) => {
                            last_spurious = Some(reason);
                        }
                    }
                    let equalities = match equalities {
                        Ok(equalities) => equalities,
                        Err(reason) => return ("unknown", Some(reason), None),
                    };
                    let blocked = not(&and(equalities));
                    blocks.push(format!("(assert {blocked})"));
                }
            }
        }
        (
            "unknown",
            Some(format!(
                "no realizable witness after {} candidates: {}",
                encoding.config.witness_attempts.max(1),
                last_spurious.unwrap_or_else(|| "native replay failed".into())
            )),
            None,
        )
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

fn failed(transition: &str, diagnostic: Diagnostic) -> VerificationResult {
    let reason = Some(diagnostic.message.clone());
    VerificationResult {
        transition: transition.into(),
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

/// Prove all three obligations for one resolved transition. A solver failure,
/// explicit resource limit or unvalidated SAT candidate always fails closed.
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
        );
    };
    let encoding = match Encoding::new(program, program.transition(id), config) {
        Ok(encoding) => encoding,
        Err(diagnostic) => return failed(name, diagnostic),
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
                program.transition(id).span,
            ));
        }
    }
    VerificationResult {
        transition: name.into(),
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

pub fn verify_transition_dual(
    program: &CheckedProgram,
    name: &str,
    config: &VerifierConfig,
) -> VerificationResult {
    verify_transition(program, name, config)
}

pub fn verify_all(program: &CheckedProgram, config: &VerifierConfig) -> Vec<VerificationResult> {
    program
        .transitions()
        .iter()
        .map(|transition| verify_transition(program, &transition.name, config))
        .collect()
}
