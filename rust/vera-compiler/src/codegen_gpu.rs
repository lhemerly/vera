//! Native lowering of checked VERA programs to exact GPU compute kernels.
//!
//! A shared eight-limb representation keeps scalars and aggregate carriers
//! exact without requiring native 64-bit shader integers. Arithmetic templates
//! are target syntax, not an interpreter or a compiler subprocess bridge.

use std::collections::BTreeMap;

use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::{
    diagnostic::{Diagnostic, Span},
    ir::{
        BinaryOp, BindingId, CheckedProgram, ColumnId, ExprId, ExprKind, Snapshot, Stmt, StmtKind,
        TableId, Transition, UnaryOp,
    },
    types::{ScalarType, MAX_PROBES},
    value::{symbol_id, Value},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuTarget {
    Glsl,
    Wgsl,
    Cuda,
}

impl GpuTarget {
    fn context(self) -> &'static str {
        if self == Self::Glsl {
            "ctx"
        } else {
            "(*ctx)"
        }
    }

    fn math(self) -> &'static str {
        match self {
            Self::Glsl => include_str!("../templates/gpu/math.glsl"),
            Self::Wgsl => include_str!("../templates/gpu/math.wgsl"),
            Self::Cuda => include_str!("../templates/gpu/math.cuda"),
        }
    }
}

fn word_end(source: &str, start: usize) -> usize {
    start
        + source[start..]
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count()
}

fn unsigned_type(source: &str, name: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut start = 0;
    while start < source.len() {
        let end = word_end(source, start);
        if end == start {
            let ch = source[start..].chars().next().expect("nonempty suffix");
            result.push(ch);
            start += ch.len_utf8();
        } else {
            let word = &source[start..end];
            result.push_str(if word == "u32" { name } else { word });
            start = end;
        }
    }
    result
}

/// The shared emitter writes fixed C-style declarations. Translate those small
/// fragments directly; source-language syntax has already been resolved in IR.
fn declarations(source: &str, target: GpuTarget) -> String {
    let mut result = String::with_capacity(source.len());
    let mut start = 0;
    while start < source.len() {
        let end = word_end(source, start);
        if end == start {
            let ch = source[start..].chars().next().expect("nonempty suffix");
            result.push(ch);
            start += ch.len_utf8();
            continue;
        }
        let ty = &source[start..end];
        if let Some((name, size, value, next)) = declaration(source, end, ty) {
            let initializer = value.map(|v| format!(" = {v}")).unwrap_or_default();
            if target == GpuTarget::Wgsl {
                let ty = size
                    .map(|n| format!("array<{ty}, {n}>"))
                    .unwrap_or_else(|| ty.to_owned());
                result.push_str(&format!("var {name}: {ty}{initializer};"));
            } else {
                let size = size.map(|n| format!("[{n}]")).unwrap_or_default();
                result.push_str(&format!("{ty} {name}{size}{initializer};"));
            }
            start = next;
        } else {
            result.push_str(ty);
            start = end;
        }
    }
    result
}

type Declaration<'a> = (&'a str, Option<&'a str>, Option<&'a str>, usize);

fn declaration<'a>(source: &'a str, mut next: usize, ty: &str) -> Option<Declaration<'a>> {
    if !matches!(ty, "u32" | "bool" | "V" | "W" | "R" | "D" | "Ctx" | "State") {
        return None;
    }
    let bytes = source.as_bytes();
    if !bytes.get(next)?.is_ascii_whitespace() {
        return None;
    }
    while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
        next += 1;
    }
    if !bytes
        .get(next)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return None;
    }
    let end = word_end(source, next);
    let name = &source[next..end];
    next = end;
    let mut size = None;
    if bytes.get(next) == Some(&b'[') {
        next += 1;
        let start = next;
        while bytes.get(next).is_some_and(u8::is_ascii_digit) {
            next += 1;
        }
        if start == next || bytes.get(next) != Some(&b']') {
            return None;
        }
        size = Some(&source[start..next]);
        next += 1;
    }
    while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
        next += 1;
    }
    let mut value = None;
    if bytes.get(next) == Some(&b'=') {
        next += 1;
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        let start = next;
        while bytes.get(next).is_some_and(|b| *b != b';' && *b != b'\n') {
            next += 1;
        }
        if start == next {
            return None;
        }
        value = Some(&source[start..next]);
    }
    if bytes.get(next) != Some(&b';') {
        return None;
    }
    Some((name, size, value, next + 1))
}

struct Output {
    target: GpuTarget,
    source: String,
    body: Vec<String>,
    counter: usize,
}

impl Output {
    fn new(target: GpuTarget) -> Self {
        Self {
            target,
            source: String::new(),
            body: Vec::new(),
            counter: 0,
        }
    }

    fn line(&mut self, text: impl AsRef<str>) {
        self.source.push_str(text.as_ref());
        self.source.push('\n');
    }

    fn variable(&self, name: &str, ty: &str, initial: Option<&str>) -> String {
        let initializer = initial.map(|v| format!(" = {v}")).unwrap_or_default();
        if self.target == GpuTarget::Wgsl {
            format!("var {name}: {ty}{initializer};")
        } else {
            format!("{ty} {name}{initializer};")
        }
    }

    fn temporary(&mut self, ty: &str, value: &str) -> String {
        self.counter += 1;
        let name = format!("tmp_{}", self.counter);
        self.body.push(self.variable(&name, ty, Some(value)));
        name
    }

    fn syntax(&self, body: &str) -> String {
        let body = declarations(body, self.target);
        match self.target {
            GpuTarget::Wgsl => body,
            GpuTarget::Glsl => unsigned_type(&body, "uint"),
            GpuTarget::Cuda => unsigned_type(&body, "uint32_t"),
        }
    }

    fn context_parameters(&self, rest: Vec<(String, String)>) -> Vec<(String, String)> {
        let ty = match self.target {
            GpuTarget::Wgsl => "ptr<function, Ctx>",
            GpuTarget::Glsl => "inout Ctx",
            GpuTarget::Cuda => "Ctx*",
        };
        std::iter::once(("ctx".into(), ty.into()))
            .chain(rest)
            .collect()
    }

