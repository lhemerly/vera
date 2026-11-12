//! Exclusive mutation ownership and retained stateful call frames.
use super::*;

impl Analyzer {
    pub(super) fn collect_components(&mut self, declarations: &[a::Decl]) -> Result<()> {
        let mut names = BTreeSet::new();
        let mut owned = BTreeSet::new();
        for declaration in declarations {
            let a::Decl::Component(component) = declaration else {
                continue;
            };
            if !names.insert(component.name.clone()) {
                return Err(diagnostic(
                    "D_DUPLICATE",
                    "component declared twice",
                    component.span,
                ));
            }
            let id = ComponentId(self.program.components.len());
            let mut tables = Vec::new();
            for name in &component.owns {
                let table = self.table_id(name, component.span)?;
                if !owned.insert(table) {
                    return Err(diagnostic(
                        "D_COMPONENT_OWNERSHIP",
                        format!("table {name:?} has more than one owner"),
                        component.span,
                    ));
                }
                tables.push(table);
            }
            self.program.components.push(Component {
                id,
                name: component.name.clone(),
                tables,
                invariants: Vec::new(),
            });
        }
        Ok(())
    }

    pub(super) fn component(&mut self, component: &a::ComponentDecl) -> Result<()> {
        let id = self
            .program
            .components
            .iter()
            .find(|c| c.name == component.name)
            .expect("collected component")
            .id;
        self.active_component = Some(id);
        let mut names = BTreeSet::new();
        for invariant in &component.invariants {
            if !names.insert(invariant.name.clone()) {
                return Err(diagnostic(
                    "D_DUPLICATE",
                    "component invariant declared twice",
                    invariant.span,
                ));
            }
            let first = self.program.expressions.len();
            let index = self.program.invariants.len();
            self.invariant(invariant)?;
            self.check_component_expressions(first, id)?;
            if let Some((table, _)) = self.program.invariants[index].quantifier {
                self.require_owner(table, id, invariant.span)?;
            }
            self.program.invariants[index].name = format!("{}.{}", component.name, invariant.name);
            self.program.components[id.0].invariants.push(index);
        }
        let mut names = BTreeSet::new();
        for operation in &component.operations {
            let mut syntax = operation.transition.clone();
            if !names.insert(syntax.name.clone()) {
                return Err(diagnostic(
                    "D_DUPLICATE",
                    "component operation declared twice",
                    syntax.span,
                ));
            }
            syntax.name = format!("{}.{}", component.name, syntax.name);
            let first = self.program.expressions.len();
            let transition = self.lower_transition(&syntax)?;
            let scope = transition
                .params
                .iter()
                .map(|p| (p.name.clone(), p.binding))
                .collect();
            let reads = self.accesses(&syntax.reads, &scope)?;
            let writes = self.accesses(&syntax.writes, &scope)?;
            for access in reads.iter().chain(&writes) {
                self.require_owner(access.column.table, id, access.span)?;
                if access
                    .key
                    .is_some_and(|key| self.program.expr(key).state_dependent)
                {
                    return Err(diagnostic(
                        "D_OPERATION_FRAME",
                        "operation frame keys must depend only on input values, or use [*]",
                        access.span,
                    ));
                }
            }
            let mut effects = Vec::new();
            for name in &operation.effects {
                let effect = self.effect_names.get(name).copied().ok_or_else(|| {
                    diagnostic(
                        "D_UNKNOWN_EFFECT",
                        format!("unknown effect {name:?}"),
                        syntax.span,
                    )
                })?;
                if effects.contains(&effect) {
                    return Err(diagnostic(
                        "D_DUPLICATE",
                        "operation effect frame contains a duplicate",
                        syntax.span,
                    ));
                }
                effects.push(effect);
            }
            self.check_component_expressions(first, id)?;
            self.check_operation_statements(&transition.body, id, &effects)?;
            for expression in &self.program.expressions[first..] {
                if let ExprKind::Fold(fold) = &expression.kind {
                    self.check_operation_statements(&fold.body, id, &effects)?;
                }
            }
            // Contracts observe state too. A declared read frame covers both
            // implementation reads and the operation's public observations.
            let mut contract_reads = Vec::new();
            let mut ignored_tier = false;
            for ensure in &transition.ensures {
                self.collect_reads(ensure.value, &mut contract_reads, &mut ignored_tier);
            }
            for access in &contract_reads {
                if !self.covered(&reads, access) && !self.covered(&writes, access) {
                    return Err(diagnostic(
                        "D_OPERATION_FRAME",
                        "operation postcondition observes state outside its read/write frame",
                        access.span,
                    ));
                }
            }
            let operation = Operation {
                id: OperationId(self.program.operations.len()),
                component: id,
                transition,
                reads,
                writes,
                effects,
            };
            self.program.operations.push(operation);
        }
        self.active_component = None;
        Ok(())
    }

