//! Resolve and type-check syntax once into the immutable compiler IR.
//!
//! Backends consume declaration identities, contextual constants and evaluation
//! domains from this pass. They never resolve source names or infer types.

use std::collections::{BTreeMap, BTreeSet};

use num_bigint::BigInt;

use crate::ast::{self as a, LiteralValue};
use crate::diagnostic::{Diagnostic, Span};
use crate::ir::{self as i, *};
use crate::types::{compute_layout, Domain, ScalarType};
use crate::value::{literal_in_domain, symbol_id, Value};

type Result<T> = std::result::Result<T, Diagnostic>;
type Scope = BTreeMap<String, BindingId>;

/// Invalid or partially resolved programs never cross this boundary.
pub fn analyze(module: a::Module) -> std::result::Result<CheckedProgram, Vec<Diagnostic>> {
    Analyzer::new(module).run().map_err(|error| vec![error])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purity {
    Body,
    Specification,
    OldKey,
}

#[derive(Clone, Copy)]
struct Context {
    state: Snapshot,
    purity: Purity,
    fold_depth: u32,
    fold_product: u32,
    commit: bool,
    outbox: bool,
}
impl Context {
    fn specification(state: Snapshot) -> Self {
        Self {
            state,
            purity: Purity::Specification,
            fold_depth: 0,
            fold_product: 1,
            commit: false,
            outbox: false,
        }
    }
    fn body() -> Self {
        Self {
            state: Snapshot::Current,
            purity: Purity::Body,
            fold_depth: 0,
            fold_product: 1,
            commit: true,
            outbox: false,
        }
    }
}

#[derive(Default)]
struct FoldStats {
    depth: u32,
    product: u32,
}

struct Lowered {
    id: ExprId,
    cost: u64,
}

struct Analyzer {
    program: CheckedProgram,
    table_names: BTreeMap<String, TableId>,
    effect_names: BTreeMap<String, EffectId>,
    transition_names: BTreeMap<String, TransitionId>,
    function_names: BTreeMap<String, FunctionId>,
    stats: FoldStats,
}

fn diagnostic(code: &str, message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        message: message.into(),
        span,
    }
}

fn add_cost(a: u64, b: u64, span: Span) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| diagnostic("D_WCET_OVERFLOW", "instruction bound exceeds u64", span))
}

impl Analyzer {
    fn new(module: a::Module) -> Self {
        Self {
            program: CheckedProgram::empty(module),
            table_names: BTreeMap::new(),
            effect_names: BTreeMap::new(),
            transition_names: BTreeMap::new(),
            function_names: BTreeMap::new(),
            stats: FoldStats::default(),
        }
    }

