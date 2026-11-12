//! Independently admitted leaf operations and framed, body-independent summaries.
use super::*;

impl Encoding<'_> {
    pub(super) fn operation_call(
        &mut self,
        id: OperationId,
        args: &[ExprId],
        span: Span,
    ) -> LowerResult<()> {
        if !self.verified_operations.contains(&id) {
            return Err(diagnostic(
                "D_VERIFY_DEPENDENCY",
                "operation has no independently admitted contract",
                span,
            ));
        }
        let operation = self.program.operation(id).clone();
        let mut values = Vec::new();
        for (argument, parameter) in args.iter().zip(&operation.transition.params) {
            let value = self.expr(*argument)?;
            values.push(self.narrow(value, &parameter.ty));
        }
        let entry = self.current.clone();
        let saved_pre = self.invocation_pre.replace(entry.clone());
        let saved_env = self.env.clone();
        for (parameter, value) in operation.transition.params.iter().zip(values) {
            self.env.insert(parameter.binding, value);
        }
        // The caller must re-establish the owner's boundary invariant, even if
        // a preceding operation temporarily invalidated its own local state.
        let saved_target = self.target_operation.replace(id);
        let invariant = self.invariants(false)?;
        self.target_operation = saved_target;
        self.script.trap_if(&mut self.path, &not(&invariant));
        for requirement in &operation.transition.requires {
            let condition = self.expr(requirement.value)?;
            self.script.trap_if(&mut self.path, &not(&condition.text));
        }
        let mut frames = BTreeMap::<ColumnId, Vec<Option<Term>>>::new();
        for frame in &operation.writes {
            let key = frame.key.map(|key| self.expr(key)).transpose()?;
            frames.entry(frame.column).or_default().push(key);
        }
        self.summary_calls += 1;
        let call = self.summary_calls;
        let active = and([self.path.live.clone(), self.path.active.clone()]);
        for (column, keys) in frames {
            let info = self.program.column(column);
            // Unique columns are immutable except during allocation, which is
            // not part of leaf operation semantics.
            if info.unique {
                continue;
            }
            for (slot, row) in entry.tables[column.table.0].iter().enumerate() {
                let selected = or(keys.iter().map(|key| match key {
                    None => "true".into(),
                    Some(key) => eq(&self.slot(column.table, &key.text), &bv(slot, 64)),
                }));
                let hit = and([active.clone(), selected, row.occupied.text.clone()]);
                let fresh = self.script.declare(
                    &format!("op_{call}_t{}_c{}_s{slot}", column.table.0, column.index),
                    value_sort(&info.ty),
                );
                self.constrain_value(&fresh, &info.ty);
                let value = self.script.bind(
                    "operation_write",
                    ite(&hit, &fresh.text, &row.columns[column.index].text),
                    fresh.sort,
                );
                self.current.tables[column.table.0][slot].columns[column.index] = value;
                let key = Term::bits(self.key(column.table, slot, &row.generation), 64);
                self.current.writes.push(Write {
                    column,
                    key: key.clone(),
                    value: fresh,
                    active: hit.clone(),
                });
                self.current
                    .touched
                    .entry(column.table)
                    .or_default()
                    .push((key, hit));
            }
            if info.tracked_sum {
                let width = u32::from(
                    info.ty
                        .as_scalar()
                        .expect("tracked scalar")
                        .aggregate_width(),
                );
                let mut total = bv(0, width);
                for row in &self.current.tables[column.table.0] {
                    let value = &row.columns[column.index];
                    total = self
                        .script
                        .bind(
                            "operation_sum",
                            format!(
                                "(bvadd {total} {})",
                                extend(&value.text, value.width(), width, value_signed(&info.ty))
                            ),
                            Sort::Bits(width),
                        )
                        .text;
                }
                self.current.aggregates[column.table.0][column.index] =
                    Some(Term::bits(total, width));
            }
        }
        // The summary owns a local suffix. Contract observations are relative
        // to that suffix, never the caller's already-emitted prefix.
        let prior_count = self.effect_count.clone();
        let prior_tags = self.effect_tags.clone();
        let prior_args = self.effect_args.clone();
        self.script.enable_datatypes(); // General logic supports quantified array frames.
        let count = self
            .script
            .declare(&format!("op_{call}_count"), Sort::Bits(64));
        self.script
            .assert(implies(&not(&active), &eq(&count.text, &bv(0, 64))));
        self.script.assert(format!(
            "(bvule {} (bvsub {} {}))",
            count.text,
            bv(u64::MAX, 64),
            prior_count.text
        ));
        let tags = format!("op_{call}_tags");
        self.script.declaration(format!(
            "(declare-const {tags} (Array (_ BitVec 64) (_ BitVec 64)))"
        ));
        let allowed = or(operation
            .effects
            .iter()
            .map(|effect| eq(&format!("(select {tags} j)"), &bv(effect.0 + 1, 64))));
        self.script.assert(format!(
            "(forall ((j (_ BitVec 64))) (=> (bvult j {}) {allowed}))",
            count.text
        ));
        let mut suffix_args = BTreeMap::new();
        for effect in &operation.effects {
            for (index, parameter) in self.program.effect(*effect).params.iter().enumerate() {
                let name = format!("op_{call}_e{}_p{index}", effect.0);
                self.script.declaration(format!(
                    "(declare-const {name} (Array (_ BitVec 64) {}))",
                    value_sort(&parameter.ty).smt()
                ));
                let valid = valid_value(&parameter.ty, &format!("(select {name} j)"));
                self.script.assert(format!("(forall ((j (_ BitVec 64))) (=> (and (bvult j {}) (= (select {tags} j) {})) {valid}))", count.text, bv(effect.0 + 1, 64)));
                suffix_args.insert((*effect, index), name);
            }
        }
        self.effect_count = count.clone();
        self.effect_tags = tags.clone();
        self.effect_args = suffix_args.clone();
        for ensure in &operation.transition.ensures {
            let condition = self.spec(ensure.value, false)?;
            self.script.assert(implies(&active, &condition));
        }
        let saved_target = self.target_operation.replace(id);
        let invariant = self.invariants(false)?;
        self.target_operation = saved_target;
        self.script.assert(implies(&active, &invariant));
        self.invocation_pre = saved_pre;
        let combined_tags = format!("op_{call}_combined_tags");
        self.append_summary_array(
            &combined_tags,
            &prior_tags,
            &tags,
            &prior_count,
            Sort::Bits(64),
        );
        self.effect_tags = combined_tags;
        self.effect_args = prior_args.clone();
        for (key, suffix) in suffix_args {
            let sort = value_sort(&self.program.effect(key.0).params[key.1].ty);
            let prior = prior_args
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Self::empty_effect_array(sort));
            let name = format!("op_{call}_combined_e{}_p{}", key.0 .0, key.1);
            self.append_summary_array(&name, &prior, &suffix, &prior_count, sort);
            self.effect_args.insert(key, name);
        }
        self.effect_count = self.script.bind(
            "operation_effect_count",
            format!("(bvadd {} {})", prior_count.text, count.text),
            Sort::Bits(64),
        );
        self.env = saved_env;
        self.check_script_budget(span)
    }

    fn append_summary_array(
        &mut self,
        name: &str,
        prefix: &str,
        suffix: &str,
        offset: &Term,
        sort: Sort,
    ) {
        self.script.declaration(format!(
            "(declare-const {name} (Array (_ BitVec 64) {}))",
            sort.smt()
        ));
        self.script.assert(format!("(forall ((j (_ BitVec 64))) (= (select {name} j) (ite (bvult j {}) (select {prefix} j) (select {suffix} (bvsub j {})))))", offset.text, offset.text));
    }
}

