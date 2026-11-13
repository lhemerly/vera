//! Objective syntactic specification inventory. This is not a completeness proof.
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{diagnostic::Span, ir::*, verifier::VerificationResult};

#[derive(Clone, Debug, Serialize)]
pub struct FootprintItem {
    pub id: String,
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PropertyReferences {
    pub kind: String,
    pub name: String,
    pub span: Span,
    pub references: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WitnessEvidence {
    pub status: String,
    pub result_field: Option<String>,
    pub claim: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SpecificationFootprint {
    pub schema_version: u32,
    pub interpretation: String,
    pub universe_scope: String,
    pub inventory: Vec<FootprintItem>,
    pub properties: Vec<PropertyReferences>,
    pub mentioned: Vec<String>,
    pub unmentioned: Vec<String>,
    pub implementation_references: Vec<String>,
    pub joint_safety_status: String,
    pub committing_witness: WitnessEvidence,
    pub counterexample: WitnessEvidence,
    pub unknowns: Vec<String>,
}

fn column(program: &CheckedProgram, id: ColumnId) -> String {
    format!(
        "state:{}.{}",
        program.table(id.table).name,
        program.column(id).name
    )
}
fn effect(program: &CheckedProgram, id: EffectId) -> String {
    format!("effect:{}", program.effect(id).name)
}

struct Walker<'a> {
    program: &'a CheckedProgram,
    bindings: BTreeMap<BindingId, String>,
    references: BTreeSet<String>,
    visited: BTreeSet<ExprId>,
    todo: Vec<ExprId>,
}
impl<'a> Walker<'a> {
    fn new(program: &'a CheckedProgram, params: &[Param], result: Option<BindingId>) -> Self {
        let mut bindings = params
            .iter()
            .map(|p| (p.binding, format!("input:{}", p.name)))
            .collect::<BTreeMap<_, _>>();
        if let Some(id) = result {
            bindings.insert(id, "result".into());
        }
        Self {
            program,
            bindings,
            references: BTreeSet::new(),
            visited: BTreeSet::new(),
            todo: Vec::new(),
        }
    }
    fn statements(&mut self, statements: &[Stmt]) {
        for statement in statements {
            match &statement.kind {
                StmtKind::Let { value, .. } | StmtKind::Eval(value) => self.todo.push(*value),
                StmtKind::Guard { condition, code } => {
                    self.todo.push(*condition);
                    self.references.insert(format!("error:{code}"));
                }
                StmtKind::TableSet {
                    column: id,
                    key,
                    value,
                } => {
                    self.references.insert(column(self.program, *id));
                    self.todo.extend([*key, *value]);
                }
                StmtKind::Allocate { table, fields, .. } => {
                    self.references
                        .insert(format!("table:{}", self.program.table(*table).name));
                    for (id, value) in fields {
                        self.references.insert(column(self.program, *id));
                        self.todo.push(*value);
                    }
                }
                StmtKind::Emit { effect: id, args } => {
                    self.references.insert(effect(self.program, *id));
                    self.references.insert("effect_count".into());
                    for p in &self.program.effect(*id).params {
                        self.references.insert(format!(
                            "effect_arg:{}.{}",
                            self.program.effect(*id).name,
                            p.name
                        ));
                    }
                    self.todo.extend(args);
                }
                StmtKind::OperationCall {
                    operation, args, ..
                } => {
                    self.references.insert(format!(
                        "operation:{}",
                        self.program.operation(*operation).transition.name
                    ));
                    self.todo.extend(args);
                }
                StmtKind::Commit => {}
            }
        }
    }
    fn finish(mut self) -> Vec<String> {
        while let Some(id) = self.todo.pop() {
            if !self.visited.insert(id) {
                continue;
            }
            match &self.program.expr(id).kind {
                ExprKind::Binding(id) => {
                    if let Some(name) = self.bindings.get(id) {
                        self.references.insert(name.clone());
                    }
                }
                ExprKind::Constant(_) => {}
                ExprKind::Unary { operand, .. } => self.todo.push(*operand),
                ExprKind::Binary { left, right, .. }
                | ExprKind::BytesConcat { left, right, .. } => self.todo.extend([*left, *right]),
                ExprKind::Select { condition, yes, no } => {
                    self.todo.extend([*condition, *yes, *no])
                }
                ExprKind::TableGet {
                    column: id, key, ..
                }
                | ExprKind::SlotRead {
                    column: id, key, ..
                } => {
                    self.references.insert(column(self.program, *id));
                    self.todo.push(*key);
                }
                ExprKind::TableExists { table, key, .. }
                | ExprKind::KeyOf { table, value: key } => {
                    self.references
                        .insert(format!("table:{}", self.program.table(*table).name));
                    self.todo.push(*key);
                }
                ExprKind::UniqueLookup {
                    column: id, value, ..
                } => {
                    self.references.insert(column(self.program, *id));
                    self.todo.push(*value);
                }
                ExprKind::Sum { column: id, .. } => {
                    self.references.insert(column(self.program, *id));
                }
                ExprKind::BytesLen { value }
                | ExprKind::BytesResize { value, .. }
                | ExprKind::Encode { value, .. }
                | ExprKind::CanDecode { value, .. }
                | ExprKind::Decode { value, .. }
                | ExprKind::IsVariant { value, .. }
                | ExprKind::Payload { value, .. } => self.todo.push(*value),
                ExprKind::BytesGet { value, index } => self.todo.extend([*value, *index]),
                ExprKind::BytesSlice {
                    value,
                    start,
                    count,
                    ..
                } => self.todo.extend([*value, *start, *count]),
                ExprKind::Field { record, .. } => self.todo.push(*record),
                ExprKind::Record { fields, .. } => self.todo.extend(fields.iter().map(|(_, v)| *v)),
                ExprKind::Variant { payload, .. } => self.todo.push(*payload),
                ExprKind::Call { function, args } => {
                    self.references.insert(format!(
                        "function:{}",
                        self.program.function(*function).name
                    ));
                    self.todo.extend(args);
                }
                ExprKind::Fold(fold) => {
                    self.todo
                        .extend([fold.init, fold.lo, fold.hi, fold.yield_value]);
                    self.statements(&fold.body);
                }
                ExprKind::EffectCount => {
                    self.references.insert("effect_count".into());
                }
                ExprKind::EffectIs { effect: id, index } => {
                    self.references.insert(effect(self.program, *id));
                    self.todo.push(*index);
                }
                ExprKind::EffectArg {
                    effect: id,
                    index,
                    parameter,
                } => {
                    let e = self.program.effect(*id);
                    self.references.insert(effect(self.program, *id));
                    self.references.insert(format!(
                        "effect_arg:{}.{}",
                        e.name, e.params[*parameter].name
                    ));
                    self.todo.push(*index);
                }
            }
        }
        self.references.into_iter().collect()
    }
}

/// A target-local input/result inventory plus program-declared state and effects.
/// No new SMT queries, state enumeration, or solver budgets are introduced.
pub fn specification_footprint(
    program: &CheckedProgram,
    report: &VerificationResult,
) -> Option<SpecificationFootprint> {
    let (params, result, requires, ensures, body, expression, owner) =
        match report.target_kind.as_str() {
            "function" => {
                let f = program.function(program.function_id(&report.transition)?);
                (
                    &f.params,
                    Some(f.result),
                    &f.requires,
                    &f.ensures,
                    None,
                    Some(f.body),
                    None,
                )
            }
            "operation" => {
                let o = program.operation(program.operation_id(&report.transition)?);
                let t = &o.transition;
                (
                    &t.params,
                    None,
                    &t.requires,
                    &t.ensures,
                    Some(&t.body),
                    None,
                    Some(o.component),
                )
            }
            "transition" => {
                let t = program.transition(program.transition_id(&report.transition)?);
                (
                    &t.params,
                    None,
                    &t.requires,
                    &t.ensures,
                    Some(&t.body),
                    None,
                    None,
                )
            }
            _ => return None,
        };
    let mut inventory = BTreeMap::new();
    let mut add = |id: String, kind: &str, name: String| {
        inventory.insert(
            id.clone(),
            FootprintItem {
                id,
                kind: kind.into(),
                name,
            },
        );
    };
    for p in params {
        add(format!("input:{}", p.name), "input", p.name.clone());
    }
    if result.is_some() {
        add("result".into(), "result", "function return".into());
    }
    if result.is_none() {
        for t in program.tables() {
            add(
                format!("table:{}", t.name),
                "table_existence",
                t.name.clone(),
            );
            for c in &t.columns {
                add(
                    column(program, c.id),
                    "state_column",
                    format!("{}.{}", t.name, c.name),
                );
            }
        }
        add(
            "effect_count".into(),
            "effect_count",
            "ordered outbox count".into(),
        );
        for e in program.effects() {
            add(effect(program, e.id), "effect_variant", e.name.clone());
            for p in &e.params {
                add(
                    format!("effect_arg:{}.{}", e.name, p.name),
                    "effect_argument",
                    format!("{}.{}", e.name, p.name),
                );
            }
        }
    }
    let mut implementation = Walker::new(program, params, result);
    if let Some(body) = body {
        implementation.statements(body);
    }
    if let Some(expression) = expression {
        implementation.todo.push(expression);
    }
    let implementation_references = implementation.finish();
    for reference in &implementation_references {
        if let Some(code) = reference.strip_prefix("error:") {
            add(reference.clone(), "guard_error", code.into());
        }
    }
    let mut properties = Vec::new();
    for (kind, expressions) in [("requires", requires), ("ensures", ensures)] {
        for named in expressions {
            let mut walker = Walker::new(program, params, result);
            walker.todo.push(named.value);
            properties.push(PropertyReferences {
                kind: kind.into(),
                name: named.name.clone(),
                span: program.expr(named.value).span,
                references: walker.finish(),
            });
        }
    }
    if result.is_none() {
        for invariant in program.invariants() {
            if owner.is_some() && invariant.owner != owner {
                continue;
            }
            let mut walker = Walker::new(program, params, result);
            walker.todo.push(invariant.condition);
            if let Some((table, _)) = invariant.quantifier {
                walker
                    .references
                    .insert(format!("table:{}", program.table(table).name));
            }
            properties.push(PropertyReferences {
                kind: "invariant".into(),
                name: invariant.name.clone(),
                span: invariant.span,
                references: walker.finish(),
            });
        }
    }
    let mentioned = properties
        .iter()
        .flat_map(|p| p.references.iter().cloned())
        .collect::<BTreeSet<_>>();
    let unmentioned = inventory
        .keys()
        .filter(|id| !mentioned.contains(*id))
        .cloned()
        .collect();
    Some(SpecificationFootprint {
        schema_version:1,
        interpretation:"syntactic_references_only_not_semantic_coverage".into(),
        universe_scope:if result.is_some() { "target_inputs_and_return" } else { "target_inputs_program_declared_state_and_effects_and_body_guard_codes" }.into(),
        inventory:inventory.into_values().collect(),properties,
        mentioned:mentioned.into_iter().collect(),unmentioned,implementation_references,
        joint_safety_status:report.safety.status.clone(),
        committing_witness:WitnessEvidence { status:if report.witness.is_some() { "replayed" } else { "not_established" }.into(),result_field:report.witness.as_ref().map(|_|"witness".into()),claim:"One replayed committing transition/operation or returning function invocation only. It may violate ensures (see failed_ensures); not per-item reachability or specification completeness.".into() },
        counterexample:WitnessEvidence { status:if report.counterexample.is_some() { "replayed" } else { "not_established" }.into(),result_field:report.counterexample.as_ref().map(|_|"counterexample".into()),claim:"Use the existing replayed counterexample and failed_ensures; absence is not per-property success.".into() },
        unknowns:vec!["Type domains, capacities, unique constraints, access frames and projection rules are not counted in these property lists; unmentioned does not mean unconstrained.".into(),"A syntactic mention may be tautological, redundant, conditional, or vacuous; its semantic strength is unknown.".into(),"Host I/O, codecs, external requirements and implicit runtime trap codes are not inventoried; only explicit body guard codes are listed.".into(),"Structured fields and byte elements are inventoried only at their containing input/state/effect value granularity.".into(),"Callee bodies and summaries are not expanded here; function/operation references identify separate contracts.".into(),"Implementation references are syntactic, not executed-path or reachability evidence; local data-flow dependencies are not propagated.".into(),"Unmentioned items include possibly irrelevant program declarations; no completeness percentage or independence claim is computed.".into()],
    })
}
