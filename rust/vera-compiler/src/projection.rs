//! Authorized state slices and intent affordances over the native shared state.

use std::collections::{BTreeMap, BTreeSet};

use num_bigint::BigInt;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{json, Value as Json};

use crate::ir::{
    BinaryOp, BindingId, CheckedProgram, ExprId, ExprKind, Stmt, StmtKind, Transition,
};
use crate::runtime::{
    bind_params, eval_expression, validate_state, FailureKind, RuntimeError, State,
};
use crate::types::{ScalarType, ValueType};
use crate::value::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParameterBounds {
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub exact: Option<Value>,
    scalar: Option<ScalarType>,
    exact_json: Option<Json>,
}

impl ParameterBounds {
    fn new(ty: ValueType) -> Self {
        Self {
            min: None,
            max: None,
            exact: None,
            scalar: ty.as_scalar().cloned(),
            exact_json: None,
        }
    }
}

impl Serialize for ParameterBounds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (name, value) in [
            ("min", &self.min),
            ("max", &self.max),
            ("exact", &self.exact),
        ] {
            if let Some(value) = value {
                if let Some(scalar) = &self.scalar {
                    map.serialize_entry(name, &value.to_json(scalar))?;
                } else if name == "exact" {
                    if let Some(json) = &self.exact_json {
                        map.serialize_entry(name, json)?;
                    }
                }
            }
        }
        map.end()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Affordance {
    pub transition: String,
    pub enabled: bool,
    pub bounds: BTreeMap<String, ParameterBounds>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AffordanceFrame {
    pub projection: String,
    pub allowed: bool,
    pub state_slice: BTreeMap<String, Json>,
    pub affordances: Vec<Affordance>,
    pub reason: String,
}

impl AffordanceFrame {
    pub fn to_json(&self) -> Json {
        serde_json::to_value(self).expect("projection records contain valid JSON values")
    }
}

pub fn build_affordance_frame(
    program: &CheckedProgram,
    state: &State,
    name: &str,
    args: &BTreeMap<String, Value>,
) -> Result<AffordanceFrame, RuntimeError> {
    let id = program
        .projection_id(name)
        .ok_or_else(|| RuntimeError::new("D_PROJECTION", FailureKind::Input))?;
    let projection = program.projection(id);
    let bindings = bind_params(program, &projection.params, args)?;
    validate_state(program, state)?;
    let mut frame = AffordanceFrame {
        projection: name.into(),
        allowed: false,
        state_slice: BTreeMap::new(),
        affordances: Vec::new(),
        reason: String::new(),
    };
    if eval_expression(program, state, state, &bindings, projection.allow_if).ok()
        != Some(Value::Bool(true))
    {
        frame.reason = "allow_if evaluated to false".into();
        return Ok(frame);
    }
    frame.allowed = true;
    for slice in &projection.expose {
        let table = program.table(slice.column.table);
        let column = program.column(slice.column);
        let rows = &state.tables[table.id.0];
        let label = format!("{}.{}", table.name, column.name);
        if let Some(row) = slice.row {
            let value = eval_expression(program, state, state, &bindings, row)?;
            let key = value
                .as_u64()
                .ok_or_else(|| RuntimeError::new("ERR_ARITH_OVERFLOW", FailureKind::Trap))?;
            let live = rows.valid(table, key);
            let exposed = if live {
                let slot = (key & (table.capacity - 1)) as usize;
                crate::value::value_to_json(
                    program,
                    &rows.columns[column.id.index][slot],
                    &column.ty,
                )
            } else {
                Json::Null
            };
            frame.state_slice.insert(
                format!("{label}[{key}]"),
                json!({"id":key,"value":exposed,"live":live}),
            );
        } else {
            let mut exposed = Vec::new();
            for slot in 0..rows.occupied.len() {
                if let Some(key) = rows.key_at(table, slot) {
                    let mut row = serde_json::Map::new();
                    row.insert("id".into(), Json::from(key));
                    let value = crate::value::value_to_json(
                        program,
                        &rows.columns[column.id.index][slot],
                        &column.ty,
                    );
                    if column.name == "id" {
                        row.insert("columns".into(), json!({"id": value}));
                    } else {
                        row.insert(column.name.clone(), value);
                    }
                    exposed.push(Json::Object(row));
                }
            }
            frame.state_slice.insert(label, Json::Array(exposed));
        }
    }
    for guard in &projection.enable_intents {
        let transition = program.transition(guard.transition);
        let enabled = eval_expression(program, state, state, &bindings, guard.condition).ok()
            == Some(Value::Bool(true));
        let bounds = if enabled {
            derive_bounds(program, state, args, transition)
        } else {
            BTreeMap::new()
        };
        frame.affordances.push(Affordance {
            transition: transition.name.clone(),
            enabled,
            bounds,
            reason: String::new(),
        });
    }
    Ok(frame)
}

fn derive_bounds(
    program: &CheckedProgram,
    state: &State,
    args: &BTreeMap<String, Value>,
    transition: &Transition,
) -> BTreeMap<String, ParameterBounds> {
    let mut bounds: BTreeMap<_, _> = transition
        .params
        .iter()
        .map(|p| (p.name.clone(), ParameterBounds::new(p.ty.clone())))
        .collect();
    // A projection often supplies the intent's row handle but leaves values
    // such as amount/delta for the shell to collect. Do not invent values for
    // missing parameters while evaluating state-dependent boundary hints.
    let bindings: BTreeMap<_, _> = transition
        .params
        .iter()
        .filter_map(|p| {
            args.get(&p.name)
                .and_then(|v| crate::value::coerce_value(program, v, &p.ty).ok())
                .map(|v| (p.binding, v))
        })
        .collect();
    let parameters: BTreeMap<_, _> = transition
        .params
        .iter()
        .map(|p| (p.binding, p.name.as_str()))
        .collect();
    for require in &transition.requires {
        collect_bound(
            program,
            state,
            &bindings,
            &parameters,
            require.value,
            &mut bounds,
        );
    }
    bounds
}

fn collect_bound(
    program: &CheckedProgram,
    state: &State,
    bindings: &BTreeMap<BindingId, Value>,
    parameters: &BTreeMap<BindingId, &str>,
    expression: ExprId,
    bounds: &mut BTreeMap<String, ParameterBounds>,
) {
    let ExprKind::Binary { op, left, right } = program.expr(expression).kind else {
        return;
    };
    if op == BinaryOp::And {
        collect_bound(program, state, bindings, parameters, left, bounds);
        collect_bound(program, state, bindings, parameters, right, bounds);
        return;
    }
    if !matches!(
        op,
        BinaryOp::Ge | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Lt | BinaryOp::Eq
    ) {
        return;
    }
    let (binding, value_expression, reverse) =
        match (&program.expr(left).kind, &program.expr(right).kind) {
            (ExprKind::Binding(binding), _)
                if parameters.contains_key(binding)
                    && !contains_binding(program, right, *binding, &mut BTreeSet::new()) =>
            {
                (*binding, right, false)
            }
            (_, ExprKind::Binding(binding))
                if parameters.contains_key(binding)
                    && !contains_binding(program, left, *binding, &mut BTreeSet::new()) =>
            {
                (*binding, left, true)
            }
            _ => return,
        };
    let Ok(value) = eval_expression(program, state, state, bindings, value_expression) else {
        return;
    };
    let op = if reverse {
        match op {
            BinaryOp::Ge => BinaryOp::Le,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::Le => BinaryOp::Ge,
            BinaryOp::Lt => BinaryOp::Gt,
            other => other,
        }
    } else {
        op
    };
    let Some(bound) = bounds.get_mut(parameters[&binding]) else {
        return;
    };
    match op {
        BinaryOp::Eq => {
            bound.exact_json = Some(crate::value::value_to_json(
                program,
                &value,
                &program.binding(binding).ty,
            ));
            bound.exact = Some(value);
        }
        BinaryOp::Ge | BinaryOp::Gt => {
            let Ok(number) = value.int() else {
                return;
            };
            let number = if op == BinaryOp::Gt {
                number + BigInt::from(1u8)
            } else {
                number.clone()
            };
            if bound
                .min
                .as_ref()
                .and_then(|v| v.int().ok())
                .is_none_or(|v| v < &number)
            {
                bound.min = Some(Value::Int(number));
            }
        }
        BinaryOp::Le | BinaryOp::Lt => {
            let Ok(number) = value.int() else {
                return;
            };
            let number = if op == BinaryOp::Lt {
                number - BigInt::from(1u8)
            } else {
                number.clone()
            };
            if bound
                .max
                .as_ref()
                .and_then(|v| v.int().ok())
                .is_none_or(|v| v > &number)
            {
                bound.max = Some(Value::Int(number));
            }
        }
        _ => {}
    }
}

fn contains_binding(
    program: &CheckedProgram,
    expression: ExprId,
    needle: BindingId,
    seen: &mut BTreeSet<ExprId>,
) -> bool {
    if !seen.insert(expression) {
        return false;
    }
    let mut children = Vec::new();
    match &program.expr(expression).kind {
        ExprKind::Binding(binding) => return *binding == needle,
        ExprKind::Constant(_) | ExprKind::Sum { .. } | ExprKind::EffectCount => {}
        ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => {
            children.push(*index)
        }
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
        ExprKind::Record { fields, .. } => children.extend(fields.iter().map(|(_, value)| *value)),
        ExprKind::Field { record, .. } => children.push(*record),
        ExprKind::Variant { payload, .. } => children.push(*payload),
        ExprKind::IsVariant { value, .. } | ExprKind::Payload { value, .. } => {
            children.push(*value)
        }
        ExprKind::Call { args, .. } => children.extend(args),
        ExprKind::Unary { operand, .. } => children.push(*operand),
        ExprKind::Binary { left, right, .. } => {
            children.push(*left);
            children.push(*right);
        }
        ExprKind::Select { condition, yes, no } => {
            children.push(*condition);
            children.push(*yes);
            children.push(*no);
        }
        ExprKind::TableGet { key, .. }
        | ExprKind::SlotRead { key, .. }
        | ExprKind::TableExists { key, .. } => children.push(*key),
        ExprKind::UniqueLookup { value, .. } | ExprKind::KeyOf { value, .. } => {
            children.push(*value)
        }
        ExprKind::Fold(fold) => {
            children.extend([fold.init, fold.lo, fold.hi, fold.yield_value]);
            for stmt in &fold.body {
                statement_expressions(stmt, &mut children);
            }
        }
    }
    children
        .into_iter()
        .any(|child| contains_binding(program, child, needle, seen))
}

fn statement_expressions(statement: &Stmt, children: &mut Vec<ExprId>) {
    match &statement.kind {
        StmtKind::Let { value, .. } | StmtKind::Eval(value) => children.push(*value),
        StmtKind::Guard { condition, .. } => children.push(*condition),
        StmtKind::TableSet { key, value, .. } => {
            children.push(*key);
            children.push(*value);
        }
        StmtKind::Allocate { fields, .. } => {
            children.extend(fields.iter().map(|(_, expression)| *expression))
        }
        StmtKind::Emit { args, .. } => children.extend(args),
        StmtKind::Commit => {}
    }
}