    fn run(mut self) -> Result<CheckedProgram> {
        let declarations = self.program.syntax.decls.clone();
        self.collect_tables(&declarations)?;
        for declaration in &declarations {
            if let a::Decl::Effect(effect) = declaration {
                self.collect_effect(effect)?;
            }
            if let a::Decl::Transition(transition) = declaration {
                if self.transition_names.contains_key(&transition.name) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        format!("transition {:?} declared twice", transition.name),
                        transition.span,
                    ));
                }
                let id = TransitionId(self.transition_names.len());
                self.transition_names.insert(transition.name.clone(), id);
            }
        }
        self.functions(&declarations)?;
        let mut invariant_names = BTreeSet::new();
        for declaration in &declarations {
            if let a::Decl::Invariant(invariant) = declaration {
                if !invariant_names.insert(invariant.name.clone()) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        format!("invariant {:?} declared twice", invariant.name),
                        invariant.span,
                    ));
                }
                self.invariant(invariant)?;
            }
        }
        for declaration in &declarations {
            if let a::Decl::Transition(transition) = declaration {
                self.transition(transition)?;
            }
        }
        let mut projection_names = BTreeSet::new();
        for declaration in &declarations {
            if let a::Decl::Projection(projection) = declaration {
                if !projection_names.insert(projection.name.clone()) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        format!("projection {:?} declared twice", projection.name),
                        projection.span,
                    ));
                }
                self.projection(projection)?;
            }
        }
        Ok(self.program)
    }

    fn functions(&mut self, declarations: &[a::Decl]) -> Result<()> {
        let mut functions = BTreeMap::new();
        for declaration in declarations {
            if let a::Decl::Function(function) = declaration {
                if functions.insert(function.name.clone(), function).is_some() {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        format!("function {:?} declared twice", function.name),
                        function.span,
                    ));
                }
                if matches!(
                    function.name.as_str(),
                    "effect_count" | "effect_is" | "effect_arg"
                ) {
                    return Err(diagnostic(
                        "D_FUNCTION_NAME",
                        "function name conflicts with an effect observation",
                        function.span,
                    ));
                }
                if function.params.iter().any(|p| p.name == "result") {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        "result is reserved for the function postcondition value",
                        function.span,
                    ));
                }
            }
        }
        let mut dependencies = BTreeMap::new();
        for (name, function) in &functions {
            for (_, expression) in function.requires.iter().chain(&function.ensures) {
                function_expression_dependencies(expression, false, &mut BTreeSet::new())?;
            }
            let mut names = BTreeSet::new();
            function_expression_dependencies(&function.body, true, &mut names)?;
            for dependency in &names {
                if !functions.contains_key(dependency) {
                    return Err(diagnostic(
                        "D_UNKNOWN_FUNCTION",
                        format!("unknown function {dependency:?}"),
                        function.span,
                    ));
                }
            }
            dependencies.insert(name.clone(), names);
        }
        // Kahn's algorithm keeps dependency resolution stack-safe and linear in
        // graph edges, with deterministic lexical tie breaking.
        let mut pending: BTreeMap<String, usize> = dependencies
            .iter()
            .map(|(name, deps)| (name.clone(), deps.len()))
            .collect();
        let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, deps) in &dependencies {
            for dependency in deps {
                users
                    .entry(dependency.clone())
                    .or_default()
                    .push(name.clone());
            }
        }
        let mut ready: BTreeSet<String> = pending
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(name, _)| name.clone())
            .collect();
        let mut ordered = Vec::new();
        while let Some(name) = ready.pop_first() {
            if let Some(callers) = users.get(&name) {
                for caller in callers {
                    let count = pending.get_mut(caller).expect("known function dependency");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(caller.clone());
                    }
                }
            }
            ordered.push(name);
        }
        if ordered.len() != functions.len() {
            let name = pending.iter().find(|(_, count)| **count != 0).unwrap().0;
            return Err(diagnostic(
                "D_FUNCTION_CYCLE",
                format!("recursive function dependency involving {name:?}"),
                functions[name].span,
            ));
        }
        for name in ordered {
            let function = functions[&name];
            self.stats = FoldStats {
                depth: 0,
                product: 1,
            };
            let (params, scope) = self.parameters(&function.params)?;
            let result_type = self.resolve_type(&function.result_type)?;
            let mut requires = Vec::new();
            let mut ensures = Vec::new();
            let mut cost = 0;
            let mut requirement_names = BTreeSet::new();
            for (name, expression) in &function.requires {
                if !requirement_names.insert(name) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        "function requirement declared twice",
                        expression.span(),
                    ));
                }
                let lowered = self.expression(
                    expression,
                    &scope,
                    Some(Domain::scalar(ScalarType::Bool)),
                    Context::specification(Snapshot::Current),
                )?;
                cost = add_cost(cost, lowered.cost, function.span)?;
                requires.push(NamedExpr {
                    name: name.clone(),
                    value: lowered.id,
                });
            }
            let body = self.expression(
                &function.body,
                &scope,
                Some(Domain::scalar(result_type.clone())),
                Context::specification(Snapshot::Current),
            )?;
            cost = add_cost(cost, body.cost, function.span)?;
            let mut result_scope = scope;
            let result = self.bind(
                &mut result_scope,
                "result",
                result_type.clone(),
                BindingOrigin::Local,
                false,
                function.span,
            )?;
            let mut ensure_names = BTreeSet::new();
            for (name, expression) in &function.ensures {
                if !ensure_names.insert(name) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        "function ensure declared twice",
                        expression.span(),
                    ));
                }
                let lowered = self.expression(
                    expression,
                    &result_scope,
                    Some(Domain::scalar(ScalarType::Bool)),
                    Context::specification(Snapshot::Current),
                )?;
                ensures.push(NamedExpr {
                    name: name.clone(),
                    value: lowered.id,
                });
            }
            let id = FunctionId(self.program.functions.len());
            let deps = dependencies[&name]
                .iter()
                .map(|name| self.function_names[name])
                .collect();
            self.program.functions.push(Function {
                id,
                name: name.clone(),
                params,
                result,
                result_type,
                requires,
                ensures,
                body: body.id,
                dependencies: deps,
                wcet_bound: cost,
                fold_depth: self.stats.depth,
                max_fold_product: self.stats.product,
                span: function.span,
            });
            self.function_names.insert(name, id);
        }
        Ok(())
    }

    fn collect_tables(&mut self, declarations: &[a::Decl]) -> Result<()> {
        for declaration in declarations {
            let a::Decl::Table(table) = declaration else {
                continue;
            };
            if self.table_names.contains_key(&table.name) {
                return Err(diagnostic(
                    "D_DUPLICATE",
                    format!("table {:?} declared twice", table.name),
                    table.span,
                ));
            }
            let capacity = table
                .capacity
                .as_decimal()
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0 && *n <= 1u64 << 32 && n.is_power_of_two())
                .ok_or_else(|| {
                    diagnostic(
                        "D_CAPACITY",
                        "table capacity must be a power of two between 1 and 2^32",
                        table.span,
                    )
                })?;
            if table.key_type.name != "id64"
                || table.key_type.type_args.len() != 1
                || table.key_type.type_args[0].name != table.name
            {
                return Err(diagnostic(
                    "D_BAD_KEY",
                    format!("table {} key must be id64<{}>", table.name, table.name),
                    table.key_type.span,
                ));
            }
            let id = TableId(self.program.tables.len());
            self.table_names.insert(table.name.clone(), id);
            self.program.tables.push(Table {
                id,
                name: table.name.clone(),
                key_name: table.key_name.clone(),
                capacity,
                columns: Vec::new(),
                layout: Default::default(),
                span: table.span,
            });
        }
        // All table identities exist before resolving forward/self references.
        for declaration in declarations {
            let a::Decl::Table(table) = declaration else {
                continue;
            };
            let id = self.table_names[&table.name];
            let mut columns = Vec::new();
            let mut names = BTreeSet::new();
            for column in &table.columns {
                if !names.insert(column.name.clone()) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        format!("column {}.{} declared twice", table.name, column.name),
                        column.span,
                    ));
                }
                let ty = self.resolve_type(&column.ty)?;
                if column.tracked_sum && !ty.is_numeric() {
                    return Err(diagnostic(
                        "D_BAD_OPERAND",
                        "tracked_sum requires a numeric column",
                        column.span,
                    ));
                }
                columns.push(Column {
                    id: ColumnId {
                        table: id,
                        index: columns.len(),
                    },
                    name: column.name.clone(),
                    ty,
                    tracked_sum: column.tracked_sum,
                    unique: column.unique,
                    span: column.span,
                });
            }
            let layout_columns = columns
                .iter()
                .map(|column| {
                    (
                        column.name.clone(),
                        column.ty.clone(),
                        column.tracked_sum,
                        column.unique,
                    )
                })
                .collect::<Vec<_>>();
            let layout = compute_layout(self.program.table(id).capacity, &layout_columns)
                .map_err(|message| diagnostic("D_CAPACITY", message, table.span))?;
            self.program.tables[id.0].columns = columns;
            self.program.tables[id.0].layout = layout;
        }
        Ok(())
    }

    fn resolve_type(&self, reference: &a::TypeRef) -> Result<ScalarType> {
        Ok(match reference.name.as_str() {
            "bool" => ScalarType::Bool,
            "u8" => ScalarType::U8,
            "u16" => ScalarType::U16,
            "u32" => ScalarType::U32,
            "u64" => ScalarType::U64,
            "i64" => ScalarType::I64,
            "dec64" => ScalarType::Dec64,
            "dec128" => ScalarType::Dec128,
            "sym32" => ScalarType::Sym32,
            "bytes32" => ScalarType::Bytes32,
            "id64" => {
                if reference.type_args.len() != 1 {
                    return Err(diagnostic(
                        "D_BAD_KEY",
                        "id64 requires exactly one table argument",
                        reference.span,
                    ));
                }
                ScalarType::Id64(self.table_id(&reference.type_args[0].name, reference.span)?)
            }
            "enum" => {
                if reference.enum_tags.is_empty() || reference.enum_tags.len() > 256 {
                    return Err(diagnostic(
                        "D_ENUM_TAG",
                        "enum requires 1 to 256 tags",
                        reference.span,
                    ));
                }
                let unique = reference.enum_tags.iter().collect::<BTreeSet<_>>();
                if unique.len() != reference.enum_tags.len() {
                    return Err(diagnostic(
                        "D_ENUM_TAG",
                        "enum tags must be distinct",
                        reference.span,
                    ));
                }
                ScalarType::Enum(reference.enum_tags.clone())
            }
            _ => {
                return Err(diagnostic(
                    "D_TYPE_MISMATCH",
                    format!("unknown type {:?}", reference.name),
                    reference.span,
                ))
            }
        })
    }

    fn table_id(&self, name: &str, span: Span) -> Result<TableId> {
        self.table_names
            .get(name)
            .copied()
            .ok_or_else(|| diagnostic("D_UNKNOWN_TABLE", format!("unknown table {name:?}"), span))
    }

    fn column_id(&self, table: &str, name: &str, span: Span) -> Result<ColumnId> {
        let table = self.table_id(table, span)?;
        self.program.table(table).column_id(name).ok_or_else(|| {
            diagnostic(
                "D_UNKNOWN_COLUMN",
                format!("unknown column {}.{name}", self.program.table(table).name),
                span,
            )
        })
    }

    fn bind(
        &mut self,
        scope: &mut Scope,
        name: &str,
        ty: ScalarType,
        origin: BindingOrigin,
        state_dependent: bool,
        span: Span,
    ) -> Result<BindingId> {
        if scope.contains_key(name) {
            return Err(diagnostic(
                "D_DUPLICATE",
                format!("binding {name:?} is already defined"),
                span,
            ));
        }
        let id = BindingId(self.program.bindings.len());
        self.program.bindings.push(Binding {
            id,
            name: name.into(),
            ty,
            origin,
            state_dependent,
            span,
        });
        scope.insert(name.into(), id);
        Ok(id)
    }

    fn parameters(&mut self, parameters: &[a::Param]) -> Result<(Vec<Param>, Scope)> {
        let mut output = Vec::new();
        let mut scope = Scope::new();
        for parameter in parameters {
            let ty = self.resolve_type(&parameter.ty)?;
            let binding = self.bind(
                &mut scope,
                &parameter.name,
                ty.clone(),
                BindingOrigin::Parameter,
                false,
                parameter.span,
            )?;
            output.push(Param {
                name: parameter.name.clone(),
                binding,
                ty,
                span: parameter.span,
            });
        }
        Ok((output, scope))
    }

    fn collect_effect(&mut self, effect: &a::EffectDecl) -> Result<()> {
        if self.effect_names.contains_key(&effect.name) {
            return Err(diagnostic(
                "D_DUPLICATE",
                format!("effect {:?} declared twice", effect.name),
                effect.span,
            ));
        }
        let (params, _) = self.parameters(&effect.params)?;
        let id = EffectId(self.program.effects.len());
        self.effect_names.insert(effect.name.clone(), id);
        self.program.effects.push(Effect {
            id,
            name: effect.name.clone(),
            params,
            span: effect.span,
        });
        Ok(())
    }

    fn invariant(&mut self, invariant: &a::InvariantDecl) -> Result<()> {
        let mut scope = Scope::new();
        let quantifier = match (&invariant.quant_table, &invariant.quant_var) {
            (None, None) => None,
            (Some(table), Some(name)) => {
                let table = self.table_id(table, invariant.span)?;
                let binding = self.bind(
                    &mut scope,
                    name,
                    ScalarType::Id64(table),
                    BindingOrigin::Quantifier,
                    false,
                    invariant.span,
                )?;
                Some((table, binding))
            }
            _ => {
                return Err(diagnostic(
                    "D_QUANTIFIER",
                    "row quantifier requires both table and variable",
                    invariant.span,
                ))
            }
        };
        let expression = self.expression(
            &invariant.expr,
            &scope,
            Some(Domain::scalar(ScalarType::Bool)),
            Context::specification(Snapshot::Current),
        )?;
        self.program.invariants.push(Invariant {
            name: invariant.name.clone(),
            quantifier,
            condition: expression.id,
            span: invariant.span,
        });
        Ok(())
    }

    fn projection(&mut self, projection: &a::ProjectionDecl) -> Result<()> {
        let (params, scope) = self.parameters(&projection.params)?;
        let context = Context::specification(Snapshot::Current);
        let allow_if = if let Some(expression) = &projection.allow_if {
            self.expression(
                expression,
                &scope,
                Some(Domain::scalar(ScalarType::Bool)),
                context,
            )?
            .id
        } else {
            self.push(
                Domain::scalar(ScalarType::Bool),
                ExprKind::Constant(Value::Bool(true)),
                false,
                projection.span,
            )
        };
        let mut expose = Vec::new();
        for slice in &projection.expose {
            let column = self.column_id(&slice.table, &slice.column, slice.span)?;
            let row = slice
                .row
                .as_ref()
                .map(|key| self.key(key, column.table, &scope, context))
                .transpose()?
                .map(|key| key.id);
            expose.push(Slice { column, row });
        }
        let mut enable_intents = Vec::new();
        for guard in &projection.enable_intents {
            let transition = self
                .transition_names
                .get(&guard.transition)
                .copied()
                .ok_or_else(|| {
                    diagnostic(
                        "D_UNKNOWN_TRANSITION",
                        format!("unknown transition {:?}", guard.transition),
                        guard.span,
                    )
                })?;
            let condition = self
                .expression(
                    &guard.condition,
                    &scope,
                    Some(Domain::scalar(ScalarType::Bool)),
                    context,
                )?
                .id;
            enable_intents.push(i::IntentGuard {
                transition,
                condition,
            });
        }
        let id = ProjectionId(self.program.projections.len());
        self.program.projections.push(Projection {
            id,
            name: projection.name.clone(),
            params,
            allow_if,
            expose,
            enable_intents,
            span: projection.span,
        });
        Ok(())
    }

    fn transition(&mut self, transition: &a::TransitionDecl) -> Result<()> {
        let commits = transition
            .body
            .iter()
            .filter(|statement| matches!(statement, a::Stmt::Commit(_)))
            .count();
        if commits != 1 || !matches!(transition.body.last(), Some(a::Stmt::Commit(_))) {
            return Err(diagnostic(
                "D_TYPE_MISMATCH",
                "transition must end with exactly one commit",
                transition.span,
            ));
        }
        self.stats = FoldStats {
            depth: 0,
            product: 1,
        };
        let (params, scope) = self.parameters(&transition.params)?;
        let reads = self.accesses(&transition.reads, &scope)?;
        let writes = self.accesses(&transition.writes, &scope)?;
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        for (name, expression) in &transition.requires {
            let lowered = self.expression(
                expression,
                &scope,
                Some(Domain::scalar(ScalarType::Bool)),
                Context::specification(Snapshot::Current),
            )?;
            requires.push(NamedExpr {
                name: name.clone(),
                value: lowered.id,
            });
        }
        // Postconditions intentionally have parameter scope, not body SSA scope.
        for (name, expression) in &transition.ensures {
            let lowered = self.expression(
                expression,
                &scope,
                Some(Domain::scalar(ScalarType::Bool)),
                Context {
                    outbox: true,
                    ..Context::specification(Snapshot::Current)
                },
            )?;
            ensures.push(NamedExpr {
                name: name.clone(),
                value: lowered.id,
            });
        }
        let mut body_scope = scope;
        let mut body = Vec::new();
        let mut wcet_bound = 0;
        for statement in &transition.body {
            let (lowered, cost) = self.statement(statement, &mut body_scope, Context::body())?;
            wcet_bound = add_cost(wcet_bound, cost, transition.span)?;
            body.push(lowered);
        }
        let mut actual_reads = Vec::new();
        let mut actual_writes = Vec::new();
        let mut tier_two = false;
        for require in &requires {
            self.collect_reads(require.value, &mut actual_reads, &mut tier_two);
        }
        self.collect_statements(&body, &mut actual_reads, &mut actual_writes, &mut tier_two);
        for access in &actual_reads {
            if !self.covered(&reads, access) {
                return Err(diagnostic(
                    "D_ACCESS_VIOLATION",
                    format!(
                        "transition {:?} reads undeclared column {}.{}",
                        transition.name,
                        self.program.table(access.column.table).name,
                        self.program.column(access.column).name
                    ),
                    access.span,
                ));
            }
        }
        for access in &actual_writes {
            if !self.covered(&writes, access) {
                return Err(diagnostic(
                    "D_ACCESS_VIOLATION",
                    format!(
                        "transition {:?} writes undeclared column {}.{}",
                        transition.name,
                        self.program.table(access.column.table).name,
                        self.program.column(access.column).name
                    ),
                    access.span,
                ));
            }
        }
        let tier = if tier_two
            || actual_reads
                .iter()
                .chain(&actual_writes)
                .any(|access| access.tier == 2)
        {
            2
        } else {
            1
        };
        self.program.transitions.push(Transition {
            id: self.transition_names[&transition.name],
            name: transition.name.clone(),
            params,
            reads: actual_reads,
            writes: actual_writes,
            requires,
            ensures,
            body,
            wcet_bound,
            fold_depth: self.stats.depth,
            max_fold_product: self.stats.product,
            tier,
            span: transition.span,
        });
        Ok(())
    }

    fn accesses(&mut self, paths: &[a::AccessPath], scope: &Scope) -> Result<Vec<Access>> {
        let mut output = Vec::new();
        for path in paths {
            let column = self.column_id(&path.table, &path.column, path.span)?;
            let key = path
                .key
                .as_ref()
                .map(|expression| {
                    self.key(
                        expression,
                        column.table,
                        scope,
                        Context::specification(Snapshot::Current),
                    )
                })
                .transpose()?
                .map(|lowered| lowered.id);
            let tier = if key.is_some_and(|id| self.program.expr(id).state_dependent) {
                2
            } else {
                1
            };
            output.push(Access {
                column,
                key,
                tier,
                span: path.span,
            });
        }
        Ok(output)
    }

    fn push(
        &mut self,
        domain: Domain,
        kind: ExprKind,
        state_dependent: bool,
        span: Span,
    ) -> ExprId {
        let id = ExprId(self.program.expressions.len());
        self.program.expressions.push(TypedExpr {
            domain,
            kind,
            state_dependent,
            span,
        });
        id
    }

    fn key(
        &mut self,
        expression: &a::Expr,
        table: TableId,
        scope: &Scope,
        context: Context,
    ) -> Result<Lowered> {
        let lowered = self.expression(expression, scope, None, context)?;
        if self.program.expr(lowered.id).domain.scalar != ScalarType::Id64(table) {
            return Err(diagnostic(
                "D_TYPE_MISMATCH",
                format!("row key requires id64<{}>", self.program.table(table).name),
                expression.span(),
            ));
        }
        Ok(lowered)
    }

    fn expression(
        &mut self,
        expression: &a::Expr,
        scope: &Scope,
        expected: Option<Domain>,
        context: Context,
    ) -> Result<Lowered> {
        let lowered = self.expression_inner(expression, scope, expected.clone(), context)?;
        if let Some(expected) = expected {
            let actual = &self.program.expr(lowered.id).domain.scalar;
            if actual != &expected.scalar {
                return Err(diagnostic(
                    "D_TYPE_MISMATCH",
                    format!("expected {}, got {actual}", expected.scalar),
                    expression.span(),
                ));
            }
        }
        Ok(lowered)
    }

    fn expression_inner(
        &mut self,
        expression: &a::Expr,
        scope: &Scope,
        expected: Option<Domain>,
        context: Context,
    ) -> Result<Lowered> {
        let span = expression.span();
        let (domain, kind, dependent, cost) = match expression {
            a::Expr::Literal(literal) => {
                let domain = self.literal_domain(&literal.value, expected, span)?;
                if let LiteralValue::String(text) = &literal.value {
                    if domain.scalar == ScalarType::Sym32 {
                        let code = symbol_id(text);
                        if self
                            .program
                            .symbols
                            .get(&code)
                            .is_some_and(|previous| previous != text)
                        {
                            return Err(diagnostic(
                                "D_LITERAL_RANGE",
                                "distinct symbol strings have the same sym32 encoding",
                                span,
                            ));
                        }
                        self.program.symbols.insert(code, text.clone());
                    }
                }
                let value = literal_in_domain(&literal.value, &domain)
                    .map_err(|message| diagnostic("D_LITERAL_RANGE", message, span))?;
                (domain, ExprKind::Constant(value), false, 0)
            }
            a::Expr::Var(variable) => {
                if let Some(binding) = scope.get(&variable.name).copied() {
                    let info = self.program.binding(binding);
                    let domain = Self::promote(Domain::scalar(info.ty.clone()), expected.as_ref());
                    (domain, ExprKind::Binding(binding), info.state_dependent, 0)
                } else if let Some(Domain {
                    scalar: ScalarType::Enum(tags),
                    ..
                }) = &expected
                {
                    let index = tags
                        .iter()
                        .position(|tag| tag == &variable.name)
                        .ok_or_else(|| {
                            diagnostic(
                                "D_UNKNOWN_PARAM",
                                format!("unknown variable {:?}", variable.name),
                                span,
                            )
                        })?;
                    (
                        expected.clone().expect("enum context exists"),
                        ExprKind::Constant(Value::Int(BigInt::from(index))),
                        false,
                        0,
                    )
                } else {
                    return Err(diagnostic(
                        "D_UNKNOWN_PARAM",
                        format!("unknown variable {:?}", variable.name),
                        span,
                    ));
                }
            }
            a::Expr::Unary(unary) => {
                let (op, hint) = match unary.op.as_str() {
                    "not" => (UnaryOp::Not, Some(Domain::scalar(ScalarType::Bool))),
                    "neg" => (UnaryOp::Neg, expected.clone()),
                    _ => return Err(diagnostic("D_BAD_OPERAND", "unknown unary operation", span)),
                };
                let operand = self.expression(&unary.operand, scope, hint, context)?;
                let domain = self.program.expr(operand.id).domain.clone();
                if op == UnaryOp::Neg && !domain.signed() {
                    return Err(diagnostic(
                        "D_BAD_OPERAND",
                        "neg requires a signed numeric operand",
                        span,
                    ));
                }
                (
                    domain,
                    ExprKind::Unary {
                        op,
                        operand: operand.id,
                    },
                    self.program.expr(operand.id).state_dependent,
                    operand.cost,
                )
            }
            a::Expr::Binary(binary) => return self.binary(binary, scope, expected, context),
            a::Expr::Select(select) => {
                let condition = self.expression(
                    &select.cond,
                    scope,
                    Some(Domain::scalar(ScalarType::Bool)),
                    context,
                )?;
                let mut target = expected
                    .clone()
                    .or(self.concrete(&select.a, scope)?)
                    .or(self.concrete(&select.b, scope)?);
                if let Some(domain) = &mut target {
                    for branch in [&select.a, &select.b] {
                        if let Some(other) = self.concrete(branch, scope)? {
                            if domain.scalar == other.scalar {
                                domain.width = domain.width.max(other.width);
                            }
                        }
                    }
                }
                let yes = self.expression(&select.a, scope, target, context)?;
                let yes_domain = self.program.expr(yes.id).domain.clone();
                let no = self.expression(&select.b, scope, Some(yes_domain.clone()), context)?;
                let no_domain = &self.program.expr(no.id).domain;
                let domain = Domain {
                    scalar: yes_domain.scalar,
                    width: yes_domain.width.max(no_domain.width),
                };
                let dependent = [condition.id, yes.id, no.id]
                    .iter()
                    .any(|id| self.program.expr(*id).state_dependent);
                let cost = add_cost(add_cost(condition.cost, yes.cost, span)?, no.cost, span)?;
                (
                    domain,
                    ExprKind::Select {
                        condition: condition.id,
                        yes: yes.id,
                        no: no.id,
                    },
                    dependent,
                    cost,
                )
            }
            a::Expr::TableGet(get) => {
                let column = self.column_id(&get.table, &get.column, span)?;
                let key = self.key(&get.key, column.table, scope, context)?;
                let domain = Self::promote(
                    Domain::scalar(self.program.column(column).ty.clone()),
                    expected.as_ref(),
                );
                (
                    domain,
                    ExprKind::TableGet {
                        column,
                        key: key.id,
                        state: context.state,
                    },
                    true,
                    key.cost,
                )
            }
            a::Expr::TableAccess(get) => {
                let column = self.column_id(&get.table, &get.column, span)?;
                let expression = get.key.as_ref().ok_or_else(|| {
                    diagnostic(
                        "D_TYPE_MISMATCH",
                        "whole-column access requires a projection slice or sum",
                        span,
                    )
                })?;
                let key = self.key(expression, column.table, scope, context)?;
                let domain = Self::promote(
                    Domain::scalar(self.program.column(column).ty.clone()),
                    expected.as_ref(),
                );
                (
                    domain,
                    ExprKind::SlotRead {
                        column,
                        key: key.id,
                        state: context.state,
                    },
                    true,
                    key.cost,
                )
            }
            a::Expr::Old(old) => {
                let column = self.column_id(&old.access.table, &old.access.column, span)?;
                let expression = old.access.key.as_ref().ok_or_else(|| {
                    diagnostic("D_TYPE_MISMATCH", "old requires a scalar row key", span)
                })?;
                let old_context = Context {
                    state: Snapshot::Pre,
                    outbox: false,
                    purity: if context.purity == Purity::Body {
                        Purity::OldKey
                    } else {
                        context.purity
                    },
                    commit: false,
                    ..context
                };
                let key = self.key(expression, column.table, scope, old_context)?;
                let domain = Self::promote(
                    Domain::scalar(self.program.column(column).ty.clone()),
                    expected.as_ref(),
                );
                (
                    domain,
                    ExprKind::SlotRead {
                        column,
                        key: key.id,
                        state: Snapshot::Pre,
                    },
                    true,
                    key.cost,
                )
            }
            a::Expr::TableExists(exists) => {
                let table = self.table_id(&exists.table, span)?;
                let key = self.key(&exists.key, table, scope, context)?;
                (
                    Domain::scalar(ScalarType::Bool),
                    ExprKind::TableExists {
                        table,
                        key: key.id,
                        state: context.state,
                    },
                    true,
                    key.cost,
                )
            }
            a::Expr::TableLookup(lookup) => {
                return self.lookup(
                    &lookup.table,
                    &lookup.key_column,
                    &lookup.key,
                    false,
                    scope,
                    expected,
                    context,
                    span,
                )
            }
            a::Expr::TableExistsUnique(lookup) => {
                return self.lookup(
                    &lookup.table,
                    &lookup.key_column,
                    &lookup.key,
                    true,
                    scope,
                    expected,
                    context,
                    span,
                )
            }
            a::Expr::SumAgg(sum) => {
                let column = self.column_id(&sum.table, &sum.column, span)?;
                if !self.program.column(column).tracked_sum {
                    return Err(diagnostic(
                        "D_BAD_OPERAND",
                        "sum requires a tracked_sum column",
                        span,
                    ));
                }
                (
                    Domain::aggregate(self.program.column(column).ty.clone()),
                    ExprKind::Sum {
                        column,
                        state: if sum.old {
                            Snapshot::Pre
                        } else {
                            context.state
                        },
                    },
                    true,
                    0,
                )
            }
            a::Expr::KeyOf(key) => {
                let table = self.table_id(&key.table, span)?;
                let unsigned_literal = matches!(&*key.expr, a::Expr::Literal(literal) if matches!(&literal.value, LiteralValue::Integer(number) if !number.as_decimal().starts_with('-')));
                let hint = unsigned_literal.then(|| Domain::scalar(ScalarType::U64));
                let value = self.expression(&key.expr, scope, hint, context)?;
                if !self.program.expr(value.id).domain.scalar.is_integer() {
                    return Err(diagnostic(
                        "D_TYPE_MISMATCH",
                        "id64 constructor requires an integer",
                        span,
                    ));
                }
                (
                    Domain::scalar(ScalarType::Id64(table)),
                    ExprKind::KeyOf {
                        table,
                        value: value.id,
                    },
                    self.program.expr(value.id).state_dependent,
                    value.cost,
                )
            }
            a::Expr::Call(call) => {
                let function = self
                    .function_names
                    .get(&call.function)
                    .copied()
                    .ok_or_else(|| {
                        diagnostic(
                            "D_UNKNOWN_FUNCTION",
                            format!("unknown function {:?}", call.function),
                            span,
                        )
                    })?;
                let info = self.program.function(function).clone();
                if call.args.len() != info.params.len() {
                    return Err(diagnostic(
                        "D_ARITY",
                        "function argument count differs from its declaration",
                        span,
                    ));
                }
                let mut args = Vec::new();
                let mut cost = add_cost(1, info.wcet_bound, span)?;
                let mut dependent = false;
                for (argument, parameter) in call.args.iter().zip(&info.params) {
                    let value = self.expression(
                        argument,
                        scope,
                        Some(Domain::scalar(parameter.ty.clone())),
                        context,
                    )?;
                    cost = add_cost(cost, value.cost, span)?;
                    dependent |= self.program.expr(value.id).state_dependent;
                    args.push(value.id);
                }
                let product = context
                    .fold_product
                    .checked_mul(info.max_fold_product)
                    .ok_or_else(|| {
                        diagnostic(
                            "D_FOLD_BOUND",
                            "transitive function fold product exceeds u32 representation",
                            span,
                        )
                    })?;
                self.stats.product = self.stats.product.max(product);
                self.stats.depth = self
                    .stats
                    .depth
                    .max(context.fold_depth.saturating_add(info.fold_depth));
                (
                    Self::promote(Domain::scalar(info.result_type), expected.as_ref()),
                    ExprKind::Call { function, args },
                    dependent,
                    cost,
                )
            }
            a::Expr::EffectCount(_) => {
                if !context.outbox {
                    return Err(diagnostic(
                        "D_EFFECT_CONTEXT",
                        "effect observations are only valid in transition ensures",
                        span,
                    ));
                }
                (
                    Domain::scalar(ScalarType::U64),
                    ExprKind::EffectCount,
                    true,
                    0,
                )
            }
            a::Expr::EffectIs(query) => {
                let (effect, index) =
                    self.effect_index(&query.effect, &query.index, scope, context, span)?;
                (
                    Domain::scalar(ScalarType::Bool),
                    ExprKind::EffectIs {
                        effect,
                        index: index.id,
                    },
                    true,
                    index.cost,
                )
            }
            a::Expr::EffectArg(query) => {
                let (effect, index) =
                    self.effect_index(&query.effect, &query.index, scope, context, span)?;
                let info = self.program.effect(effect);
                let parameter = info
                    .params
                    .iter()
                    .position(|p| p.name == query.parameter)
                    .ok_or_else(|| {
                        diagnostic(
                            "D_UNKNOWN_PARAM",
                            format!("unknown effect parameter {:?}", query.parameter),
                            span,
                        )
                    })?;
                let domain = Self::promote(
                    Domain::scalar(info.params[parameter].ty.clone()),
                    expected.as_ref(),
                );
                (
                    domain,
                    ExprKind::EffectArg {
                        effect,
                        index: index.id,
                        parameter,
                    },
                    true,
                    index.cost,
                )
            }
            a::Expr::Fold(fold) => return self.fold(fold, scope, expected, context),
        };
        Ok(Lowered {
            id: self.push(domain, kind, dependent, span),
            cost,
        })
    }

    fn effect_index(
        &mut self,
        name: &str,
        index: &a::Expr,
        scope: &Scope,
        context: Context,
        span: Span,
    ) -> Result<(EffectId, Lowered)> {
        if !context.outbox {
            return Err(diagnostic(
                "D_EFFECT_CONTEXT",
                "effect observations are only valid in transition ensures",
                span,
            ));
        }
        let effect = self.effect_names.get(name).copied().ok_or_else(|| {
            diagnostic("D_UNKNOWN_EFFECT", format!("unknown effect {name:?}"), span)
        })?;
        let index =
            self.expression(index, scope, Some(Domain::scalar(ScalarType::U64)), context)?;
        Ok((effect, index))
    }

    fn promote(mut domain: Domain, expected: Option<&Domain>) -> Domain {
        if let Some(expected) = expected {
            if expected.scalar == domain.scalar {
                domain.width = domain.width.max(expected.width);
            }
        }
        domain
    }

    fn literal_domain(
        &self,
        literal: &LiteralValue,
        expected: Option<Domain>,
        span: Span,
    ) -> Result<Domain> {
        match literal {
            LiteralValue::Bool(_) => Ok(Domain::scalar(ScalarType::Bool)),
            LiteralValue::Integer(_) => {
                let domain = expected.unwrap_or_else(|| Domain::scalar(ScalarType::I64));
                if domain.scalar.is_bool() {
                    return Err(diagnostic(
                        "D_TYPE_MISMATCH",
                        "integer literal cannot be used as bool",
                        span,
                    ));
                }
                Ok(domain)
            }
            LiteralValue::Decimal(_) => {
                let domain = expected.unwrap_or_else(|| Domain::scalar(ScalarType::Dec64));
                if !domain.scalar.is_decimal() {
                    return Err(diagnostic(
                        "D_TYPE_MISMATCH",
                        "fractional literals require a decimal type",
                        span,
                    ));
                }
                Ok(domain)
            }
            LiteralValue::String(value) => match expected {
                Some(
                    domain @ Domain {
                        scalar: ScalarType::Sym32,
                        ..
                    },
                ) => Ok(domain),
                Some(
                    domain @ Domain {
                        scalar: ScalarType::Enum(_),
                        ..
                    },
                ) => {
                    if let ScalarType::Enum(tags) = &domain.scalar {
                        if !tags.contains(value) {
                            return Err(diagnostic(
                                "D_ENUM_TAG",
                                format!("unknown enum tag {value:?}"),
                                span,
                            ));
                        }
                    }
                    Ok(domain)
                }
                _ => Err(diagnostic(
                    "D_TYPE_MISMATCH",
                    "string literal requires sym32 or enum context",
                    span,
                )),
            },
        }
    }

    fn concrete(&self, expression: &a::Expr, scope: &Scope) -> Result<Option<Domain>> {
        Ok(match expression {
            a::Expr::Literal(literal) => matches!(literal.value, LiteralValue::Bool(_))
                .then(|| Domain::scalar(ScalarType::Bool)),
            a::Expr::Var(variable) => scope
                .get(&variable.name)
                .map(|id| Domain::scalar(self.program.binding(*id).ty.clone())),
            a::Expr::TableGet(get) => Some(Domain::scalar(
                self.program
                    .column(self.column_id(&get.table, &get.column, get.span)?)
                    .ty
                    .clone(),
            )),
            a::Expr::TableAccess(get) => Some(Domain::scalar(
                self.program
                    .column(self.column_id(&get.table, &get.column, get.span)?)
                    .ty
                    .clone(),
            )),
            a::Expr::Old(old) => Some(Domain::scalar(
                self.program
                    .column(self.column_id(&old.access.table, &old.access.column, old.span)?)
                    .ty
                    .clone(),
            )),
            a::Expr::SumAgg(sum) => Some(Domain::aggregate(
                self.program
                    .column(self.column_id(&sum.table, &sum.column, sum.span)?)
                    .ty
                    .clone(),
            )),
            a::Expr::KeyOf(key) => Some(Domain::scalar(ScalarType::Id64(
                self.table_id(&key.table, key.span)?,
            ))),
            a::Expr::TableLookup(lookup) => Some(Domain::scalar(ScalarType::Id64(
                self.table_id(&lookup.table, lookup.span)?,
            ))),
            a::Expr::TableExists(_) | a::Expr::TableExistsUnique(_) => {
                Some(Domain::scalar(ScalarType::Bool))
            }
            a::Expr::Call(call) => {
                let function = self
                    .function_names
                    .get(&call.function)
                    .copied()
                    .ok_or_else(|| {
                        diagnostic(
                            "D_UNKNOWN_FUNCTION",
                            format!("unknown function {:?}", call.function),
                            call.span,
                        )
                    })?;
                Some(Domain::scalar(
                    self.program.function(function).result_type.clone(),
                ))
            }
            a::Expr::EffectCount(_) => Some(Domain::scalar(ScalarType::U64)),
            a::Expr::EffectIs(_) => Some(Domain::scalar(ScalarType::Bool)),
            a::Expr::EffectArg(query) => {
                let effect = self
                    .effect_names
                    .get(&query.effect)
                    .copied()
                    .ok_or_else(|| {
                        diagnostic(
                            "D_UNKNOWN_EFFECT",
                            format!("unknown effect {:?}", query.effect),
                            query.span,
                        )
                    })?;
                let parameter = self
                    .program
                    .effect(effect)
                    .params
                    .iter()
                    .find(|p| p.name == query.parameter)
                    .ok_or_else(|| {
                        diagnostic(
                            "D_UNKNOWN_PARAM",
                            format!("unknown effect parameter {:?}", query.parameter),
                            query.span,
                        )
                    })?;
                Some(Domain::scalar(parameter.ty.clone()))
            }
            a::Expr::Fold(fold) => Some(Domain::scalar(self.resolve_type(&fold.acc_type)?)),
            a::Expr::Unary(unary) => {
                if unary.op == "not" {
                    Some(Domain::scalar(ScalarType::Bool))
                } else {
                    self.concrete(&unary.operand, scope)?
                }
            }
            a::Expr::Select(select) => self.merge_concrete(
                self.concrete(&select.a, scope)?,
                self.concrete(&select.b, scope)?,
            ),
            a::Expr::Binary(binary) => {
                if matches!(
                    binary.op.as_str(),
                    "eq" | "ne" | "lt" | "le" | "gt" | "ge" | "and" | "or" | "xor"
                ) {
                    Some(Domain::scalar(ScalarType::Bool))
                } else {
                    self.merge_concrete(
                        self.concrete(&binary.left, scope)?,
                        self.concrete(&binary.right, scope)?,
                    )
                }
            }
        })
    }

    fn merge_concrete(&self, left: Option<Domain>, right: Option<Domain>) -> Option<Domain> {
        match (left, right) {
            (Some(mut left), Some(right)) if left.scalar == right.scalar => {
                left.width = left.width.max(right.width);
                Some(left)
            }
            (left @ Some(_), _) => left,
            (None, right) => right,
        }
    }

    fn binary(
        &mut self,
        binary: &a::Binary,
        scope: &Scope,
        expected: Option<Domain>,
        context: Context,
    ) -> Result<Lowered> {
        let op = binary_op(&binary.op)
            .ok_or_else(|| diagnostic("D_BAD_OPERAND", "unknown binary operation", binary.span))?;
        let left_concrete = self.concrete(&binary.left, scope)?;
        let right_concrete = self.concrete(&binary.right, scope)?;
        let mut target = if op.boolean() {
            Some(Domain::scalar(ScalarType::Bool))
        } else if !op.comparison() {
            expected.or_else(|| self.merge_concrete(left_concrete.clone(), right_concrete.clone()))
        } else {
            self.merge_concrete(left_concrete.clone(), right_concrete.clone())
        };
        if let Some(domain) = &mut target {
            for other in [left_concrete, right_concrete].into_iter().flatten() {
                if domain.scalar == other.scalar {
                    domain.width = domain.width.max(other.width);
                }
            }
        }
        let left = self.expression(&binary.left, scope, target.clone(), context)?;
        let left_domain = self.program.expr(left.id).domain.clone();
        let right = self.expression(
            &binary.right,
            scope,
            Some(target.unwrap_or_else(|| left_domain.clone())),
            context,
        )?;
        let right_domain = self.program.expr(right.id).domain.clone();
        if left_domain.scalar != right_domain.scalar {
            return Err(diagnostic(
                "D_TYPE_MISMATCH",
                "binary operands require the same scalar type",
                binary.span,
            ));
        }
        if op.boolean() && left_domain.scalar != ScalarType::Bool {
            return Err(diagnostic(
                "D_TYPE_MISMATCH",
                "boolean operation requires bool operands",
                binary.span,
            ));
        }
        if matches!(
            op,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        ) && !left_domain.scalar.is_ordered()
        {
            return Err(diagnostic(
                "D_BAD_OPERAND",
                "ordered comparison requires numeric or enum operands",
                binary.span,
            ));
        }
        if !op.comparison() && !op.boolean() && !left_domain.scalar.is_numeric() {
            return Err(diagnostic(
                "D_BAD_OPERAND",
                "arithmetic requires numeric operands",
                binary.span,
            ));
        }
        let domain = if op.comparison() || op.boolean() {
            Domain::scalar(ScalarType::Bool)
        } else {
            Domain {
                scalar: left_domain.scalar,
                width: left_domain.width.max(right_domain.width),
            }
        };
        let dependent = self.program.expr(left.id).state_dependent
            || self.program.expr(right.id).state_dependent;
        let cost = add_cost(left.cost, right.cost, binary.span)?;
        Ok(Lowered {
            id: self.push(
                domain,
                ExprKind::Binary {
                    op,
                    left: left.id,
                    right: right.id,
                },
                dependent,
                binary.span,
            ),
            cost,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn lookup(
        &mut self,
        table: &str,
        name: &str,
        expression: &a::Expr,
        exists_only: bool,
        scope: &Scope,
        expected: Option<Domain>,
        context: Context,
        span: Span,
    ) -> Result<Lowered> {
        let column = self.column_id(table, name, span)?;
        let info = self.program.column(column);
        if !info.unique {
            return Err(diagnostic(
                "D_BAD_OPERAND",
                "secondary lookup requires a unique column",
                span,
            ));
        }
        let value = self.expression(
            expression,
            scope,
            Some(Domain::scalar(info.ty.clone())),
            context,
        )?;
        let scalar = if exists_only {
            ScalarType::Bool
        } else {
            ScalarType::Id64(column.table)
        };
        let domain = Self::promote(Domain::scalar(scalar), expected.as_ref());
        Ok(Lowered {
            id: self.push(
                domain,
                ExprKind::UniqueLookup {
                    column,
                    value: value.id,
                    state: context.state,
                    exists_only,
                },
                true,
                span,
            ),
            cost: value.cost,
        })
    }

    fn fold(
        &mut self,
        fold: &a::Fold,
        scope: &Scope,
        expected: Option<Domain>,
        context: Context,
    ) -> Result<Lowered> {
        let limit = fold
            .n
            .as_decimal()
            .parse::<u32>()
            .ok()
            .filter(|bound| *bound > 0)
            .ok_or_else(|| {
                diagnostic(
                    "D_FOLD_BOUND",
                    "fold bound must be a positive u32",
                    fold.span,
                )
            })?;
        let depth = context.fold_depth + 1;
        let product = context.fold_product.checked_mul(limit).ok_or_else(|| {
            diagnostic(
                "D_FOLD_BOUND",
                "nested fold product exceeds u32 representation",
                fold.span,
            )
        })?;
        self.stats.depth = self.stats.depth.max(depth);
        self.stats.product = self.stats.product.max(product);
        let inner_context = Context {
            fold_depth: depth,
            fold_product: product,
            commit: false,
            ..context
        };
        let accumulator_type = self.resolve_type(&fold.acc_type)?;
        let init = self.expression(
            &fold.init,
            scope,
            Some(Domain::scalar(accumulator_type.clone())),
            inner_context,
        )?;
        let range_type = self
            .concrete(&fold.lo, scope)?
            .or(self.concrete(&fold.hi, scope)?);
        let lo = self.expression(&fold.lo, scope, range_type, inner_context)?;
        let index_type = self.program.expr(lo.id).domain.scalar.clone();
        if !index_type.is_integer() {
            return Err(diagnostic(
                "D_BAD_OPERAND",
                "fold range bounds must be integers",
                fold.span,
            ));
        }
        let hi = self.expression(
            &fold.hi,
            scope,
            Some(Domain::scalar(index_type.clone())),
            inner_context,
        )?;
        if fold.acc_name == fold.index_name
            || scope.contains_key(&fold.acc_name)
            || scope.contains_key(&fold.index_name)
        {
            return Err(diagnostic(
                "D_DUPLICATE",
                "fold bindings must be distinct fresh names",
                fold.span,
            ));
        }
        let mut inner_scope = scope.clone();
        let dependent = self.program.expr(init.id).state_dependent
            || self.program.expr(lo.id).state_dependent
            || self.program.expr(hi.id).state_dependent
            || self.source_state_dependent(&fold.yield_expr, scope)
            || fold
                .body
                .iter()
                .any(|statement| self.statement_state_dependent(statement, scope));
        let accumulator = self.bind(
            &mut inner_scope,
            &fold.acc_name,
            accumulator_type.clone(),
            BindingOrigin::FoldAccumulator,
            dependent,
            fold.span,
        )?;
        let index = self.bind(
            &mut inner_scope,
            &fold.index_name,
            index_type,
            BindingOrigin::FoldIndex,
            self.program.expr(lo.id).state_dependent || self.program.expr(hi.id).state_dependent,
            fold.span,
        )?;
        let mut body = Vec::new();
        let mut body_cost = 1u64;
        for statement in &fold.body {
            if matches!(statement, a::Stmt::Commit(_) | a::Stmt::Yield(_)) {
                return Err(diagnostic(
                    "D_BAD_OPERAND",
                    "fold body cannot commit or yield before its final result",
                    fold.span,
                ));
            }
            let (lowered, cost) = self.statement(statement, &mut inner_scope, inner_context)?;
            body_cost = add_cost(body_cost, cost, fold.span)?;
            body.push(lowered);
        }
        let yield_value = self.expression(
            &fold.yield_expr,
            &inner_scope,
            Some(Domain::scalar(accumulator_type.clone())),
            inner_context,
        )?;
        body_cost = add_cost(body_cost, yield_value.cost, fold.span)?;
        let setup = add_cost(add_cost(init.cost, lo.cost, fold.span)?, hi.cost, fold.span)?;
        let repeated = body_cost
            .checked_mul(u64::from(limit))
            .ok_or_else(|| diagnostic("D_WCET_OVERFLOW", "fold cost exceeds u64", fold.span))?;
        let cost = add_cost(add_cost(1, setup, fold.span)?, repeated, fold.span)?;
        let state_dependent = dependent
            || self.program.expr(lo.id).state_dependent
            || self.program.expr(hi.id).state_dependent
            || self.program.expr(yield_value.id).state_dependent;
        let kind = ExprKind::Fold(Box::new(i::Fold {
            limit,
            accumulator,
            index,
            init: init.id,
            lo: lo.id,
            hi: hi.id,
            body,
            yield_value: yield_value.id,
        }));
        let domain = Self::promote(Domain::scalar(accumulator_type), expected.as_ref());
        Ok(Lowered {
            id: self.push(domain, kind, state_dependent, fold.span),
            cost,
        })
    }

    fn statement(
        &mut self,
        statement: &a::Stmt,
        scope: &mut Scope,
        context: Context,
    ) -> Result<(Stmt, u64)> {
        let span = statement_span(statement);
        if context.purity != Purity::Body
            && !matches!(statement, a::Stmt::Let(_))
            && !(context.purity == Purity::OldKey && matches!(statement, a::Stmt::Guard(_)))
        {
            return Err(diagnostic(
                "D_BAD_OPERAND",
                "specification/projection folds may only bind pure values",
                span,
            ));
        }
        let (kind, cost) = match statement {
            a::Stmt::Let(binding) => {
                if scope.contains_key(&binding.name) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        "SSA binding is already defined",
                        span,
                    ));
                }
                let ty = self.resolve_type(&binding.ty)?;
                let value = self.expression(
                    &binding.value,
                    scope,
                    Some(Domain::scalar(ty.clone())),
                    context,
                )?;
                let binding_id = self.bind(
                    scope,
                    &binding.name,
                    ty,
                    BindingOrigin::Local,
                    self.program.expr(value.id).state_dependent,
                    span,
                )?;
                let cost = add_cost(
                    u64::from(!matches!(binding.value, a::Expr::Fold(_))),
                    value.cost,
                    span,
                )?;
                (
                    StmtKind::Let {
                        binding: binding_id,
                        value: value.id,
                    },
                    cost,
                )
            }
            a::Stmt::Guard(guard) => {
                let condition = self.expression(
                    &guard.cond,
                    scope,
                    Some(Domain::scalar(ScalarType::Bool)),
                    context,
                )?;
                (
                    StmtKind::Guard {
                        condition: condition.id,
                        code: guard.code.clone(),
                    },
                    add_cost(1, condition.cost, span)?,
                )
            }
            a::Stmt::TableSet(set) => {
                let column = self.column_id(&set.table, &set.column, span)?;
                if self.program.column(column).unique {
                    return Err(diagnostic(
                        "D_BAD_OPERAND",
                        "unique columns are immutable after allocation",
                        span,
                    ));
                }
                let key = self.key(&set.key, column.table, scope, context)?;
                let value = self.expression(
                    &set.value,
                    scope,
                    Some(Domain::scalar(self.program.column(column).ty.clone())),
                    context,
                )?;
                let cost = add_cost(1, add_cost(key.cost, value.cost, span)?, span)?;
                (
                    StmtKind::TableSet {
                        column,
                        key: key.id,
                        value: value.id,
                    },
                    cost,
                )
            }
            a::Stmt::TableAlloc(allocation) => {
                if scope.contains_key(&allocation.name) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        "allocation binding is already defined",
                        span,
                    ));
                }
                let table = self.table_id(&allocation.table, span)?;
                let mut fields = Vec::new();
                let mut seen = BTreeSet::new();
                let mut cost = 1;
                for (name, expression) in &allocation.fields {
                    let column = self.column_id(&allocation.table, name, span)?;
                    if !seen.insert(column) {
                        return Err(diagnostic(
                            "D_DUPLICATE",
                            "allocation initializes a column twice",
                            span,
                        ));
                    }
                    let value = self.expression(
                        expression,
                        scope,
                        Some(Domain::scalar(self.program.column(column).ty.clone())),
                        context,
                    )?;
                    cost = add_cost(cost, value.cost, span)?;
                    fields.push((column, value.id));
                }
                if fields.len() != self.program.table(table).columns.len() {
                    return Err(diagnostic(
                        "D_UNINITIALIZED",
                        "allocation must initialize every column exactly once",
                        span,
                    ));
                }
                let binding = self.bind(
                    scope,
                    &allocation.name,
                    ScalarType::Id64(table),
                    BindingOrigin::Allocation,
                    true,
                    span,
                )?;
                (
                    StmtKind::Allocate {
                        table,
                        binding,
                        fields,
                    },
                    cost,
                )
            }
            a::Stmt::Emit(emit) => {
                let effect = self
                    .effect_names
                    .get(&emit.effect)
                    .copied()
                    .ok_or_else(|| {
                        diagnostic(
                            "D_UNKNOWN_EFFECT",
                            format!("unknown effect {:?}", emit.effect),
                            span,
                        )
                    })?;
                let parameters = self.program.effect(effect).params.clone();
                if parameters.len() != emit.args.len() {
                    return Err(diagnostic(
                        "D_TYPE_MISMATCH",
                        "effect argument count differs from its declaration",
                        span,
                    ));
                }
                let mut args = Vec::new();
                let mut cost = 1;
                for (expression, parameter) in emit.args.iter().zip(parameters) {
                    let value = self.expression(
                        expression,
                        scope,
                        Some(Domain::scalar(parameter.ty)),
                        context,
                    )?;
                    cost = add_cost(cost, value.cost, span)?;
                    args.push(value.id);
                }
                (StmtKind::Emit { effect, args }, cost)
            }
            a::Stmt::Yield(_) => {
                return Err(diagnostic(
                    "D_BAD_OPERAND",
                    "yield is only valid as the final fold result",
                    span,
                ))
            }
            a::Stmt::Commit(_) => {
                if !context.commit {
                    return Err(diagnostic(
                        "D_BAD_OPERAND",
                        "nested blocks cannot commit",
                        span,
                    ));
                }
                (StmtKind::Commit, 0)
            }
        };
        Ok((Stmt { kind, span }, cost))
    }

    fn source_state_dependent(&self, expression: &a::Expr, scope: &Scope) -> bool {
        match expression {
            a::Expr::Var(variable) => scope
                .get(&variable.name)
                .is_some_and(|id| self.program.binding(*id).state_dependent),
            a::Expr::Literal(_) => false,
            a::Expr::Call(call) => call
                .args
                .iter()
                .any(|arg| self.source_state_dependent(arg, scope)),
            a::Expr::Unary(unary) => self.source_state_dependent(&unary.operand, scope),
            a::Expr::Binary(binary) => {
                self.source_state_dependent(&binary.left, scope)
                    || self.source_state_dependent(&binary.right, scope)
            }
            a::Expr::Select(select) => {
                self.source_state_dependent(&select.cond, scope)
                    || self.source_state_dependent(&select.a, scope)
                    || self.source_state_dependent(&select.b, scope)
            }
            a::Expr::KeyOf(key) => self.source_state_dependent(&key.expr, scope),
            a::Expr::Fold(fold) => {
                [&fold.init, &fold.lo, &fold.hi, &fold.yield_expr]
                    .iter()
                    .any(|child| self.source_state_dependent(child, scope))
                    || fold
                        .body
                        .iter()
                        .any(|statement| self.statement_state_dependent(statement, scope))
            }
            _ => true,
        }
    }

    fn statement_state_dependent(&self, statement: &a::Stmt, scope: &Scope) -> bool {
        match statement {
            a::Stmt::Let(binding) => self.source_state_dependent(&binding.value, scope),
            a::Stmt::Guard(guard) => self.source_state_dependent(&guard.cond, scope),
            a::Stmt::TableSet(_) | a::Stmt::TableAlloc(_) => true,
            a::Stmt::Emit(emit) => emit
                .args
                .iter()
                .any(|expression| self.source_state_dependent(expression, scope)),
            a::Stmt::Yield(value) => self.source_state_dependent(&value.value, scope),
            a::Stmt::Commit(_) => false,
        }
    }

    fn access(&self, column: ColumnId, key: Option<ExprId>, span: Span) -> Access {
        let tier = if key.is_some_and(|id| self.program.expr(id).state_dependent) {
            2
        } else {
            1
        };
        Access {
            column,
            key,
            tier,
            span,
        }
    }

    fn collect_reads(&self, id: ExprId, reads: &mut Vec<Access>, tier_two: &mut bool) {
        let expression = self.program.expr(id);
        match &expression.kind {
            ExprKind::Constant(_) | ExprKind::Binding(_) | ExprKind::EffectCount => {}
            ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => {
                self.collect_reads(*index, reads, tier_two)
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.collect_reads(*arg, reads, tier_two);
                }
            }
            ExprKind::Unary { operand, .. } => self.collect_reads(*operand, reads, tier_two),
            ExprKind::Binary { left, right, .. } => {
                self.collect_reads(*left, reads, tier_two);
                self.collect_reads(*right, reads, tier_two);
            }
            ExprKind::Select { condition, yes, no } => {
                for child in [condition, yes, no] {
                    self.collect_reads(*child, reads, tier_two);
                }
            }
            ExprKind::TableGet { column, key, .. } | ExprKind::SlotRead { column, key, .. } => {
                reads.push(self.access(*column, Some(*key), expression.span));
                self.collect_reads(*key, reads, tier_two);
            }
            ExprKind::TableExists { key, .. } => {
                *tier_two |= self.program.expr(*key).state_dependent;
                self.collect_reads(*key, reads, tier_two);
            }
            ExprKind::UniqueLookup { column, value, .. } => {
                reads.push(self.access(*column, Some(*value), expression.span));
                self.collect_reads(*value, reads, tier_two);
            }
            ExprKind::Sum { column, .. } => reads.push(self.access(*column, None, expression.span)),
            ExprKind::KeyOf { value, .. } => self.collect_reads(*value, reads, tier_two),
            ExprKind::Fold(fold) => {
                for child in [fold.init, fold.lo, fold.hi, fold.yield_value] {
                    self.collect_reads(child, reads, tier_two);
                }
                let mut ignored_writes = Vec::new();
                self.collect_statements(&fold.body, reads, &mut ignored_writes, tier_two);
            }
        }
    }

    fn collect_expression_writes(&self, id: ExprId, writes: &mut Vec<Access>, tier_two: &mut bool) {
        match &self.program.expr(id).kind {
            ExprKind::Constant(_)
            | ExprKind::Binding(_)
            | ExprKind::Sum { .. }
            | ExprKind::EffectCount => {}
            ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => {
                self.collect_expression_writes(*index, writes, tier_two)
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.collect_expression_writes(*arg, writes, tier_two);
                }
            }
            ExprKind::Unary { operand, .. } => {
                self.collect_expression_writes(*operand, writes, tier_two)
            }
            ExprKind::Binary { left, right, .. } => {
                self.collect_expression_writes(*left, writes, tier_two);
                self.collect_expression_writes(*right, writes, tier_two);
            }
            ExprKind::Select { condition, yes, no } => {
                for child in [condition, yes, no] {
                    self.collect_expression_writes(*child, writes, tier_two);
                }
            }
            ExprKind::TableGet { key, .. }
            | ExprKind::SlotRead { key, .. }
            | ExprKind::TableExists { key, .. } => {
                self.collect_expression_writes(*key, writes, tier_two)
            }
            ExprKind::UniqueLookup { value, .. } | ExprKind::KeyOf { value, .. } => {
                self.collect_expression_writes(*value, writes, tier_two)
            }
            ExprKind::Fold(fold) => {
                for child in [fold.init, fold.lo, fold.hi, fold.yield_value] {
                    self.collect_expression_writes(child, writes, tier_two);
                }
                self.collect_statements(&fold.body, &mut Vec::new(), writes, tier_two);
            }
        }
    }

    fn collect_statements(
        &self,
        statements: &[Stmt],
        reads: &mut Vec<Access>,
        writes: &mut Vec<Access>,
        tier_two: &mut bool,
    ) {
        for statement in statements {
            let expressions = match &statement.kind {
                StmtKind::Let { value, .. } | StmtKind::Eval(value) => vec![*value],
                StmtKind::Guard { condition, .. } => vec![*condition],
                StmtKind::TableSet { column, key, value } => {
                    writes.push(self.access(*column, Some(*key), statement.span));
                    vec![*key, *value]
                }
                StmtKind::Allocate { table, fields, .. } => {
                    *tier_two = true;
                    for column in &self.program.table(*table).columns {
                        writes.push(Access {
                            column: column.id,
                            key: None,
                            tier: 2,
                            span: statement.span,
                        });
                    }
                    fields.iter().map(|(_, expression)| *expression).collect()
                }
                StmtKind::Emit { args, .. } => args.clone(),
                StmtKind::Commit => Vec::new(),
            };
            for expression in expressions {
                self.collect_reads(expression, reads, tier_two);
                self.collect_expression_writes(expression, writes, tier_two);
            }
        }
    }

    fn covered(&self, declarations: &[Access], actual: &Access) -> bool {
        declarations.iter().any(|declared| {
            declared.column == actual.column
                && match (declared.key, actual.key) {
                    (None, _) => true,
                    (Some(left), Some(right)) => {
                        self.same_expression(left, right, &BTreeMap::new())
                    }
                    _ => false,
                }
        })
    }

    fn same_expression(
        &self,
        left: ExprId,
        right: ExprId,
        bindings: &BTreeMap<BindingId, BindingId>,
    ) -> bool {
        let (left, right) = (self.program.expr(left), self.program.expr(right));
        if left.domain != right.domain {
            return false;
        }
        let eq = |left, right| self.same_expression(left, right, bindings);
        match (&left.kind, &right.kind) {
            (
                ExprKind::Call {
                    function: a,
                    args: x,
                },
                ExprKind::Call {
                    function: b,
                    args: y,
                },
            ) => a == b && x.len() == y.len() && x.iter().zip(y).all(|(x, y)| eq(*x, *y)),
            (ExprKind::Constant(a), ExprKind::Constant(b)) => a == b,
            (ExprKind::Binding(a), ExprKind::Binding(b)) => bindings.get(a).unwrap_or(a) == b,
            (ExprKind::Unary { op: a, operand: x }, ExprKind::Unary { op: b, operand: y }) => {
                a == b && eq(*x, *y)
            }
            (
                ExprKind::Binary {
                    op: a,
                    left: x,
                    right: y,
                },
                ExprKind::Binary {
                    op: b,
                    left: u,
                    right: v,
                },
            ) => a == b && eq(*x, *u) && eq(*y, *v),
            (
                ExprKind::Select {
                    condition: a,
                    yes: b,
                    no: c,
                },
                ExprKind::Select {
                    condition: x,
                    yes: y,
                    no: z,
                },
            ) => eq(*a, *x) && eq(*b, *y) && eq(*c, *z),
            (
                ExprKind::TableGet {
                    column: a,
                    key: x,
                    state: left_state,
                },
                ExprKind::TableGet {
                    column: b,
                    key: y,
                    state: right_state,
                },
            )
            | (
                ExprKind::SlotRead {
                    column: a,
                    key: x,
                    state: left_state,
                },
                ExprKind::SlotRead {
                    column: b,
                    key: y,
                    state: right_state,
                },
            ) => a == b && left_state == right_state && eq(*x, *y),
            (
                ExprKind::TableExists {
                    table: a,
                    key: x,
                    state: left_state,
                },
                ExprKind::TableExists {
                    table: b,
                    key: y,
                    state: right_state,
                },
            ) => a == b && left_state == right_state && eq(*x, *y),
            (
                ExprKind::UniqueLookup {
                    column: a,
                    value: x,
                    exists_only: e,
                    state: left_state,
                },
                ExprKind::UniqueLookup {
                    column: b,
                    value: y,
                    exists_only: f,
                    state: right_state,
                },
            ) => a == b && e == f && left_state == right_state && eq(*x, *y),
            (
                ExprKind::Sum {
                    column: a,
                    state: left_state,
                },
                ExprKind::Sum {
                    column: b,
                    state: right_state,
                },
            ) => a == b && left_state == right_state,
            (ExprKind::KeyOf { table: a, value: x }, ExprKind::KeyOf { table: b, value: y }) => {
                a == b && eq(*x, *y)
            }
            (ExprKind::Fold(a), ExprKind::Fold(b)) => {
                if a.limit != b.limit || !eq(a.init, b.init) || !eq(a.lo, b.lo) || !eq(a.hi, b.hi) {
                    return false;
                }
                if self.program.binding(a.accumulator).ty != self.program.binding(b.accumulator).ty
                    || self.program.binding(a.index).ty != self.program.binding(b.index).ty
                {
                    return false;
                }
                let mut inner = bindings.clone();
                inner.insert(a.accumulator, b.accumulator);
                inner.insert(a.index, b.index);
                self.same_statements(&a.body, &b.body, &mut inner)
                    && self.same_expression(a.yield_value, b.yield_value, &inner)
            }
            _ => false,
        }
    }

    fn same_statements(
        &self,
        left: &[Stmt],
        right: &[Stmt],
        bindings: &mut BTreeMap<BindingId, BindingId>,
    ) -> bool {
        if left.len() != right.len() {
            return false;
        }
        for (left, right) in left.iter().zip(right) {
            let same = match (&left.kind, &right.kind) {
                (
                    StmtKind::Let {
                        binding: a,
                        value: x,
                    },
                    StmtKind::Let {
                        binding: b,
                        value: y,
                    },
                ) => {
                    if self.program.binding(*a).ty != self.program.binding(*b).ty
                        || !self.same_expression(*x, *y, bindings)
                    {
                        return false;
                    }
                    bindings.insert(*a, *b);
                    true
                }
                (
                    StmtKind::Guard {
                        condition: a,
                        code: x,
                    },
                    StmtKind::Guard {
                        condition: b,
                        code: y,
                    },
                ) => x == y && self.same_expression(*a, *b, bindings),
                (StmtKind::Eval(a), StmtKind::Eval(b)) => self.same_expression(*a, *b, bindings),
                (StmtKind::Commit, StmtKind::Commit) => true,
                _ => false,
            };
            if !same {
                return false;
            }
        }
        true
    }
}