    fn function(&mut self, name: &str, result: &str, params: &[(String, String)], body: &str) {
        if self.target == GpuTarget::Wgsl {
            let parameters = params
                .iter()
                .map(|(name, ty)| format!("{name}: {ty}"))
                .collect::<Vec<_>>()
                .join(", ");
            let result = if result == "void" {
                String::new()
            } else {
                format!(" -> {result}")
            };
            self.line(format!("fn {name}({parameters}){result} {{"));
        } else {
            let parameters = params
                .iter()
                .map(|(name, ty)| format!("{ty} {name}"))
                .collect::<Vec<_>>()
                .join(", ");
            let prefix = if self.target == GpuTarget::Cuda {
                "__device__ inline "
            } else {
                ""
            };
            self.line(format!("{prefix}{result} {name}({parameters}) {{"));
        }
        self.line(self.syntax(body));
        self.line("}\n");
    }

    fn structure(&mut self, name: &str, fields: &[(&str, &str, Option<usize>)]) {
        self.line(format!("struct {name} {{"));
        for (field, ty, size) in fields {
            if self.target == GpuTarget::Wgsl {
                let ty = size
                    .map(|n| format!("array<{ty}, {}>", n.max(1)))
                    .unwrap_or_else(|| (*ty).into());
                self.line(format!("    {field}: {ty},"));
            } else {
                let size = size.map(|n| format!("[{}]", n.max(1))).unwrap_or_default();
                self.line(format!("    {ty} {field}{size};"));
            }
        }
        self.line(if self.target == GpuTarget::Wgsl {
            "}"
        } else {
            "};"
        });
    }

    fn preamble(
        &mut self,
        rows: usize,
        columns: usize,
        aggregates: usize,
        hashes: usize,
        effects: Option<(usize, usize)>,
    ) {
        self.line("// Generated by VERA: exact 32-bit-limb GPU semantics.");
        match self.target {
            GpuTarget::Glsl => self.line("#version 460 core\nlayout(local_size_x=32) in;\nlayout(std430,binding=0) buffer Output { uint violations; uint committed; uint reverted; uint effects; };"),
            GpuTarget::Wgsl => self.line("struct Output { violations:atomic<u32>, committed:atomic<u32>, reverted:atomic<u32>, effects:atomic<u32>, }\n@group(0) @binding(0) var<storage,read_write> output:Output;"),
            GpuTarget::Cuda => self.line("#include <cstdint>\n#ifndef VERA_GPU_HOST\n#include <cuda_runtime.h>\n#else\n#define __device__\n#define __global__\nstruct VeraDim { uint32_t x; };\nstatic VeraDim blockIdx={0}, blockDim={32}, threadIdx={0};\ninline void atomicAdd(uint32_t* p,uint32_t x){*p+=x;}\n#endif"),
        }
        self.structure("V", &[("w", "u32", Some(8))]);
        self.structure("W", &[("w", "u32", Some(16))]);
        self.structure("R", &[("value", "V", None), ("ok", "bool", None)]);
        self.structure("D", &[("q", "W", None), ("r", "W", None)]);
        self.structure(
            "State",
            &[
                ("occ", "u32", Some(rows)),
                ("gen", "u32", Some(rows)),
                ("cols", "V", Some(columns)),
                ("aggs", "V", Some(aggregates)),
                ("hused", "u32", Some(hashes)),
                ("hslot", "u32", Some(hashes)),
                ("effect_count", "u32", None),
                ("effect_digest", "V", None),
            ],
        );
        let mut context = vec![
            ("s", "State", None),
            ("pre", "State", None),
            ("rng", "u32", Some(4)),
            ("trap", "u32", None),
        ];
        if let Some((slots, payloads)) = effects {
            context.extend([
                ("emitted_count", "u32", None),
                ("emitted_kind", "u32", Some(slots)),
                ("emitted_args", "V", Some(payloads)),
            ]);
        }
        self.structure("Ctx", &context);
        self.line(self.target.math());
    }

    fn finish(self) -> String {
        match self.target {
            GpuTarget::Wgsl => self.source,
            GpuTarget::Glsl => unsigned_type(&self.source, "uint"),
            GpuTarget::Cuda => unsigned_type(&self.source, "uint32_t"),
        }
    }
}

type Environment = BTreeMap<BindingId, String>;

/// Generate a complete compute furnace without requiring native 64-bit shader
/// integers. The same callable transition helpers are usable by conformance
/// harnesses and applications which supply their own dispatch entry point.
pub fn generate_gpu(
    program: &CheckedProgram,
    target: GpuTarget,
    depth: u32,
) -> Result<String, Diagnostic> {
    Generator::new(program, target, depth).map(Generator::generate)
}

pub fn generate_glsl(program: &CheckedProgram, depth: u32) -> Result<String, Diagnostic> {
    generate_gpu(program, GpuTarget::Glsl, depth)
}

pub fn generate_wgsl(program: &CheckedProgram, depth: u32) -> Result<String, Diagnostic> {
    generate_gpu(program, GpuTarget::Wgsl, depth)
}

pub fn generate_cuda(program: &CheckedProgram, depth: u32) -> Result<String, Diagnostic> {
    generate_gpu(program, GpuTarget::Cuda, depth)
}

