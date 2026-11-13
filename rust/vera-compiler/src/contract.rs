//! Parser-backed contract snapshots for a trusted caller's frozen specification.
//!
//! These snapshots preserve source-order and all declarations, omitting only
//! explicitly selected implementation bodies. Identity is conservative syntax
//! identity, not semantic equivalence or a proof of implementation correctness.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{analyze, ast::Decl, parse_source, Diagnostic, Span};

pub const CONTRACT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BodyTarget {
    Transition { name: String },
    Function { name: String },
    Operation { component: String, name: String },
}

/// Keep this complete document outside the executor's write authority. Internal
/// consistency checks cannot authenticate who approved a replacement snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenContract {
    pub schema_version: u32,
    /// Retained so deserialized snapshots can be parsed and independently checked.
    pub source: String,
    pub editable_bodies: Vec<BodyTarget>,
    /// Canonical span-free AST; only selected body fields are omitted.
    pub canonical: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContractDifference {
    /// RFC 6901 JSON pointer into the canonical contract AST.
    pub path: String,
    /// None distinguishes a missing entry from a JSON null value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContractComparison {
    pub schema_version: u32,
    pub matches: bool,
    pub differences: Vec<ContractDifference>,
}

fn error(message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: "D_FROZEN_CONTRACT".into(),
        message: message.into(),
        span: Span::default(),
    }
}

fn calls(value: &Value, names: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            if fields.get("node").and_then(Value::as_str) == Some("Call") {
                if let Some(name) = fields.get("function").and_then(Value::as_str) {
                    names.insert(name.into());
                }
            }
            for value in fields.values() {
                calls(value, names);
            }
        }
        Value::Array(values) => {
            for value in values {
                calls(value, names);
            }
        }
        _ => {}
    }
}

fn canonical(source: &str, editable: &[BodyTarget]) -> Result<Value, Diagnostic> {
    let module = parse_source(source)?;
    // Invalid types, duplicate declarations, footprints and names must not enter
    // a trusted snapshot or be admitted just because JSON happens to match.
    analyze(module.clone()).map_err(|diagnostics| {
        error(format!(
            "semantic analysis failed: {}",
            diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        ))
    })?;
    let mut wanted = BTreeSet::new();
    for target in editable {
        if !wanted.insert(target.clone()) {
            return Err(error("editable body target is listed more than once"));
        }
    }
    let mut remaining = wanted.clone();
    let mut result = module.canonical_json();
    let declarations = result["decls"]
        .as_array_mut()
        .expect("Module canonical serialization has declarations");
    for (declaration, value) in module.decls.iter().zip(declarations) {
        match declaration {
            Decl::Transition(transition) => {
                let target = BodyTarget::Transition {
                    name: transition.name.clone(),
                };
                if remaining.remove(&target) {
                    value.as_object_mut().unwrap().remove("body");
                }
            }
            Decl::Function(function) => {
                let target = BodyTarget::Function {
                    name: function.name.clone(),
                };
                if remaining.remove(&target) {
                    value.as_object_mut().unwrap().remove("body");
                }
            }
            Decl::Component(component) => {
                let operations = value["operations"].as_array_mut().unwrap();
                for (operation, value) in component.operations.iter().zip(operations) {
                    let target = BodyTarget::Operation {
                        component: component.name.clone(),
                        name: operation.transition.name.clone(),
                    };
                    if remaining.remove(&target) {
                        value["contract"].as_object_mut().unwrap().remove("body");
                    }
                }
            }
            _ => {}
        }
    }
    if !remaining.is_empty() {
        return Err(error(format!(
            "unknown editable body targets: {remaining:?}"
        )));
    }
    // An editable helper called from a protected contract (or from another
    // protected body) could silently change the meaning of that frozen code.
    // Every noneditable body is retained, so collecting its calls also covers
    // transitive helper chains without trusting a regex or inferred purity.
    let mut protected_calls = BTreeSet::new();
    calls(&result, &mut protected_calls);
    for target in wanted {
        if let BodyTarget::Function { name } = target {
            if protected_calls.contains(&name) {
                return Err(error(format!(
                    "function {name} is referenced by protected syntax and its body cannot be editable"
                )));
            }
        }
    }
    Ok(result)
}