impl VerificationSession<'_> {
    pub(super) fn operation(&mut self, id: OperationId) {
        if self.operation_results.contains_key(&id) {
            return;
        }
        let operation = self.program.operation(id).clone();
        let mut dependencies = BTreeSet::new();
        for invariant in self
            .program
            .invariants()
            .iter()
            .filter(|i| i.owner == Some(operation.component))
        {
            expression_dependencies(self.program, invariant.condition, &mut dependencies);
        }
        for frame in operation.reads.iter().chain(&operation.writes) {
            if let Some(key) = frame.key {
                expression_dependencies(self.program, key, &mut dependencies);
            }
        }
        for expression in operation
            .transition
            .requires
            .iter()
            .chain(&operation.transition.ensures)
        {
            expression_dependencies(self.program, expression.value, &mut dependencies);
        }
        statement_dependencies(self.program, &operation.transition.body, &mut dependencies);
        let dependencies = dependencies.into_iter().collect::<Vec<_>>();
        for dependency in &dependencies {
            self.function(*dependency);
        }
        let mut result = if !operation_dependencies(self.program, &operation.transition).is_empty()
        {
            failed(
                &operation.transition.name,
                diagnostic(
                    "D_VERIFY_DEPENDENCY",
                    "leaf operations cannot call operations",
                    operation.transition.span,
                ),
                self.config,
            )
        } else if dependencies.iter().any(|id| !self.verified.contains(id)) {
            failed(
                &operation.transition.name,
                diagnostic(
                    "D_VERIFY_DEPENDENCY",
                    "a called function failed verification",
                    operation.transition.span,
                ),
                self.config,
            )
        } else {
            verify_target(
                self.program,
                &operation.transition,
                self.config,
                &self.verified,
                None,
                Some(id),
                &self.verified_operations,
            )
        };
        result.target_kind = "operation".into();
        result.contract_dependencies = self.dependency_evidence(&dependencies);
        if result.ok() {
            self.verified_operations.insert(id);
        }
        self.operation_results.insert(id, result);
    }
}