// Structural bounds count evaluated operands and both select branches. They
// deliberately overestimate but never truncate execution or expand fold bodies.
fn expression_emissions(program: &CheckedProgram, id: ExprId) -> usize {
    let child = |id| expression_emissions(program, id);
    match &program.expr(id).kind {
        ExprKind::Unary { operand, .. } => child(*operand),
        ExprKind::Binary { left, right, .. } => child(*left).saturating_add(child(*right)),
        ExprKind::Select { condition, yes, no } => child(*condition)
            .saturating_add(child(*yes))
            .saturating_add(child(*no)),
        ExprKind::TableGet { key, .. }
        | ExprKind::SlotRead { key, .. }
        | ExprKind::TableExists { key, .. } => child(*key),
        ExprKind::UniqueLookup { value, .. } | ExprKind::KeyOf { value, .. } => child(*value),
        ExprKind::Fold(fold) => child(fold.init)
            .saturating_add(child(fold.lo))
            .saturating_add(child(fold.hi))
            .saturating_add(
                emission_bound(program, &fold.body)
                    .saturating_add(child(fold.yield_value))
                    .saturating_mul(fold.limit as usize),
            ),
        ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => child(*index),
        ExprKind::Constant(_)
        | ExprKind::Binding(_)
        | ExprKind::Sum { .. }
        | ExprKind::EffectCount => 0,
    }
}
fn emission_bound(program: &CheckedProgram, statements: &[Stmt]) -> usize {
    statements.iter().fold(0usize, |total, statement| {
        let child = |id| expression_emissions(program, id);
        let count = match &statement.kind {
            StmtKind::Let { value, .. } | StmtKind::Eval(value) => child(*value),
            StmtKind::Guard { condition, .. } => child(*condition),
            StmtKind::TableSet { key, value, .. } => child(*key).saturating_add(child(*value)),
            StmtKind::Allocate { fields, .. } => fields
                .iter()
                .fold(0usize, |n, (_, id)| n.saturating_add(child(*id))),
            StmtKind::Emit { args, .. } => args
                .iter()
                .fold(1usize, |n, id| n.saturating_add(child(*id))),
            StmtKind::Commit => 0,
        };
        total.saturating_add(count)
    })
}
// None means no effect query; Some(0) needs only count/tags. Payload
// prefixes are sized per transition, avoiding a cross-product between a large
// event count in one component and a wide payload in an unrelated component.
fn effect_stride(program: &CheckedProgram, id: ExprId) -> Option<usize> {
    let child = |id| effect_stride(program, id);
    match &program.expr(id).kind {
        ExprKind::EffectCount => Some(0),
        ExprKind::EffectIs { index, .. } => Some(0).max(child(*index)),
        ExprKind::EffectArg {
            parameter, index, ..
        } => Some(parameter + 1).max(child(*index)),
        ExprKind::Unary { operand, .. } => child(*operand),
        ExprKind::Binary { left, right, .. } => child(*left).max(child(*right)),
        ExprKind::Select { condition, yes, no } => {
            child(*condition).max(child(*yes)).max(child(*no))
        }
        ExprKind::TableGet { key, .. }
        | ExprKind::SlotRead { key, .. }
        | ExprKind::TableExists { key, .. } => child(*key),
        ExprKind::UniqueLookup { value, .. } | ExprKind::KeyOf { value, .. } => child(*value),
        ExprKind::Fold(fold) => child(fold.init)
            .max(child(fold.lo))
            .max(child(fold.hi))
            .max(child(fold.yield_value))
            .max(
                fold.body
                    .iter()
                    .filter_map(|statement| match statement.kind {
                        StmtKind::Let { value, .. } | StmtKind::Eval(value) => child(value),
                        _ => None, // Specification folds can only bind pure values.
                    })
                    .max(),
            ),
        ExprKind::Constant(_) | ExprKind::Binding(_) | ExprKind::Sum { .. } => None,
    }
}
fn transition_effect_stride(program: &CheckedProgram, transition: &Transition) -> Option<usize> {
    transition
        .ensures
        .iter()
        .filter_map(|ensure| effect_stride(program, ensure.value))
        .max()
}

struct Generator<'a> {
    program: &'a CheckedProgram,
    output: Output,
    depth: u32,
    rows: usize,
    columns: usize,
    aggregates: usize,
    hashes: usize,
    table_offsets: Vec<usize>,
    column_offsets: Vec<Vec<usize>>,
    aggregate_offsets: Vec<Vec<Option<usize>>>,
    hash_offsets: Vec<Vec<Option<usize>>>,
    effect_storage: Option<(usize, usize)>,
    record_effects: bool,
    effect_stride: usize,
}

impl<'a> Generator<'a> {
    fn new(program: &'a CheckedProgram, target: GpuTarget, depth: u32) -> Result<Self, Diagnostic> {
        let error = |code: &str, message: &str, span: Span| Diagnostic {
            code: code.into(),
            message: message.into(),
            span,
        };
        if depth == 0 {
            return Err(error(
                "D_GPU",
                "furnace depth must be positive",
                Span::default(),
            ));
        }
        let mut result = Self {
            program,
            output: Output::new(target),
            depth,
            rows: 0,
            columns: 0,
            aggregates: 0,
            hashes: 0,
            table_offsets: Vec::new(),
            column_offsets: Vec::new(),
            aggregate_offsets: Vec::new(),
            hash_offsets: Vec::new(),
            effect_storage: None,
            record_effects: false,
            effect_stride: 0,
        };
        for table in program.tables() {
            if table.key_bits() >= 32 {
                return Err(error(
                    "D_GPU_RESOURCE",
                    "GPU capacities must fit a 32-bit slot index",
                    table.span,
                ));
            }
            let capacity = table.capacity as usize;
            result.table_offsets.push(result.rows);
            result.rows += capacity;
            let mut columns = Vec::new();
            let mut aggregates = Vec::new();
            let mut hashes = Vec::new();
            for column in &table.columns {
                columns.push(result.columns);
                result.columns += capacity;
                aggregates.push(if column.tracked_sum {
                    let offset = result.aggregates;
                    result.aggregates += 1;
                    Some(offset)
                } else {
                    None
                });
                hashes.push(if column.unique {
                    let offset = result.hashes;
                    result.hashes += table.layout.unique_buckets[column.id.index]
                        .expect("checked unique layout")
                        as usize;
                    Some(offset)
                } else {
                    None
                });
            }
            result.column_offsets.push(columns);
            result.aggregate_offsets.push(aggregates);
            result.hash_offsets.push(hashes);
            if [
                result.rows,
                result.columns,
                result.aggregates,
                result.hashes,
            ]
            .iter()
            .any(|&n| n > u32::MAX as usize)
            {
                return Err(error(
                    "D_GPU_RESOURCE",
                    "GPU state arrays exceed 32-bit indexing",
                    table.span,
                ));
            }
        }
        if program.expressions().iter().any(|expression| {
            matches!(
                expression.kind,
                ExprKind::EffectCount | ExprKind::EffectIs { .. } | ExprKind::EffectArg { .. }
            )
        }) {
            let mut slots = 1usize;
            let mut payloads = 1usize;
            for transition in program.transitions() {
                if let Some(stride) = transition_effect_stride(program, transition) {
                    let bound = emission_bound(program, &transition.body);
                    slots = slots.max(bound);
                    payloads = payloads.max(bound.saturating_mul(stride));
                }
            }
            // This is transaction-local storage, not a finite approximation of
            // the persistent outbox. Refuse an oversized exact model explicitly.
            if slots.saturating_add(payloads) > 4096 {
                return Err(error(
                    "D_GPU_RESOURCE",
                    "effect contract buffer exceeds 4096 tag/payload cells",
                    Span::default(),
                ));
            }
            result.effect_storage = Some((slots, payloads));
        }
        Ok(result)
    }

    fn table_name(table: TableId) -> String {
        format!("vera_table_{}", table.0)
    }

    fn column_name(column: ColumnId) -> String {
        format!("vera_table_{}_column_{}", column.table.0, column.index)
    }

    fn transition_name(transition: &Transition) -> String {
        format!("transition_v_{}", transition.name)
    }