    fn require_owner(&self, table: TableId, owner: ComponentId, span: Span) -> Result<()> {
        if self.program.table_owner(table) != Some(owner) {
            return Err(diagnostic(
                "D_COMPONENT_OWNERSHIP",
                format!(
                    "component {:?} cannot access table {:?} it does not own",
                    self.program.component(owner).name,
                    self.program.table(table).name
                ),
                span,
            ));
        }
        Ok(())
    }
    fn check_component_expressions(&self, first: usize, owner: ComponentId) -> Result<()> {
        // Every lowered expression is visited, including dead branches, old
        // expressions, fold bodies, TableExists metadata and postconditions.
        for expression in &self.program.expressions[first..] {
            let table = match &expression.kind {
                ExprKind::TableGet { column, .. }
                | ExprKind::SlotRead { column, .. }
                | ExprKind::UniqueLookup { column, .. }
                | ExprKind::Sum { column, .. } => Some(column.table),
                ExprKind::TableExists { table, .. } => Some(*table),
                _ => None,
            };
            if let Some(table) = table {
                self.require_owner(table, owner, expression.span)?;
            }
        }
        Ok(())
    }
    pub(super) fn check_owned_writes(&self, body: &[Stmt]) -> Result<()> {
        for statement in body {
            let table = match &statement.kind {
                StmtKind::TableSet { column, .. } => Some(column.table),
                StmtKind::Allocate { table, .. } => Some(*table),
                _ => None,
            };
            if let Some(table) = table {
                if self.program.table_owner(table).is_some() {
                    return Err(diagnostic(
                        "D_COMPONENT_OWNERSHIP",
                        "owned tables can only be mutated through their component operations",
                        statement.span,
                    ));
                }
            }
        }
        Ok(())
    }
    fn check_operation_statements(
        &self,
        body: &[Stmt],
        owner: ComponentId,
        effects: &[EffectId],
    ) -> Result<()> {
        for statement in body {
            match &statement.kind {
                StmtKind::TableSet { column, .. } => self.require_owner(column.table, owner, statement.span)?,
                StmtKind::Allocate { .. } => return Err(diagnostic("D_OPERATION_ALLOCATION", "operation allocation requires an explicit allocation frame and is not yet supported", statement.span)),
                StmtKind::Emit { effect, .. } if !effects.contains(effect) => return Err(diagnostic("D_OPERATION_EFFECT", "operation emits an effect outside its declared frame", statement.span)),
                _ => (),
            }
        }
        Ok(())
    }

    pub(super) fn instantiate_frame(
        &mut self,
        accesses: &[Access],
        replacements: &BTreeMap<BindingId, ExprId>,
    ) -> Vec<Access> {
        accesses
            .iter()
            .map(|access| Access {
                key: access
                    .key
                    .map(|key| self.substitute_input(key, replacements)),
                ..access.clone()
            })
            .collect()
    }
    fn substitute_input(
        &mut self,
        id: ExprId,
        replacements: &BTreeMap<BindingId, ExprId>,
    ) -> ExprId {
        if let ExprKind::Binding(binding) = self.program.expr(id).kind {
            if let Some(value) = replacements.get(&binding) {
                return *value;
            }
        }
        let mut expression = self.program.expr(id).clone();
        let mut replace = |id: &mut ExprId| {
            *id = self.substitute_input(*id, replacements);
        };
        match &mut expression.kind {
            ExprKind::BytesLen { value }
            | ExprKind::BytesResize { value, .. }
            | ExprKind::Encode { value, .. }
            | ExprKind::CanDecode { value, .. }
            | ExprKind::Decode { value, .. } => replace(value),
            ExprKind::BytesGet { value, index } => {
                replace(value);
                replace(index);
            }
            ExprKind::BytesSlice {
                value,
                start,
                count,
                ..
            } => {
                replace(value);
                replace(start);
                replace(count);
            }
            ExprKind::BytesConcat { left, right, .. } | ExprKind::Binary { left, right, .. } => {
                replace(left);
                replace(right);
            }
            ExprKind::Record { fields, .. } => {
                for (_, value) in fields {
                    replace(value);
                }
            }
            ExprKind::Field { record, .. } => replace(record),
            ExprKind::Variant { payload, .. } => replace(payload),
            ExprKind::IsVariant { value, .. } | ExprKind::Payload { value, .. } => replace(value),
            ExprKind::Unary { operand, .. } => replace(operand),
            ExprKind::Select { condition, yes, no } => {
                replace(condition);
                replace(yes);
                replace(no);
            }
            ExprKind::Call { args, .. } => {
                for value in args {
                    replace(value);
                }
            }
            ExprKind::KeyOf { value, .. } => replace(value),
            ExprKind::Fold(fold) => {
                replace(&mut fold.init);
                replace(&mut fold.lo);
                replace(&mut fold.hi);
                for statement in &mut fold.body {
                    if let StmtKind::Let { value, .. } = &mut statement.kind {
                        replace(value);
                    }
                }
                replace(&mut fold.yield_value);
            }
            // Input-only frames exclude all state and outbox observations.
            _ => (),
        }
        expression.state_dependent = match &expression.kind {
            ExprKind::Binding(binding) => self.program.binding(*binding).state_dependent,
            _ => {
                expression.state_dependent
                    || replacements
                        .values()
                        .any(|id| self.program.expr(*id).state_dependent)
            }
        };
        let id = ExprId(self.program.expressions.len());
        self.program.expressions.push(expression);
        id
    }
}