/// Includes calls nested in expression folds, including call argument folds.
pub(super) fn operation_dependencies(
    program: &CheckedProgram,
    transition: &Transition,
) -> Vec<OperationId> {
    // Expressions are identity-indexed. Walk the operation-bearing statement
    // bodies through the entire reachable expression graph, without recursion.
    let mut statements = transition.body.iter().collect::<Vec<_>>();
    let mut expressions = transition
        .requires
        .iter()
        .chain(&transition.ensures)
        .map(|e| e.value)
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut operations = BTreeSet::new();
    loop {
        while let Some(statement) = statements.pop() {
            match &statement.kind {
                StmtKind::OperationCall {
                    operation, args, ..
                } => {
                    operations.insert(*operation);
                    expressions.extend(args);
                }
                StmtKind::Let { value, .. } | StmtKind::Eval(value) => expressions.push(*value),
                StmtKind::Guard { condition, .. } => expressions.push(*condition),
                StmtKind::TableSet { key, value, .. } => expressions.extend([key, value]),
                StmtKind::Allocate { fields, .. } => {
                    expressions.extend(fields.iter().map(|(_, e)| e))
                }
                StmtKind::Emit { args, .. } => expressions.extend(args),
                StmtKind::Commit => {}
            }
        }
        let Some(id) = expressions.pop() else {
            break;
        };
        if !seen.insert(id) {
            continue;
        }
        match &program.expr(id).kind {
            ExprKind::BytesLen { value }
            | ExprKind::BytesResize { value, .. }
            | ExprKind::Encode { value, .. }
            | ExprKind::CanDecode { value, .. }
            | ExprKind::Decode { value, .. }
            | ExprKind::Field { record: value, .. }
            | ExprKind::Variant { payload: value, .. }
            | ExprKind::IsVariant { value, .. }
            | ExprKind::Payload { value, .. }
            | ExprKind::Unary { operand: value, .. }
            | ExprKind::UniqueLookup { value, .. }
            | ExprKind::KeyOf { value, .. } => expressions.push(*value),
            ExprKind::BytesGet { value, index } => expressions.extend([value, index]),
            ExprKind::BytesConcat { left, right, .. } | ExprKind::Binary { left, right, .. } => {
                expressions.extend([left, right])
            }
            ExprKind::BytesSlice {
                value,
                start,
                count,
                ..
            } => expressions.extend([value, start, count]),
            ExprKind::Record { fields, .. } => expressions.extend(fields.iter().map(|(_, e)| e)),
            ExprKind::Call { args, .. } => expressions.extend(args),
            ExprKind::Select { condition, yes, no } => expressions.extend([condition, yes, no]),
            ExprKind::TableGet { key, .. }
            | ExprKind::SlotRead { key, .. }
            | ExprKind::TableExists { key, .. } => expressions.push(*key),
            ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => {
                expressions.push(*index)
            }
            ExprKind::Fold(fold) => {
                expressions.extend([fold.init, fold.lo, fold.hi, fold.yield_value]);
                statements.extend(&fold.body);
            }
            ExprKind::Constant(_)
            | ExprKind::Binding(_)
            | ExprKind::Sum { .. }
            | ExprKind::EffectCount => {}
        }
    }
    operations.into_iter().collect()
}