    fn constant(value: &BigInt) -> String {
        let mask = (BigInt::from(1u8) << 256usize) - 1u8;
        let raw = value & mask;
        let words = (0..8)
            .map(|i| {
                let limb = (&raw >> (i * 32usize)) & BigInt::from(u32::MAX);
                format!("{}u", limb.to_u32().expect("masked limb fits u32"))
            })
            .collect::<Vec<_>>();
        format!("v_make({})", words.join(","))
    }

    fn bool_text(value: bool) -> &'static str {
        if value {
            "true"
        } else {
            "false"
        }
    }

    fn flag(snapshot: Snapshot) -> &'static str {
        Self::bool_text(snapshot == Snapshot::Pre)
    }

    fn context(&self) -> &'static str {
        self.output.target.context()
    }

    fn push(&mut self, line: impl Into<String>) {
        self.output.body.push(line.into());
    }

    fn temporary(&mut self, ty: &str, value: impl AsRef<str>) -> String {
        self.output.temporary(ty, value.as_ref())
    }

    fn context_function(
        &mut self,
        name: &str,
        result: &str,
        params: Vec<(String, String)>,
        body: &str,
    ) {
        let params = self.output.context_parameters(params);
        self.output.function(name, result, &params, body);
    }

    fn finish_function(&mut self, name: &str, result: &str, params: Vec<(String, String)>) {
        let body = std::mem::take(&mut self.output.body).join("\n");
        self.context_function(name, result, params, &body);
    }

    fn generate(mut self) -> String {
        self.output.preamble(
            self.rows,
            self.columns,
            self.aggregates,
            self.hashes,
            self.effect_storage,
        );
        self.table_helpers();
        self.invariants();
        for transition in self.program.transitions() {
            self.transition(transition);
        }
        self.initialize();
        self.main();
        self.output.finish()
    }

    fn table_helpers(&mut self) {
        let c = self.context();
        for table in self.program.tables() {
            let tn = Self::table_name(table.id);
            let cap = table.capacity;
            let kb = table.key_bits();
            let off = self.table_offsets[table.id.0];
            self.output.function(
                &format!("{tn}_slot"),
                "u32",
                &[("key".into(), "V".into())],
                &format!("return key.w[0]&{}u;", cap - 1),
            );
            let generation = if kb == 0 {
                "key.w[0]".into()
            } else {
                format!("((key.w[0]>>{kb}u)|(key.w[1]<<{}u))", 32 - kb)
            };
            let high = if kb == 0 {
                "key.w[1]!=0u".into()
            } else {
                format!("(key.w[1]>>{kb}u)!=0u")
            };
            self.context_function(&format!("{tn}_exists"), "bool", vec![("key".into(), "V".into()), ("old_".into(), "bool".into())], &format!(
                "for(u32 i=2u;i<8u;i=i+1u){{if(key.w[i]!=0u){{return false;}}}}if({high}){{return false;}}u32 slot={tn}_slot(key)+{off}u;u32 generation={generation};if(old_){{return {c}.pre.occ[slot]!=0u && {c}.pre.gen[slot]==generation;}}return {c}.s.occ[slot]!=0u && {c}.s.gen[slot]==generation;"
            ));
            self.context_function(&format!("{tn}_key"), "V", vec![("slot".into(), "u32".into()), ("old_".into(), "bool".into())], &format!(
                "u32 generation={c}.s.gen[slot+{off}u];if(old_){{generation={c}.pre.gen[slot+{off}u];}}return v_add(v_shl(v_small(generation),{kb}u),v_small(slot));"
            ));
            for column in &table.columns {
                let cn = Self::column_name(column.id);
                let co = self.column_offsets[table.id.0][column.id.index];
                self.context_function(&format!("{cn}_get"), "V", vec![("key".into(), "V".into()), ("old_".into(), "bool".into())], &format!(
                    "u32 slot={tn}_slot(key)+{co}u;if(old_){{return {c}.pre.cols[slot];}}return {c}.s.cols[slot];"
                ));
                let mut body = format!("u32 slot={tn}_slot(key)+{co}u;");
                if let Some(ao) = self.aggregate_offsets[table.id.0][column.id.index] {
                    body.push_str(&format!(
                        "{c}.s.aggs[{ao}]=v_add(v_sub({c}.s.aggs[{ao}],{c}.s.cols[slot]),value);"
                    ));
                }
                body.push_str(&format!("{c}.s.cols[slot]=value;"));
                self.context_function(
                    &format!("{cn}_set"),
                    "void",
                    vec![("key".into(), "V".into()), ("value".into(), "V".into())],
                    &body,
                );
                if column.unique {
                    let ho =
                        self.hash_offsets[table.id.0][column.id.index].expect("unique hash offset");
                    let mask = table.layout.unique_buckets[column.id.index]
                        .expect("unique bucket count")
                        - 1;
                    for (state, suffix, flag) in [("s", "", "false"), ("pre", "_pre", "true")] {
                        self.context_function(&format!("{cn}_lookup{suffix}"), "R", vec![("value".into(), "V".into())], &format!(
                            "V hash=vera_mix(value);u32 pos=hash.w[0]&{mask}u;for(u32 distance=0u;distance<{MAX_PROBES}u;distance=distance+1u){{if({c}.{state}.hused[pos+{ho}u]==0u){{return v_result(v_zero(),false);}}u32 slot={c}.{state}.hslot[pos+{ho}u];V existing={c}.{state}.cols[slot+{co}u];if(v_eq(existing,value)){{return v_result({tn}_key(ctx,slot,{flag}),true);}}V h=vera_mix(existing);u32 rd=(pos-(h.w[0]&{mask}u))&{mask}u;if(rd<distance){{return v_result(v_zero(),false);}}pos=(pos+1u)&{mask}u;}}return v_result(v_zero(),false);"
                        ));
                    }
                    self.context_function(&format!("{cn}_insert"), "bool", vec![("value".into(), "V".into()), ("slot_".into(), "u32".into())], &format!(
                        "V hash=vera_mix(value);u32 pos=hash.w[0]&{mask}u;u32 distance=0u;u32 slot=slot_;for(u32 step=0u;step<{MAX_PROBES}u;step=step+1u){{if({c}.s.hused[pos+{ho}u]==0u){{{c}.s.hused[pos+{ho}u]=1u;{c}.s.hslot[pos+{ho}u]=slot;return true;}}u32 existing={c}.s.hslot[pos+{ho}u];V oldvalue={c}.s.cols[existing+{co}u];if(v_eq(oldvalue,{c}.s.cols[slot+{co}u])){{return false;}}V oldhash=vera_mix(oldvalue);u32 olddistance=(pos-(oldhash.w[0]&{mask}u))&{mask}u;if(olddistance<distance){{{c}.s.hslot[pos+{ho}u]=slot;slot=existing;distance=olddistance;}}pos=(pos+1u)&{mask}u;distance=distance+1u;}}return false;"
                    ));
                }
            }
            let params = table
                .columns
                .iter()
                .map(|col| (format!("arg_v_{}", col.name), "V".into()))
                .collect();
            let mut body = format!("u32 slot={cap}u;for(u32 i=0u;i<{cap}u;i=i+1u){{if({c}.s.occ[i+{off}u]==0u){{slot=i;break;}}}}if(slot=={cap}u){{return v_result(v_zero(),false);}}if({c}.s.gen[slot+{off}u]==4294967295u){{return v_result(v_zero(),false);}}{c}.s.occ[slot+{off}u]=1u;{c}.s.gen[slot+{off}u]={c}.s.gen[slot+{off}u]+1u;");
            for column in &table.columns {
                let co = self.column_offsets[table.id.0][column.id.index];
                let value = format!("arg_v_{}", column.name);
                body.push_str(&format!("{c}.s.cols[slot+{co}u]={value};"));
                if let Some(ao) = self.aggregate_offsets[table.id.0][column.id.index] {
                    body.push_str(&format!(
                        "{c}.s.aggs[{ao}]=v_add({c}.s.aggs[{ao}],{value});"
                    ));
                }
            }
            for column in table.columns.iter().filter(|col| col.unique) {
                body.push_str(&format!(
                    "if(!{}_insert(ctx,arg_v_{},slot)){{return v_result(v_zero(),false);}}",
                    Self::column_name(column.id),
                    column.name
                ));
            }
            body.push_str(&format!("return v_result({tn}_key(ctx,slot,false),true);"));
            self.context_function(&format!("{tn}_alloc"), "R", params, &body);
        }
    }

    fn accepts(value: &str, ty: &ScalarType) -> String {
        match ty {
            ScalarType::Bool => format!("v_eq({value},v_zero()) || v_eq({value},v_small(1u))"),
            ScalarType::Enum(tags) => format!(
                "v_ucmp({value},{})==0u",
                Self::constant(&BigInt::from(tags.len()))
            ),
            _ => format!(
                "v_fits({value},{}u,{})",
                ty.bits(),
                Self::bool_text(ty.signed())
            ),
        }
    }

    fn narrow(&mut self, value: &str, ty: &ScalarType) {
        self.push(format!(
            "if(!({})){{{}.trap=1u;return false;}}",
            Self::accepts(value, ty),
            self.context()
        ));
    }

    fn typed_expression(&mut self, id: ExprId, env: &Environment, ty: &ScalarType) -> String {
        let value = self.expression(id, env);
        self.narrow(&value, ty);
        value
    }

    fn expression(&mut self, id: ExprId, env: &Environment) -> String {
        let expression = self.program.expr(id);
        let c = self.context();
        match &expression.kind {
            ExprKind::Constant(value) => {
                let value = match value {
                    Value::Bool(v) => BigInt::from(u8::from(*v)),
                    Value::Int(v) => v.clone(),
                };
                self.temporary("V", Self::constant(&value))
            }
            ExprKind::Binding(binding) => env
                .get(binding)
                .expect("checked binding is in GPU scope")
                .clone(),
            ExprKind::EffectCount => self.temporary("V", format!("v_small({c}.emitted_count)")),
            ExprKind::EffectIs { effect, index } | ExprKind::EffectArg { effect, index, .. } => {
                let index = self.expression(*index, env);
                let found = self.temporary("bool", "false");
                // Check all limbs before indexing. Short-circuiting is not relied
                // upon for out-of-range storage accesses on any shader target.
                self.push(format!("if(v_ucmp({index},v_small({c}.emitted_count))==0u){{{found}=({c}.emitted_kind[{index}.w[0]]=={}u);}}", effect.0));
                if let ExprKind::EffectArg { parameter, .. } = &expression.kind {
                    self.push(format!("if(!{found}){{{c}.trap=1u;return false;}}"));
                    let stride = self.effect_stride;
                    self.temporary(
                        "V",
                        format!("{c}.emitted_args[{index}.w[0]*{stride}u+{parameter}u]"),
                    )
                } else {
                    let value = self.temporary("V", "v_zero()");
                    self.push(format!("if({found}){{{value}=v_small(1u);}}"));
                    value
                }
            }
            ExprKind::KeyOf { value, .. } => {
                let value = self.expression(*value, env);
                self.temporary("V", format!("v_norm({value},64u,false)"))
            }
            ExprKind::Select { condition, yes, no } => {
                let cond = self.expression(*condition, env);
                let out = self.temporary("V", "v_zero()");
                self.push(format!("if(v_truth({cond})){{"));
                let yes = self.expression(*yes, env);
                self.push(format!("{out}={yes};}}else{{"));
                let no = self.expression(*no, env);
                self.push(format!("{out}={no};}}"));
                out
            }
            ExprKind::TableGet { column, key, state }
            | ExprKind::SlotRead { column, key, state } => {
                let key = self.expression(*key, env);
                if matches!(&expression.kind, ExprKind::TableGet { .. }) {
                    self.push(format!(
                        "if(!{}_exists(ctx,{key},{})){{{c}.trap=1u;return false;}}",
                        Self::table_name(column.table),
                        Self::flag(*state)
                    ));
                }
                self.temporary(
                    "V",
                    format!(
                        "{}_get(ctx,{key},{})",
                        Self::column_name(*column),
                        Self::flag(*state)
                    ),
                )
            }
            ExprKind::TableExists { table, key, state } => {
                let key = self.expression(*key, env);
                self.temporary(
                    "V",
                    format!(
                        "v_bool({}_exists(ctx,{key},{}))",
                        Self::table_name(*table),
                        Self::flag(*state)
                    ),
                )
            }
            ExprKind::UniqueLookup {
                column,
                value,
                state,
                exists_only,
            } => {
                let value = self.expression(*value, env);
                let suffix = if *state == Snapshot::Pre { "_pre" } else { "" };
                let result = self.temporary(
                    "R",
                    format!("{}_lookup{suffix}(ctx,{value})", Self::column_name(*column)),
                );
                if *exists_only {
                    self.temporary("V", format!("v_bool({result}.ok)"))
                } else {
                    self.push(format!("if(!{result}.ok){{{c}.trap=1u;return false;}}"));
                    self.temporary("V", format!("{result}.value"))
                }
            }
            ExprKind::Sum { column, state } => {
                let offset = self.aggregate_offsets[column.table.0][column.index]
                    .expect("checked tracked sum");
                let state = if *state == Snapshot::Pre { "pre" } else { "s" };
                self.temporary("V", format!("{c}.{state}.aggs[{offset}]"))
            }
            ExprKind::Unary { op, operand } => {
                let operand = self.expression(*operand, env);
                match op {
                    UnaryOp::Not => self.temporary("V", format!("v_bool(!v_truth({operand}))")),
                    UnaryOp::Neg => {
                        let out = self.temporary("V", format!("v_neg({operand})"));
                        self.push(format!("if(!v_fits({out},{}u,{}) || (v_negative({operand}) && v_eq({operand},{out}) && !v_eq({operand},v_zero()))){{{c}.trap=1u;return false;}}", expression.domain.width, Self::bool_text(expression.domain.signed())));
                        out
                    }
                }
            }
            ExprKind::Binary { op, left, right } => {
                // Both operands are evaluated in source order, even for boolean
                // operators. A select is the language's lazy conditional.
                let a = self.expression(*left, env);
                let b = self.expression(*right, env);
                let signed = Self::bool_text(self.program.expr(*left).domain.signed());
                match op {
                    BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                        let operator = match op {
                            BinaryOp::And => "&&",
                            BinaryOp::Or => "||",
                            _ => "!=",
                        };
                        self.temporary("V", format!("v_bool(v_truth({a}) {operator} v_truth({b}))"))
                    }
                    BinaryOp::Eq | BinaryOp::Ne => {
                        let negate = if *op == BinaryOp::Ne { "!" } else { "" };
                        self.temporary("V", format!("v_bool({negate}v_eq({a},{b}))"))
                    }
                    BinaryOp::Lt
                    | BinaryOp::Le
                    | BinaryOp::Gt
                    | BinaryOp::Ge
                    | BinaryOp::Min
                    | BinaryOp::Max => {
                        let comparison = self.temporary("u32", format!("v_cmp({a},{b},{signed})"));
                        let condition = match op {
                            BinaryOp::Lt | BinaryOp::Max => format!("{comparison}==0u"),
                            BinaryOp::Le => format!("{comparison}!=2u"),
                            BinaryOp::Gt | BinaryOp::Min => format!("{comparison}==2u"),
                            _ => format!("{comparison}!=0u"),
                        };
                        if matches!(op, BinaryOp::Min | BinaryOp::Max) {
                            let out = self.temporary("V", &a);
                            self.push(format!("if({condition}){{{out}={b};}}"));
                            out
                        } else {
                            self.temporary("V", format!("v_bool({condition})"))
                        }
                    }
                    BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Rem => {
                        let opcode = match op {
                            BinaryOp::Add => 0,
                            BinaryOp::Sub => 1,
                            BinaryOp::Mul => 2,
                            BinaryOp::Div => 3,
                            _ => 4,
                        };
                        let scale =
                            Self::constant(&BigInt::from(10u8).pow(expression.domain.scale()));
                        let result = self.temporary(
                            "R",
                            format!(
                                "v_arith({a},{b},{opcode}u,{}u,{},{} )",
                                expression.domain.width,
                                Self::bool_text(expression.domain.signed()),
                                scale
                            ),
                        );
                        self.push(format!("if(!{result}.ok){{{c}.trap=1u;return false;}}"));
                        self.temporary("V", format!("{result}.value"))
                    }
                }
            }
            ExprKind::Fold(fold) => {
                let accumulator_type = &self.program.binding(fold.accumulator).ty;
                let index_type = &self.program.binding(fold.index).ty;
                let init = self.typed_expression(fold.init, env, accumulator_type);
                let out = self.temporary("V", init);
                let lo = self.typed_expression(fold.lo, env, index_type);
                let hi = self.typed_expression(fold.hi, env, index_type);
                let index = self.temporary("V", lo);
                let counter = self.temporary("u32", "0u");
                self.push(format!("for(;{counter}<{}u;{counter}={counter}+1u){{if(v_cmp({index},{hi},{})==2u){{break;}}", fold.limit, Self::bool_text(index_type.signed())));
                let mut inner = env.clone();
                inner.insert(fold.accumulator, out.clone());
                inner.insert(fold.index, index.clone());
                for statement in &fold.body {
                    self.statement(statement, &mut inner);
                }
                let value = self.typed_expression(fold.yield_value, &inner, accumulator_type);
                self.push(format!("{out}={value};if(v_eq({index},{})){{break;}}{index}=v_add({index},v_small(1u));}}", Self::constant(&index_type.max_value())));
                out
            }
        }
    }

    fn statement(&mut self, statement: &Stmt, env: &mut Environment) {
        let c = self.context();
        match &statement.kind {
            StmtKind::Let { binding, value } => {
                let ty = &self.program.binding(*binding).ty;
                let value = self.typed_expression(*value, env, ty);
                env.insert(*binding, value);
            }
            StmtKind::Guard { condition, .. } => {
                let value = self.expression(*condition, env);
                self.push(format!("if(!v_truth({value})){{return false;}}"));
            }
            StmtKind::TableSet { column, key, value } => {
                let key = self.expression(*key, env);
                let value = self.expression(*value, env);
                self.push(format!(
                    "if(!{}_exists(ctx,{key},false)){{{c}.trap=1u;return false;}}",
                    Self::table_name(column.table)
                ));
                self.narrow(&value, &self.program.column(*column).ty);
                self.push(format!(
                    "{}_set(ctx,{key},{value});",
                    Self::column_name(*column)
                ));
            }
            StmtKind::Allocate {
                table,
                binding,
                fields,
            } => {
                let mut arguments = BTreeMap::new();
                // The IR retains initializer source order. Reordering into
                // physical column order happens only after evaluation.
                for (column, expression) in fields {
                    let value =
                        self.typed_expression(*expression, env, &self.program.column(*column).ty);
                    arguments.insert(*column, value);
                }
                let args = self
                    .program
                    .table(*table)
                    .columns
                    .iter()
                    .map(|col| arguments[&col.id].clone())
                    .collect::<Vec<_>>();
                let suffix = if args.is_empty() {
                    String::new()
                } else {
                    format!(",{}", args.join(","))
                };
                let result = self.temporary(
                    "R",
                    format!("{}_alloc(ctx{suffix})", Self::table_name(*table)),
                );
                self.push(format!("if(!{result}.ok){{return false;}}"));
                let value = self.temporary("V", format!("{result}.value"));
                env.insert(*binding, value);
            }
            StmtKind::Emit { effect, args } => {
                let effect = self.program.effect(*effect);
                let values = args
                    .iter()
                    .zip(&effect.params)
                    .map(|(expression, parameter)| {
                        self.typed_expression(*expression, env, &parameter.ty)
                    })
                    .collect::<Vec<_>>();
                if self.record_effects {
                    let slots = self.effect_storage.expect("effect recording storage").0;
                    let stride = self.effect_stride;
                    self.push(format!(
                        "if({c}.emitted_count>={slots}u){{{c}.trap=1u;return false;}}"
                    ));
                    self.push(format!(
                        "{c}.emitted_kind[{c}.emitted_count]={}u;",
                        effect.id.0
                    ));
                    for (parameter, value) in values.iter().take(stride).enumerate() {
                        self.push(format!(
                            "{c}.emitted_args[{c}.emitted_count*{stride}u+{parameter}u]={value};"
                        ));
                    }
                    self.push(format!("{c}.emitted_count={c}.emitted_count+1u;"));
                }
                self.push(format!("{c}.s.effect_count={c}.s.effect_count+1u;{c}.s.effect_digest=v_add({c}.s.effect_digest,{});", Self::constant(&BigInt::from(symbol_id(&effect.name)))));
                for value in values {
                    self.push(format!(
                        "{c}.s.effect_digest=v_xor(v_shl({c}.s.effect_digest,1u),{value});"
                    ));
                }
            }
            StmtKind::Eval(value) => {
                self.expression(*value, env);
            }
            StmtKind::Commit => {}
        }
    }

    fn invariants(&mut self) {
        let c = self.context();
        for invariant in self.program.invariants() {
            let mut env = Environment::new();
            if let Some((table, binding)) = invariant.quantifier {
                let offset = self.table_offsets[table.0];
                let slot = self.temporary("u32", "0u");
                self.push(format!(
                    "for(;{slot}<{}u;{slot}={slot}+1u){{if({c}.s.occ[{slot}+{offset}u]!=0u){{",
                    self.program.table(table).capacity
                ));
                let key = self.temporary(
                    "V",
                    format!("{}_key(ctx,{slot},false)", Self::table_name(table)),
                );
                env.insert(binding, key);
                let value = self.expression(invariant.condition, &env);
                self.push(format!("if(!v_truth({value})){{return false;}}}}}}"));
            } else {
                let value = self.expression(invariant.condition, &env);
                self.push(format!("if(!v_truth({value})){{return false;}}"));
            }
        }
        self.push("return true;");
        self.finish_function("vera_invariants", "bool", Vec::new());
    }

    fn transition(&mut self, transition: &Transition) {
        let stride = transition_effect_stride(self.program, transition);
        self.record_effects = stride.is_some();
        self.effect_stride = stride.unwrap_or(0);
        let tn = Self::transition_name(transition);
        let apply = format!("vera_transition_{}_apply", transition.id.0);
        let post = format!("vera_transition_{}_post", transition.id.0);
        let params = transition
            .params
            .iter()
            .map(|p| (format!("arg_v_{}", p.name), "V".into()))
            .collect::<Vec<_>>();
        let base_env = transition
            .params
            .iter()
            .map(|p| (p.binding, format!("arg_v_{}", p.name)))
            .collect::<Environment>();
        let mut env = base_env.clone();
        for require in &transition.requires {
            let value = self.expression(require.value, &env);
            self.push(format!("if(!v_truth({value})){{return false;}}"));
        }
        for statement in &transition.body {
            self.statement(statement, &mut env);
        }
        self.push("return true;");
        self.finish_function(&apply, "bool", params.clone());
        for ensure in &transition.ensures {
            let value = self.expression(ensure.value, &base_env);
            self.push(format!("if(!v_truth({value})){{return false;}}"));
        }
        self.push("return vera_invariants(ctx);");
        self.finish_function(&post, "bool", params.clone());
        let mut checks = String::new();
        for parameter in &transition.params {
            checks.push_str(&format!(
                "if(!({})){{return 0u;}}",
                Self::accepts(&format!("arg_v_{}", parameter.name), &parameter.ty)
            ));
        }
        let args = params
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let args = if args.is_empty() {
            args
        } else {
            format!(",{args}")
        };
        let c = self.context();
        let reset_effects = if self.effect_storage.is_some() {
            format!("{c}.emitted_count=0u;")
        } else {
            String::new()
        };
        self.context_function(&tn, "u32", params, &format!("{c}.pre={c}.s;{c}.trap=0u;{reset_effects}{checks}if(!{apply}(ctx{args})){{{c}.s={c}.pre;if({c}.trap!=0u){{return 3u;}}return 0u;}}if(!{post}(ctx{args})){{return 2u;}}return 1u;"));
    }

    fn sample(&self, ty: &ScalarType, seed: bool, slot: &str) -> String {
        match ty {
            ScalarType::Id64(table) => format!(
                "{}_key(ctx,({slot})%{}u,false)",
                Self::table_name(*table),
                self.program.table(*table).capacity
            ),
            ScalarType::Bool => {
                if seed {
                    "v_zero()".into()
                } else {
                    "v_small(xoshiro_next(ctx)%2u)".into()
                }
            }
            ScalarType::Enum(tags) => {
                if seed {
                    "v_zero()".into()
                } else {
                    format!("v_small(xoshiro_next(ctx)%{}u)", tags.len().max(1))
                }
            }
            ScalarType::Bytes32 => "vera_sample_bytes32(ctx)".into(),
            ScalarType::Sym32 => "v_small(xoshiro_next(ctx))".into(),
            ty if ty.is_decimal() => format!(
                "w_low(w_mul(v_small((xoshiro_next(ctx)%8u)+1u),{}))",
                Self::constant(&BigInt::from(10u8).pow(ty.scale()))
            ),
            _ => {
                if seed {
                    "v_small((xoshiro_next(ctx)%100u)+1u)".into()
                } else {
                    "v_small(xoshiro_next(ctx)%100u)".into()
                }
            }
        }
    }

    fn initialize(&mut self) {
        let c = self.context();
        // Function argument evaluation order differs between C++ and shader
        // languages. Draw each limb in a statement to keep seeded furnaces
        // deterministic across targets.
        let mut random_bytes = String::new();
        for i in 0..8 {
            random_bytes.push_str(&format!("u32 limb_{i}=xoshiro_next(ctx);"));
        }
        random_bytes
            .push_str("return v_make(limb_0,limb_1,limb_2,limb_3,limb_4,limb_5,limb_6,limb_7);");
        self.context_function("vera_sample_bytes32", "V", Vec::new(), &random_bytes);
        self.context_function("vera_init", "void", Vec::new(), &format!("for(u32 i=0u;i<{}u;i=i+1u){{{c}.s.occ[i]=0u;{c}.s.gen[i]=0u;}}for(u32 i=0u;i<{}u;i=i+1u){{{c}.s.cols[i]=v_zero();}}for(u32 i=0u;i<{}u;i=i+1u){{{c}.s.aggs[i]=v_zero();}}for(u32 i=0u;i<{}u;i=i+1u){{{c}.s.hused[i]=0u;{c}.s.hslot[i]=0u;}}{c}.s.effect_count=0u;{c}.s.effect_digest=v_zero();{c}.pre={c}.s;{c}.trap=0u;", self.rows.max(1), self.columns.max(1), self.aggregates.max(1), self.hashes.max(1)));
        let mut body = String::new();
        for table in self.program.tables() {
            for i in 0..table.capacity.min(2) {
                body.push_str(&format!(
                    "{c}.s.gen[{}]=1u;",
                    self.table_offsets[table.id.0] + i as usize
                ));
            }
        }
        for table in self.program.tables() {
            for i in 0..table.capacity.min(2) {
                for column in &table.columns {
                    let value = self.sample(&column.ty, true, &format!("{i}u"));
                    let co = self.column_offsets[table.id.0][column.id.index] + i as usize;
                    body.push_str(&format!("{c}.s.cols[{co}]={value};"));
                    if let Some(ao) = self.aggregate_offsets[table.id.0][column.id.index] {
                        body.push_str(&format!(
                            "{c}.s.aggs[{ao}]=v_add({c}.s.aggs[{ao}],{c}.s.cols[{co}]);"
                        ));
                    }
                }
                body.push_str(&format!(
                    "{c}.s.occ[{}]=1u;",
                    self.table_offsets[table.id.0] + i as usize
                ));
                for column in table.columns.iter().filter(|col| col.unique) {
                    let co = self.column_offsets[table.id.0][column.id.index] + i as usize;
                    body.push_str(&format!(
                        "if(!{}_insert(ctx,{c}.s.cols[{co}],{i}u)){{return false;}}",
                        Self::column_name(column.id)
                    ));
                }
            }
        }
        body.push_str(&format!("{c}.pre={c}.s;return vera_invariants(ctx);"));
        self.context_function("vera_seed", "bool", Vec::new(), &body);
        let mut body = String::new();
        for transition in self.program.transitions() {
            body.push_str(&format!("if(which=={}u){{", transition.id.0));
            let mut args = String::new();
            for parameter in &transition.params {
                let name = format!("param_v_{}", parameter.name);
                body.push_str(&self.output.variable(
                    &name,
                    "V",
                    Some(&self.sample(&parameter.ty, false, "xoshiro_next(ctx)")),
                ));
                args.push(',');
                args.push_str(&name);
            }
            body.push_str(&format!(
                "return {}(ctx{args});}}",
                Self::transition_name(transition)
            ));
        }
        body.push_str("return 0u;");
        self.context_function(
            "vera_dispatch",
            "u32",
            vec![("which".into(), "u32".into())],
            &body,
        );
    }

    fn main(&mut self) {
        let (ptr, seed, limit) = match self.output.target {
            GpuTarget::Wgsl => {
                self.output.line("@compute @workgroup_size(32)\nfn furnace(@builtin(global_invocation_id) gid:vec3<u32>) {\nvar local_ctx:Ctx; let tid=gid.x;");
                ("&local_ctx", "12648430u", format!("{}u", self.depth))
            }
            GpuTarget::Glsl => {
                self.output
                    .line("void main(){\nCtx local_ctx;uint tid=gl_GlobalInvocationID.x;");
                ("local_ctx", "12648430u", format!("{}u", self.depth))
            }
            GpuTarget::Cuda => {
                self.output.line("extern \"C\" __global__ void vera_furnace_kernel(uint32_t* output,uint32_t seed,uint32_t depth,uint32_t count){\nuint32_t tid=blockIdx.x*blockDim.x+threadIdx.x;if(tid>=count){return;}Ctx local_ctx;");
                ("&local_ctx", "seed", "depth".into())
            }
        };
        self.output.line(self.output.syntax(&format!("for(u32 j=0u;j<4u;j=j+1u){{local_ctx.rng[j]=(tid+1u)*747796405u+(j+1u)*2891336453u+{seed};}}")));
        self.output.line(format!("vera_init({ptr});if(!vera_seed({ptr})){{vera_init({ptr});if(!vera_invariants({ptr})){{return;}}}}"));
        self.output.line(self.output.syntax(&format!("for(u32 step=0u;step<{limit};step=step+1u){{u32 before_effects=local_ctx.s.effect_count;u32 which=xoshiro_next({ptr})%{}u;u32 status=vera_dispatch({ptr},which);", self.program.transitions().len().max(1))));
        let count = |name: &str, index: usize, value: &str| match self.output.target {
            GpuTarget::Wgsl => format!("atomicAdd(&output.{name},{value});"),
            GpuTarget::Glsl => format!("atomicAdd({name},{value});"),
            GpuTarget::Cuda => format!("atomicAdd(output+{index},{value});"),
        };
        self.output.line(format!("if(status==3u){{{}{}break;}}if(status==0u){{{}}}else{{{}{}if(status==2u){{{}break;}}}}", count("violations", 0, "1u"), count("reverted", 2, "1u"), count("reverted", 2, "1u"), count("committed", 1, "1u"), count("effects", 3, "local_ctx.s.effect_count-before_effects"), count("violations", 0, "1u")));
        self.output.line("}\n}");
    }
}

#[cfg(test)]
mod tests {
    use super::{unsigned_type, GpuTarget, Output};

    #[test]
    fn wgsl_declarations_preserve_loop_bounds_arrays_and_identifiers() {
        let body =
            "for(u32 i=0u;i<4u;i=i+1u){V row=v_zero();u32 words[8];bool done;}return value_u32;";
        assert_eq!(
            Output::new(GpuTarget::Wgsl).syntax(body),
            "for(var i: u32 = 0u;i<4u;i=i+1u){var row: V = v_zero();var words: array<u32, 8>;var done: bool;}return value_u32;"
        );
    }

    #[test]
    fn unsigned_type_names_are_replaced_only_as_complete_tokens() {
        let body = "u32 words[4];value_u32=v_u32(value);u32_count=4u;";
        assert_eq!(
            unsigned_type(body, "uint"),
            "uint words[4];value_u32=v_u32(value);u32_count=4u;"
        );
        assert_eq!(
            Output::new(GpuTarget::Cuda).syntax(body),
            "uint32_t words[4];value_u32=v_u32(value);u32_count=4u;"
        );
    }
}