/// Enumerate a safe default policy: all transition/operation bodies and only
/// function bodies not referenced (even transitively) by protected contracts.
/// This reports permission candidates, not an authorization from the user.
pub fn eligible_bodies(source: &str) -> Result<Vec<BodyTarget>, Diagnostic> {
    let module = parse_source(source)?;
    analyze(module.clone()).map_err(|diagnostics| {
        error(format!(
            "semantic analysis failed: {}",
            diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        ))
    })?;
    let mut root = module.canonical_json();
    let mut targets = Vec::new();
    let mut function_calls = std::collections::BTreeMap::new();
    for (decl, value) in module
        .decls
        .iter()
        .zip(root["decls"].as_array_mut().unwrap())
    {
        match decl {
            Decl::Transition(transition) => {
                targets.push(BodyTarget::Transition {
                    name: transition.name.clone(),
                });
                value.as_object_mut().unwrap().remove("body");
            }
            Decl::Function(function) => {
                targets.push(BodyTarget::Function {
                    name: function.name.clone(),
                });
                let body = value.as_object_mut().unwrap().remove("body").unwrap();
                let mut dependencies = BTreeSet::new();
                calls(&body, &mut dependencies);
                function_calls.insert(function.name.clone(), dependencies);
            }
            Decl::Component(component) => {
                for (operation, value) in component
                    .operations
                    .iter()
                    .zip(value["operations"].as_array_mut().unwrap())
                {
                    targets.push(BodyTarget::Operation {
                        component: component.name.clone(),
                        name: operation.transition.name.clone(),
                    });
                    value["contract"].as_object_mut().unwrap().remove("body");
                }
            }
            _ => {}
        }
    }
    let mut protected = BTreeSet::new();
    calls(&root, &mut protected);
    let mut pending: Vec<_> = protected.iter().cloned().collect();
    while let Some(name) = pending.pop() {
        if let Some(dependencies) = function_calls.get(&name) {
            for dependency in dependencies {
                if protected.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
        }
    }
    targets.retain(
        |target| !matches!(target, BodyTarget::Function { name } if protected.contains(name)),
    );
    targets.sort();
    Ok(targets)
}

/// Freeze a valid program, protecting every AST field except explicitly selected
/// transition/function/operation bodies. No body is editable by default.
pub fn freeze_source(
    source: &str,
    editable_bodies: &[BodyTarget],
) -> Result<FrozenContract, Diagnostic> {
    let canonical = canonical(source, editable_bodies)?;
    let mut editable_bodies = editable_bodies.to_vec();
    editable_bodies.sort();
    Ok(FrozenContract {
        schema_version: CONTRACT_SCHEMA_VERSION,
        source: source.into(),
        editable_bodies,
        canonical,
    })
}

impl FrozenContract {
    /// Validate version, syntax, semantic analysis, policy and derived canonical
    /// data. Authentication and immutable storage remain the caller's duty.
    pub fn validate(&self) -> Result<(), Diagnostic> {
        if self.schema_version != CONTRACT_SCHEMA_VERSION {
            return Err(error("unsupported frozen contract schema version"));
        }
        let rebuilt = freeze_source(&self.source, &self.editable_bodies)?;
        if rebuilt != *self {
            return Err(error(
                "frozen contract is not its source-derived canonical snapshot",
            ));
        }
        Ok(())
    }
}

fn pointer(parent: &str, part: &str) -> String {
    format!("{parent}/{}", part.replace('~', "~0").replace('/', "~1"))
}

fn differences(expected: &Value, actual: &Value, path: &str, out: &mut Vec<ContractDifference>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<_> = left.keys().chain(right.keys()).collect();
            for key in keys {
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &pointer(path, key), out),
                    (left, right) => out.push(ContractDifference {
                        path: pointer(path, key),
                        expected: left.cloned(),
                        actual: right.cloned(),
                    }),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let path = pointer(path, &index.to_string());
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => differences(left, right, &path, out),
                    (left, right) => out.push(ContractDifference {
                        path,
                        expected: left.cloned(),
                        actual: right.cloned(),
                    }),
                }
            }
        }
        _ if expected != actual => out.push(ContractDifference {
            path: path.into(),
            expected: Some(expected.clone()),
            actual: Some(actual.clone()),
        }),
        _ => {}
    }
}

/// Check valid candidate syntax against a caller-controlled frozen specification.
/// A match permits body editing, not execution: verification is still required.
pub fn compare_source(
    frozen: &FrozenContract,
    candidate: &str,
) -> Result<ContractComparison, Diagnostic> {
    frozen.validate()?;
    let actual = canonical(candidate, &frozen.editable_bodies)?;
    let mut changes = Vec::new();
    differences(&frozen.canonical, &actual, "", &mut changes);
    Ok(ContractComparison {
        schema_version: CONTRACT_SCHEMA_VERSION,
        matches: changes.is_empty(),
        differences: changes,
    })
}