pub fn verify_operation(
    program: &CheckedProgram,
    name: &str,
    config: &VerifierConfig,
) -> VerificationResult {
    let Some(id) = program.operation_id(name) else {
        let mut result = failed(
            name,
            diagnostic(
                "D_UNKNOWN_OPERATION",
                format!("unknown operation {name:?}"),
                Span::default(),
            ),
            config,
        );
        result.target_kind = "operation".into();
        return result;
    };
    let mut session = VerificationSession::new(program, config);
    session.operation(id);
    session.operation_results[&id].clone()
}

/// Replay a contract-expression failure, not an invented body/runtime trap.
/// Frame expressions are observed only after all ordered requirements succeed.
pub(super) fn replay_frame_definedness(
    encoding: &Encoding<'_>,
    candidate: &Candidate,
) -> Result<Option<Counterexample>, String> {
    let Some(id) = encoding.target_operation else {
        return Ok(None);
    };
    let operation = encoding.program.operation(id);
    let bindings = crate::runtime::bind_params(
        encoding.program,
        &operation.transition.params,
        &candidate.params,
    )
    .map_err(|error| format!("frame parameter admission: {}", error.code))?;
    for require in &operation.transition.requires {
        if eval_expression(
            encoding.program,
            &candidate.state,
            &candidate.state,
            &bindings,
            require.value,
        )
        .ok()
            != Some(Value::Bool(true))
        {
            return Ok(None);
        }
    }
    for (mode, frames) in [("read", &operation.reads), ("write", &operation.writes)] {
        for frame in frames {
            let Some(key) = frame.key else {
                continue;
            };
            if let Err(error) = eval_expression(
                encoding.program,
                &candidate.state,
                &candidate.state,
                &bindings,
                key,
            ) {
                let law = format!("undefined {mode} frame key: {}", error.code);
                return Ok(Some(Counterexample {
                    target_kind: "operation".into(), return_value: None, typed_return_value: None,
                    transition: operation.transition.name.clone(), obligation: "panic".into(), law: law.clone(),
                    params: candidate.params.clone(), pre_state: candidate.state.clone(),
                    trace: vec![TraceStep { span: encoding.program.expr(key).span, kind: "frame_contract".into(), text: law.clone(), value: None }],
                    failure: None, violated: vec![law], emitted_effects: Vec::new(), failed_ensures: Vec::new(),
                    directive: "Make the declared frame key defined for every input satisfying the operation requirements; this is a contract failure, not a trap observed in the operation body.".into(),
                }));
            }
        }
    }
    Ok(None)
}