fn binary_op(name: &str) -> Option<BinaryOp> {
    Some(match name {
        "add" => BinaryOp::Add,
        "sub" => BinaryOp::Sub,
        "mul" => BinaryOp::Mul,
        "div" => BinaryOp::Div,
        "rem" => BinaryOp::Rem,
        "min" => BinaryOp::Min,
        "max" => BinaryOp::Max,
        "eq" => BinaryOp::Eq,
        "ne" => BinaryOp::Ne,
        "lt" => BinaryOp::Lt,
        "le" => BinaryOp::Le,
        "gt" => BinaryOp::Gt,
        "ge" => BinaryOp::Ge,
        "and" => BinaryOp::And,
        "or" => BinaryOp::Or,
        "xor" => BinaryOp::Xor,
        _ => return None,
    })
}

fn statement_span(statement: &a::Stmt) -> Span {
    match statement {
        a::Stmt::Let(value) => value.span,
        a::Stmt::Guard(value) => value.span,
        a::Stmt::TableSet(value) => value.span,
        a::Stmt::TableAlloc(value) => value.span,
        a::Stmt::Emit(value) => value.span,
        a::Stmt::Yield(value) => value.span,
        a::Stmt::Commit(value) => value.span,
    }
}

/// Reject effects and state observations before lowering a function. Contracts
/// additionally reject calls, preventing hidden contract expansion/dependencies.
fn function_expression_dependencies(
    expression: &a::Expr,
    calls: bool,
    deps: &mut BTreeSet<String>,
) -> Result<()> {
    let mut child = |e: &a::Expr| function_expression_dependencies(e, calls, deps);
    match expression {
        a::Expr::Literal(_) | a::Expr::Var(_) => Ok(()),
        a::Expr::Unary(v) => child(&v.operand),
        a::Expr::Binary(v) => {
            child(&v.left)?;
            child(&v.right)
        }
        a::Expr::Select(v) => {
            child(&v.cond)?;
            child(&v.a)?;
            child(&v.b)
        }
        a::Expr::KeyOf(v) => child(&v.expr),
        a::Expr::Fold(v) => {
            child(&v.init)?;
            child(&v.lo)?;
            child(&v.hi)?;
            for statement in &v.body {
                match statement {
                    a::Stmt::Let(v) => child(&v.value)?,
                    _ => {
                        return Err(diagnostic(
                            "D_FUNCTION_PURITY",
                            "pure function folds may only bind values",
                            statement_span(statement),
                        ))
                    }
                }
            }
            child(&v.yield_expr)
        }
        a::Expr::Call(v) if calls => {
            for arg in &v.args {
                child(arg)?;
            }
            deps.insert(v.function.clone());
            Ok(())
        }
        a::Expr::Call(_) => Err(diagnostic(
            "D_FUNCTION_CONTRACT",
            "function contracts cannot call functions",
            expression.span(),
        )),
        _ => Err(diagnostic(
            "D_FUNCTION_PURITY",
            "functions and their contracts cannot observe or mutate state or effects",
            expression.span(),
        )),
    }
}
