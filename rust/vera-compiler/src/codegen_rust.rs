//! Standalone Rust lowering of the checked VERA program.
//!
//! Resolution and arithmetic domains are supplied by analysis. This backend
//! neither reparses source nor asks a second type checker what an expression is.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use num_bigint::{BigInt, Sign};

use crate::diagnostic::{Diagnostic, Span};
use crate::ir::*;
use crate::types::{Domain, ExprType, ScalarType, ValueType};
#[path = "codegen_rust/bytes.rs"]
mod bytes;
#[path = "codegen_rust/structured.rs"]
mod structured;
use crate::value::Value;

#[derive(Clone, Copy, Debug)]
pub struct RustOptions {
    pub use_rt: bool,
    pub include_main: bool,
}

impl Default for RustOptions {
    fn default() -> Self {
        Self {
            use_rt: false,
            include_main: true,
        }
    }
}

pub fn generate_rust(program: &CheckedProgram, options: RustOptions) -> Result<String, Diagnostic> {
    Generator::new(program).generate(options)
}

fn error(message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: "D_CODEGEN".into(),
        message: message.into(),
        span: Span::default(),
    }
}

fn ident(name: &str) -> String {
    if matches!(name, "_" | "self" | "Self" | "super" | "crate") {
        return format!("vera_{name}");
    }
    if "as break const continue else enum extern false fn for if impl in let loop match mod move mut pub ref return static struct trait true type unsafe use where while async await dyn abstract become box do final macro override priv typeof unsized virtual yield try gen".split_whitespace().any(|word| word == name) {
        format!("r#{name}")
    } else { name.into() }
}

fn variant(name: &str) -> String {
    let value = name
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<String>();
    if value.is_empty() || value == "Self" {
        format!("V_{name}")
    } else {
        value
    }
}

fn names_map(names: &[String], bases: Vec<String>, prefix: &str) -> Vec<String> {
    let mut used: HashSet<String> = bases.iter().cloned().collect();
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let base = &bases[index];
            if bases.iter().filter(|v| *v == base).count() == 1 {
                return base.clone();
            }
            let mut candidate = format!("{prefix}_{name}");
            while used.contains(&candidate) {
                candidate.push('_');
            }
            used.insert(candidate.clone());
            candidate
        })
        .collect()
}

fn quote(value: &str) -> String {
    format!("{value:?}")
}

fn scalar(ty: &ValueType) -> &ScalarType {
    ty.as_scalar().expect("checked scalar-only operation")
}

fn rust_type(ty: &ScalarType) -> &'static str {
    match ty {
        ScalarType::Bool => "bool",
        ScalarType::U8 | ScalarType::Enum(_) => "u8",
        ScalarType::U16 => "u16",
        ScalarType::U32 | ScalarType::Sym32 => "u32",
        ScalarType::U64 | ScalarType::Id64(_) => "u64",
        ScalarType::I64 | ScalarType::Dec64 => "i64",
        ScalarType::Dec128 => "i128",
        ScalarType::Bytes32 => "[u8;32]",
    }
}

fn zero(ty: &ScalarType) -> &'static str {
    match ty {
        ScalarType::Bool => "false",
        ScalarType::Bytes32 => "[0u8;32]",
        _ => "0",
    }
}

fn aggregate_type(ty: &ScalarType) -> &'static str {
    if ty.bits() > 64 {
        "I256"
    } else if ty.signed() {
        "i128"
    } else {
        "u128"
    }
}

fn aggregate_zero(ty: &ScalarType) -> &'static str {
    if ty.bits() > 64 {
        "I256::ZERO"
    } else {
        "0"
    }
}

fn aggregate_cast(value: &str, ty: &ScalarType) -> String {
    if ty.bits() > 64 {
        format!("I256::from_i128({value})")
    } else {
        format!("({value} as {})", aggregate_type(ty))
    }
}

struct Field {
    name: String,
    ty: String,
    init: String,
}

struct Generator<'a> {
    program: &'a CheckedProgram,
    out: String,
    next_temp: u64,
    table_fields: Vec<String>,
    table_constants: Vec<String>,
    transition_variants: Vec<String>,
    effect_variants: Vec<String>,
    transition_functions: Vec<String>,
    column_functions: HashMap<ColumnId, String>,
    source_codec_ids: BTreeMap<ValueType, usize>,
    error_codes: BTreeSet<String>,
    error_variants: HashMap<String, String>,
}

impl<'a> Generator<'a> {
    fn new(program: &'a CheckedProgram) -> Self {
        let source_codec_ids = bytes::collect_source_codecs(program);
        let table_names = program
            .tables
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>();
        let table_fields = names_map(
            &table_names,
            table_names
                .iter()
                .map(|name| {
                    if name.to_lowercase() == "outbox" {
                        format!("table_{name}")
                    } else {
                        ident(&name.to_lowercase())
                    }
                })
                .collect(),
            "table",
        );
        let table_constants = names_map(
            &table_names,
            table_names.iter().map(|s| s.to_uppercase()).collect(),
            "TABLE",
        );
        let transition_names = program
            .transitions
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>();
        let transition_variants = names_map(
            &transition_names,
            transition_names.iter().map(|s| variant(s)).collect(),
            "Intent",
        );
        let effect_names = program
            .effects
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>();
        let effect_variants = names_map(
            &effect_names,
            effect_names.iter().map(|s| variant(s)).collect(),
            "Effect",
        );
        let pairs = program
            .tables
            .iter()
            .flat_map(|table| {
                table
                    .columns
                    .iter()
                    .map(move |column| (column.id, format!("{}_{}", table.name, column.name)))
            })
            .collect::<Vec<_>>();
        let bases = pairs
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        let pair_names = names_map(&bases, bases.clone(), "column");
        let column_functions = pairs
            .iter()
            .zip(pair_names)
            .map(|((id, _), name)| (*id, name))
            .collect::<HashMap<_, _>>();
        let mut reserved = "main run_batch run_batch_serial restore snapshot mix64 decimal_mul decimal_div checksum replay_file atomic_write wide_product wide_quotient decimal_wide_result decimal_wide_mul decimal_wide_div projection_bound I256 U256".split_whitespace().map(str::to_owned).collect::<HashSet<_>>();
        for name in ["vera_encode_bytes", "vera_decode_bytes"] {
            reserved.insert(name.into());
        }
        for index in 0..source_codec_ids.len() {
            reserved.insert(format!("vera_source_encode_{index}"));
            reserved.insert(format!("vera_source_decode_{index}"));
        }
        for function in program.functions() {
            reserved.insert(format!("vera_function_{}", function.id.0));
        }
        for operation in program.operations() {
            reserved.insert(format!("vera_operation_{}", operation.id.0));
        }
        for ty in program.types() {
            for stem in [
                "vera_valid_type",
                "vera_encode_type",
                "vera_decode_type",
                "vera_project_type",
            ] {
                reserved.insert(format!("{stem}_{}", ty.id.0));
            }
        }
        for transition in &program.transitions {
            reserved.insert(format!("transition_{}", transition.name));
        }
        for projection in &program.projections {
            reserved.insert(format!("project_{}", projection.name));
        }
        for table in &program.tables {
            reserved.insert(format!("table_exists_{}", table.name));
            reserved.insert(format!("table_alloc_{}", table.name));
            for column in &table.columns {
                for stem in [
                    "table_get",
                    "table_set",
                    "hash_bucket",
                    "hash_lookup",
                    "hash_exists",
                    "hash_insert",
                ] {
                    reserved.insert(format!("{stem}_{}", column_functions[&column.id]));
                }
            }
        }
        let transition_functions = names_map(
            &transition_names,
            transition_names
                .iter()
                .map(|name| {
                    if reserved.contains(name) {
                        format!("vera_transition_{name}")
                    } else {
                        ident(name)
                    }
                })
                .collect(),
            "public_transition",
        );
        let mut error_codes = [
            "ArithOverflow",
            "MissingRow",
            "TableFull",
            "GenerationExhausted",
            "Precondition",
            "DuplicateKey",
            "IndexSaturated",
            "InvalidEncoding",
            "EffectAccess",
            "FunctionPrecondition",
            "OperationPrecondition",
            "VariantAccess",
            "BytesBounds",
            "BytesCapacity",
            "Decode",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        for transition in &program.transitions {
            collect_error_codes(&transition.body, &mut error_codes);
        }
        for operation in program.operations() {
            collect_error_codes(&operation.transition.body, &mut error_codes);
        }
        for expression in &program.expressions {
            if let ExprKind::Fold(fold) = &expression.kind {
                collect_error_codes(&fold.body, &mut error_codes);
            }
        }
        let error_names = error_codes.iter().cloned().collect::<Vec<_>>();
        let error_variants = error_names
            .iter()
            .cloned()
            .zip(names_map(
                &error_names,
                error_names.iter().map(|name| ident(name)).collect(),
                "error",
            ))
            .collect();
        Self {
            program,
            out: String::new(),
            next_temp: 0,
            table_fields,
            table_constants,
            transition_variants,
            effect_variants,
            transition_functions,
            column_functions,
            source_codec_ids,
            error_codes,
            error_variants,
        }
    }

    fn line(&mut self, line: impl AsRef<str>) {
        self.out.push_str(line.as_ref());
        self.out.push('\n');
    }

    fn fresh(&mut self, stem: &str) -> String {
        self.next_temp += 1;
        format!("_vera_{stem}_{}", self.next_temp)
    }

    fn field(&self, table: TableId) -> &str {
        &self.table_fields[table.0]
    }
    fn table(&self, table: TableId) -> &Table {
        &self.program.tables[table.0]
    }
    fn column(&self, column: ColumnId) -> &Column {
        &self.table(column.table).columns[column.index]
    }
    fn expr_info(&self, expression: ExprId) -> &TypedExpr {
        &self.program.expressions[expression.0]
    }
    fn binding(&self, binding: BindingId) -> String {
        format!("b_{}", binding.0)
    }

    fn generate(mut self, options: RustOptions) -> Result<String, Diagnostic> {
        self.line("// Generated by VERA. Atomic snapshots; checked arithmetic; canonical codecs.");
        self.line("#![allow(non_camel_case_types, non_snake_case, dead_code, unused_variables, unused_mut, unused_parens, unreachable_code, unused_imports, clippy::all)]");
        self.line("use std::io::Write;");
        self.constants();
        self.line("#[derive(Clone,Copy,PartialEq,Eq,Debug)] pub enum ErrorCode {");
        for code in self.error_codes.clone() {
            self.line(format!("{},", self.error_variants[&code]));
        }
        self.line("}");
        self.line(include_str!("../templates/wide.rs"));
        self.line(include_str!("../templates/codec.rs"));
        self.line(include_str!("../templates/bytes.rs"));
        self.named_types();
        self.source_codecs();
        self.effects();
        for table in &self.program.tables {
            self.emit_table(table);
        }
        self.accessors();
        self.state();
        for function in self.program.functions() {
            self.function(function)?;
        }
        for operation in self.program.operations() {
            self.operation(operation)?;
        }
        for index in 0..self.program.transitions.len() {
            self.transition(index)?;
        }
        self.runtime(options.use_rt);
        self.postconditions()?;
        self.snapshot()?;
        self.projections()?;
        if options.include_main {
            self.demo_main();
        }
        Ok(self.out)
    }

    fn constants(&mut self) {
        for (index, table) in self.program.tables.iter().enumerate() {
            let name = &self.table_constants[index];
            self.line(format!("pub const CAP_{name}:usize={};pub const KEY_BITS_{name}:u32={};pub const GEN_BITS_{name}:u32=32;", table.capacity, table.capacity.trailing_zeros()));
        }
        self.line(format!(
            "pub const STATE_SCHEMA:[u8;32]={:?};",
            crate::persistence::schema_fingerprint(self.program)
        ));
        self.line(format!(
            "pub const PROGRAM_FINGERPRINT:[u8;32]={:?};",
            crate::persistence::program_fingerprint(self.program)
        ));
    }

    fn effects(&mut self) {
        self.line("#[derive(Clone,Debug,PartialEq,Eq)] pub enum Effect {");
        for (index, effect) in self.program.effects.iter().enumerate() {
            let fields = effect
                .params
                .iter()
                .zip(parameter_fields(&effect.params))
                .map(|(p, name)| format!("{name}:{}", self.value_rust_type(&p.ty)))
                .collect::<Vec<_>>()
                .join(",");
            self.line(format!("{}{{{fields}}},", self.effect_variants[index]));
        }
        self.line("}");
    }

    fn fields(&self, table: &Table) -> Vec<Field> {
        let cap = table.capacity;
        let mut specifications = vec![
            (
                "occupied_bitmap".into(),
                format!("[u64;{}]", cap.div_ceil(64)),
                format!("[0;{}]", cap.div_ceil(64)),
                cap.div_ceil(64) * 8,
            ),
            (
                "generation".into(),
                format!("[u32;{cap}]"),
                format!("[0;{cap}]"),
                cap * 4,
            ),
        ];
        for column in &table.columns {
            specifications.push((
                format!("col_{}", column.name),
                format!("[{};{cap}]", self.value_rust_type(&column.ty)),
                format!("[{};{cap}]", self.value_zero(&column.ty)),
                cap * self
                    .program
                    .value_storage_width(&column.ty)
                    .expect("checked value layout") as u64,
            ));
        }
        for column in &table.columns {
            if column.tracked_sum {
                specifications.push((
                    format!("agg_sum_{}", column.name),
                    aggregate_type(scalar(&column.ty)).into(),
                    aggregate_zero(scalar(&column.ty)).into(),
                    if scalar(&column.ty).bits() > 64 {
                        32
                    } else {
                        16
                    },
                ));
            }
        }
        for column in &table.columns {
            if column.unique {
                let buckets = (cap * 2).max(16);
                specifications.push((
                    format!("hash_{}_key", column.name),
                    format!("[u64;{buckets}]"),
                    format!("[0;{buckets}]"),
                    buckets * 8,
                ));
                specifications.push((
                    format!("hash_{}_used", column.name),
                    format!("[u64;{}]", buckets.div_ceil(64)),
                    format!("[0;{}]", buckets.div_ceil(64)),
                    buckets.div_ceil(64) * 8,
                ));
            }
        }
        let mut fields = Vec::new();
        let mut offset = 0u64;
        for (index, (name, ty, init, size)) in specifications.into_iter().enumerate() {
            let start = offset.div_ceil(64) * 64;
            let pad = start - offset;
            if pad > 0 {
                fields.push(Field {
                    name: format!("_pad{index}"),
                    ty: format!("[u8;{pad}]"),
                    init: format!("[0;{pad}]"),
                });
            }
            fields.push(Field { name, ty, init });
            offset = start + size;
        }
        let tail = offset.div_ceil(64) * 64 - offset;
        if tail > 0 {
            fields.push(Field {
                name: "_pad_end".into(),
                ty: format!("[u8;{tail}]"),
                init: format!("[0;{tail}]"),
            });
        }
        fields
    }

    fn emit_table(&mut self, table: &Table) {
        self.line(format!(
            "#[derive(Clone,Debug,PartialEq,Eq)] #[repr(C,align(64))] pub struct Table_{}{{",
            table.name
        ));
        for field in self.fields(table) {
            self.line(format!("pub {}:{},", field.name, field.ty));
        }
        self.line("}");
    }

    fn state(&mut self) {
        self.line("#[derive(Clone,Debug,PartialEq,Eq)] pub struct State{");
        for table in &self.program.tables {
            self.line(format!(
                "pub {}:Table_{},",
                self.field(table.id),
                table.name
            ));
        }
        self.line("pub outbox:Vec<Effect>,}");
        self.line("impl Default for State{fn default()->Self{Self::new()}} impl State{pub fn new()->Self{Self{");
        for table in &self.program.tables {
            self.line(format!("{}:Table_{}{{", self.field(table.id), table.name));
            for field in self.fields(table) {
                self.line(format!("{}:{},", field.name, field.init));
            }
            self.line("},");
        }
        self.line("outbox:Vec::new(),}}}");
    }

    fn accessors(&mut self) {
        for table in &self.program.tables {
            let cap = table.capacity;
            let kb = cap.trailing_zeros();
            let tn = &table.name;
            let high = if kb + 32 < 64 {
                format!("key>>{}==0 && ", kb + 32)
            } else {
                String::new()
            };
            self.line(format!("fn table_exists_{tn}(t:&Table_{tn},key:u64)->bool{{let slot=(key&{}) as usize;{high}(t.occupied_bitmap[slot/64]>>(slot%64))&1==1 && t.generation[slot]==(key>>{kb}) as u32}}",cap-1));
            for column in &table.columns {
                let cn = &column.name;
                let suffix = self.column_functions[&column.id].clone();
                let rt = self.value_rust_type(&column.ty);
                self.line(format!("fn table_get_{suffix}(t:&Table_{tn},key:u64)->Result<{rt},ErrorCode>{{if !table_exists_{tn}(t,key){{return Err(ErrorCode::MissingRow);}}Ok(t.col_{cn}[(key&{}) as usize])}}",cap-1));
                self.line(format!("fn table_set_{suffix}(t:&mut Table_{tn},key:u64,value:{rt})->Result<(),ErrorCode>{{if !table_exists_{tn}(t,key){{return Err(ErrorCode::MissingRow);}}let slot=(key&{}) as usize;let old=t.col_{cn}[slot];",cap-1));
                if column.tracked_sum {
                    self.line(format!("t.agg_sum_{cn}=t.agg_sum_{cn}.checked_sub({}).and_then(|n|n.checked_add({})).ok_or(ErrorCode::ArithOverflow)?;",aggregate_cast("old",scalar(&column.ty)),aggregate_cast("value",scalar(&column.ty))));
                }
                self.line(format!("t.col_{cn}[slot]=value;Ok(())}}"));
                if column.unique {
                    self.hash(table, column);
                }
            }
            let params = table
                .columns
                .iter()
                .map(|c| format!("field_{}:{}", c.name, self.value_rust_type(&c.ty)))
                .collect::<Vec<_>>()
                .join(",");
            self.line(format!("fn table_alloc_{tn}(t:&mut Table_{tn}{})->Result<u64,ErrorCode>{{let slot=(0..{cap}).find(|&i|(t.occupied_bitmap[i/64]>>(i%64))&1==0).ok_or(ErrorCode::TableFull)?;let gen=t.generation[slot];if gen==u32::MAX{{return Err(ErrorCode::GenerationExhausted);}}",if params.is_empty(){String::new()}else{format!(",{params}")}));
            for column in &table.columns {
                let cn = &column.name;
                if column.tracked_sum {
                    self.line(format!("t.agg_sum_{cn}=t.agg_sum_{cn}.checked_add({}).ok_or(ErrorCode::ArithOverflow)?;",aggregate_cast(&format!("field_{cn}"),scalar(&column.ty))));
                }
                self.line(format!("t.col_{cn}[slot]=field_{cn};"));
            }
            self.line("t.generation[slot]=gen+1;t.occupied_bitmap[slot/64]|=1u64<<(slot%64);");
            for column in &table.columns {
                if column.unique {
                    self.line(format!(
                        "hash_insert_{}(t,&field_{},slot)?;",
                        self.column_functions[&column.id], column.name
                    ));
                }
            }
            self.line(format!("Ok((((gen+1) as u64)<<{kb})|slot as u64)}}"));
        }
    }

    fn hash(&mut self, table: &Table, column: &Column) {
        let tn = &table.name;
        let cn = &column.name;
        let suffix = self.column_functions[&column.id].clone();
        let rt = self.value_rust_type(&column.ty);
        let buckets = (table.capacity * 2).max(16);
        let mask = buckets - 1;
        let hash = if matches!(column.ty, ValueType::Named(_) | ValueType::Bytes { .. }) {
            format!("{{let mut out=Vec::new();{}let mut hash=14695981039346656037u64;for byte in out{{hash=(hash^u64::from(byte)).wrapping_mul(1099511628211);}}hash}}",self.value_encode("*value",&column.ty))
        } else if column.ty == ValueType::Scalar(ScalarType::Bytes32) {
            "u64::from_le_bytes(value[..8].try_into().unwrap())".into()
        } else {
            "*value as u64".into()
        };
        self.line(format!(
            "fn hash_bucket_{suffix}(value:&{rt})->usize{{(mix64({hash})&{mask}) as usize}}"
        ));
        self.line(format!("fn hash_lookup_{suffix}(t:&Table_{tn},value:&{rt})->Option<usize>{{let mut pos=hash_bucket_{suffix}(value);for _ in 0..16{{if (t.hash_{cn}_used[pos/64]>>(pos%64))&1==0{{return None;}}let slot=t.hash_{cn}_key[pos] as usize;if slot<{}&&(t.occupied_bitmap[slot/64]>>(slot%64))&1==1&&t.col_{cn}[slot]==*value{{return Some(slot);}}pos=(pos+1)&{mask};}}None}}",table.capacity));
        self.line(format!("fn hash_exists_{suffix}(t:&Table_{tn},value:&{rt})->bool{{hash_lookup_{suffix}(t,value).is_some()}}"));
        self.line(format!("fn hash_insert_{suffix}(t:&mut Table_{tn},value:&{rt},slot:usize)->Result<(),ErrorCode>{{if hash_exists_{suffix}(t,value){{return Err(ErrorCode::DuplicateKey);}}let mut incoming=slot;let mut pos=hash_bucket_{suffix}(value);let mut distance=0usize;for _ in 0..16{{if (t.hash_{cn}_used[pos/64]>>(pos%64))&1==0{{t.hash_{cn}_used[pos/64]|=1u64<<(pos%64);t.hash_{cn}_key[pos]=incoming as u64;return Ok(());}}let resident=t.hash_{cn}_key[pos] as usize;let home=hash_bucket_{suffix}(&t.col_{cn}[resident]);let resident_distance=(pos+{buckets}-home)&{mask};if resident_distance<distance{{t.hash_{cn}_key[pos]=incoming as u64;incoming=resident;distance=resident_distance;}}pos=(pos+1)&{mask};distance+=1;}}Err(ErrorCode::IndexSaturated)}}"));
    }
}

fn collect_error_codes(statements: &[Stmt], codes: &mut BTreeSet<String>) {
    for statement in statements {
        if let StmtKind::Guard { code, .. } = &statement.kind {
            codes.insert(code.clone());
        }
    }
}

type Environment = HashMap<BindingId, String>;

impl Generator<'_> {
    fn snapshot(&mut self) -> Result<(), Diagnostic> {
        self.line("impl State{pub fn encode(&self)->Vec<u8>{let mut out=Vec::new();out.extend_from_slice(b\"VERASNP2\");out.extend_from_slice(&STATE_SCHEMA);");
        for table in &self.program.tables {
            let object = format!("self.{}", self.field(table.id));
            self.line(format!("for value in &{object}.occupied_bitmap{{out.extend_from_slice(&value.to_le_bytes());}}for value in &{object}.generation{{out.extend_from_slice(&value.to_le_bytes());}}"));
            for column in &table.columns {
                self.line(format!(
                    "for value in &{object}.col_{}{{{}}}",
                    column.name,
                    self.value_encode("*value", &column.ty)
                ));
                if column.tracked_sum {
                    self.line(format!(
                        "out.extend_from_slice(&{object}.agg_sum_{}.to_le_bytes());",
                        column.name
                    ));
                }
                if column.unique {
                    self.line(format!("for value in &{object}.hash_{}_key{{out.extend_from_slice(&value.to_le_bytes());}}for value in &{object}.hash_{}_used{{out.extend_from_slice(&value.to_le_bytes());}}",column.name,column.name));
                }
            }
        }
        self.line("out.extend_from_slice(&(self.outbox.len() as u64).to_le_bytes());for effect in &self.outbox{match effect{");
        for effect in &self.program.effects {
            let names = parameter_fields(&effect.params);
            self.line(format!(
                "Effect::{}{{{}}}=>{{out.extend_from_slice(&{}u32.to_le_bytes());",
                self.effect_variants[effect.id.0],
                names.join(","),
                effect.id.0
            ));
            for (param, name) in effect.params.iter().zip(names) {
                self.line(self.value_encode(&format!("*{name}"), &param.ty));
            }
            self.line("},");
        }
        if self.program.effects.is_empty() {
            self.line("_=>unreachable!(\"uninhabited effect\"),");
        }
        self.line("}}let crc=checksum(&out);out.extend_from_slice(&crc.to_le_bytes());out}");
        self.line("pub fn decode(data:impl AsRef<[u8]>)->Result<Self,ErrorCode>{let data=data.as_ref();let end=data.len().checked_sub(4).ok_or(ErrorCode::InvalidEncoding)?;if checksum(&data[..end])!=u32::from_le_bytes(data[end..].try_into().unwrap()){return Err(ErrorCode::InvalidEncoding);}let mut r=Reader::new(&data[..end]);if r.take::<8>()?!=*b\"VERASNP2\"||r.take::<32>()?!=STATE_SCHEMA{return Err(ErrorCode::InvalidEncoding);}let mut state=Self::new();");
        for table in &self.program.tables {
            let object = format!("state.{}", self.field(table.id));
            self.line(format!("for value in &mut {object}.occupied_bitmap{{*value=u64::from_le_bytes(r.take::<8>()?);}}for value in &mut {object}.generation{{*value=u32::from_le_bytes(r.take::<4>()?);}}"));
            for column in &table.columns {
                self.line(format!(
                    "for value in &mut {object}.col_{}{{*value={};}}",
                    column.name,
                    self.value_decode(&column.ty)
                ));
                if column.tracked_sum {
                    let rt = aggregate_type(scalar(&column.ty));
                    self.line(format!(
                        "{object}.agg_sum_{}={rt}::from_le_bytes(r.take::<{}>()?);",
                        column.name,
                        if rt == "I256" { 32 } else { 16 }
                    ));
                }
                if column.unique {
                    self.line(format!("for value in &mut {object}.hash_{}_key{{*value=u64::from_le_bytes(r.take::<8>()?);}}for value in &mut {object}.hash_{}_used{{*value=u64::from_le_bytes(r.take::<8>()?);}}",column.name,column.name));
                }
            }
        }
        self.line("let count=u64::from_le_bytes(r.take::<8>()?);if count>r.data.len() as u64{return Err(ErrorCode::InvalidEncoding);}for _ in 0..count{let tag=u32::from_le_bytes(r.take::<4>()?);let effect=match tag{");
        for effect in &self.program.effects {
            self.line(format!(
                "{}=>Effect::{}{{",
                effect.id.0, self.effect_variants[effect.id.0]
            ));
            for (param, name) in effect.params.iter().zip(parameter_fields(&effect.params)) {
                self.line(format!("{name}:{},", self.value_decode(&param.ty)));
            }
            self.line("},");
        }
        self.line("_=>return Err(ErrorCode::InvalidEncoding),};state.outbox.push(effect);}r.done()?;state.validate()?;Ok(state)}");
        self.line("pub fn validate(&self)->Result<(),ErrorCode>{let state=self;let pre=self;");
        for table in &self.program.tables {
            let object = format!("self.{}", self.field(table.id));
            let capacity = table.capacity;
            if capacity % 64 != 0 {
                self.line(format!("if {object}.occupied_bitmap[{}]>>{}!=0{{return Err(ErrorCode::InvalidEncoding);}}",capacity.div_ceil(64)-1,capacity%64));
            }
            for column in &table.columns {
                let name = &column.name;
                self.line(format!("for i in 0..{capacity}{{if ({object}.occupied_bitmap[i/64]>>(i%64))&1==0&&{object}.col_{name}[i]!={}{{return Err(ErrorCode::InvalidEncoding);}}}}",self.value_zero(&column.ty)));
                let valid = self.value_validate("*value", &column.ty);
                if valid != "true" {
                    self.line(format!("if {object}.col_{name}.iter().any(|value|!({valid})){{return Err(ErrorCode::InvalidEncoding);}}"));
                }
                if column.tracked_sum {
                    self.line(format!("let mut total:{}={};for i in 0..{capacity}{{if ({object}.occupied_bitmap[i/64]>>(i%64))&1==1{{total=total.checked_add({}).ok_or(ErrorCode::InvalidEncoding)?;}}}}if total!={object}.agg_sum_{name}{{return Err(ErrorCode::InvalidEncoding);}}",aggregate_type(scalar(&column.ty)),aggregate_zero(scalar(&column.ty)),aggregate_cast(&format!("{object}.col_{name}[i]"),scalar(&column.ty))));
                }
                if column.unique {
                    let buckets = table.layout.unique_buckets[column.id.index]
                        .expect("checked unique layout");
                    let lookup = self.column_functions[&column.id].clone();
                    if buckets % 64 != 0 {
                        self.line(format!("if {object}.hash_{name}_used[{}]>>{}!=0{{return Err(ErrorCode::InvalidEncoding);}}",buckets.div_ceil(64)-1,buckets%64));
                    }
                    self.line(format!("let mut seen=[false;{capacity}];for bucket in 0..{buckets}{{let used=({object}.hash_{name}_used[bucket/64]>>(bucket%64))&1==1;let slot={object}.hash_{name}_key[bucket];if !used{{if slot!=0{{return Err(ErrorCode::InvalidEncoding);}}continue;}}if slot>={capacity}u64{{return Err(ErrorCode::InvalidEncoding);}}let slot=slot as usize;if seen[slot]||({object}.occupied_bitmap[slot/64]>>(slot%64))&1==0{{return Err(ErrorCode::InvalidEncoding);}}seen[slot]=true;if hash_lookup_{lookup}(&{object},&{object}.col_{name}[slot])!=Some(slot){{return Err(ErrorCode::InvalidEncoding);}}}}for i in 0..{capacity}{{if ({object}.occupied_bitmap[i/64]>>(i%64))&1==1&&!seen[i]{{return Err(ErrorCode::InvalidEncoding);}}}}"));
                }
            }
        }
        for invariant in self.program.invariants.clone() {
            let mut env = Environment::new();
            if let Some((table_id, binding)) = invariant.quantifier {
                let table = self.table(table_id);
                let field = self.field(table_id);
                let key = self.binding(binding);
                self.line(format!("for i in 0..{}{{if (self.{field}.occupied_bitmap[i/64]>>(i%64))&1==1{{let {key}=((self.{field}.generation[i] as u64)<<{})|i as u64;",table.capacity,table.key_bits()));
                env.insert(binding, key);
            }
            let condition = self.expr(invariant.condition, &env)?;
            self.line(format!(
                "if !({condition}){{return Err(ErrorCode::InvalidEncoding);}}"
            ));
            if invariant.quantifier.is_some() {
                self.line("}}");
            }
        }
        self.line("for effect in &self.outbox{match effect{");
        for effect in &self.program.effects {
            let names = parameter_fields(&effect.params);
            self.line(format!(
                "Effect::{}{{{}}}=>{{",
                self.effect_variants[effect.id.0],
                names.join(",")
            ));
            for (param, name) in effect.params.iter().zip(names) {
                let valid = self.value_validate(&format!("*{name}"), &param.ty);
                if valid != "true" {
                    self.line(format!(
                        "if !({valid}){{return Err(ErrorCode::InvalidEncoding);}}"
                    ));
                }
            }
            self.line("},");
        }
        if self.program.effects.is_empty() {
            self.line("_=>unreachable!(\"uninhabited effect\"),");
        }
        self.line("}}Ok(())}}");
        self.line("pub fn snapshot(state:&State,path:&str)->std::io::Result<()>{state.validate().map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidData,\"invalid VERA state\"))?;atomic_write(path,&state.encode())} pub fn restore(path:&str)->std::io::Result<State>{State::decode(std::fs::read(path)?).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidData,\"invalid VERA snapshot\"))}");
        Ok(())
    }

    fn projection_value(&mut self, id: ExprId, env: &Environment) -> Result<String, Diagnostic> {
        let domain = self.expr_info(id).domain.clone();
        let value = self.expr(id, env)?;
        Ok(self.wrap_projection_value(&value, &domain.value_type(), domain.is_aggregate()))
    }

    fn wrap_projection_value(&self, value: &str, ty: &ValueType, aggregate: bool) -> String {
        if let ValueType::Named(id) = ty {
            return format!("vera_project_type_{}({value})", id.0);
        }
        if matches!(ty, ValueType::Bytes { .. }) {
            return format!("ProjectionValue::Bytes(({value}).logical().to_vec())");
        }
        let ty = scalar(ty);
        if ty.is_bool() {
            format!("ProjectionValue::Bool({value})")
        } else if *ty == ScalarType::Bytes32 {
            format!("ProjectionValue::Bytes32({value})")
        } else if aggregate {
            format!("ProjectionValue::Integer({value})")
        } else {
            let native = if ty.signed() { "i128" } else { "u128" };
            format!("ProjectionValue::Integer(I256::from_{native}(({value}) as {native}))")
        }
    }

    fn expression_bindings(&self, id: ExprId, result: &mut HashSet<BindingId>) {
        match &self.expr_info(id).kind {
            ExprKind::BytesLen { value }
            | ExprKind::BytesResize { value, .. }
            | ExprKind::Encode { value, .. }
            | ExprKind::CanDecode { value, .. }
            | ExprKind::Decode { value, .. } => self.expression_bindings(*value, result),
            ExprKind::BytesGet { value, index } => {
                self.expression_bindings(*value, result);
                self.expression_bindings(*index, result);
            }
            ExprKind::BytesSlice {
                value,
                start,
                count,
                ..
            } => {
                for id in [value, start, count] {
                    self.expression_bindings(*id, result);
                }
            }
            ExprKind::BytesConcat { left, right, .. } => {
                self.expression_bindings(*left, result);
                self.expression_bindings(*right, result);
            }
            ExprKind::Binding(binding) => {
                result.insert(*binding);
            }
            ExprKind::Record { fields, .. } => {
                for (_, id) in fields {
                    self.expression_bindings(*id, result);
                }
            }
            ExprKind::Field { record, .. } => self.expression_bindings(*record, result),
            ExprKind::Variant { payload, .. } => self.expression_bindings(*payload, result),
            ExprKind::IsVariant { value, .. } | ExprKind::Payload { value, .. } => {
                self.expression_bindings(*value, result)
            }
            ExprKind::Call { args, .. } => {
                for id in args {
                    self.expression_bindings(*id, result);
                }
            }
            ExprKind::Unary { operand, .. } => self.expression_bindings(*operand, result),
            ExprKind::Binary { left, right, .. } => {
                self.expression_bindings(*left, result);
                self.expression_bindings(*right, result);
            }
            ExprKind::Select { condition, yes, no } => {
                for id in [condition, yes, no] {
                    self.expression_bindings(*id, result);
                }
            }
            ExprKind::TableGet { key, .. }
            | ExprKind::SlotRead { key, .. }
            | ExprKind::TableExists { key, .. } => self.expression_bindings(*key, result),
            ExprKind::UniqueLookup { value, .. } | ExprKind::KeyOf { value, .. } => {
                self.expression_bindings(*value, result)
            }
            ExprKind::Fold(fold) => {
                for id in [fold.init, fold.lo, fold.hi, fold.yield_value] {
                    self.expression_bindings(id, result);
                }
                let mut locals = vec![fold.accumulator, fold.index];
                for statement in &fold.body {
                    match &statement.kind {
                        StmtKind::Let { binding, value } => {
                            self.expression_bindings(*value, result);
                            locals.push(*binding);
                        }
                        StmtKind::Guard { condition, .. } => {
                            self.expression_bindings(*condition, result)
                        }
                        StmtKind::TableSet { key, value, .. } => {
                            self.expression_bindings(*key, result);
                            self.expression_bindings(*value, result);
                        }
                        StmtKind::Allocate {
                            binding, fields, ..
                        } => {
                            for (_, id) in fields {
                                self.expression_bindings(*id, result);
                            }
                            locals.push(*binding);
                        }
                        StmtKind::Emit { args, .. } | StmtKind::OperationCall { args, .. } => {
                            for id in args {
                                self.expression_bindings(*id, result)
                            }
                        }
                        StmtKind::Eval(id) => self.expression_bindings(*id, result),
                        StmtKind::Commit => {}
                    }
                }
                for binding in locals {
                    result.remove(&binding);
                }
            }
            ExprKind::Constant(_) | ExprKind::Sum { .. } | ExprKind::EffectCount => {}
            ExprKind::EffectIs { index, .. } | ExprKind::EffectArg { index, .. } => {
                self.expression_bindings(*index, result)
            }
        }
    }

    fn projection_bounds(
        &mut self,
        id: ExprId,
        transition: &Transition,
        env: &Environment,
    ) -> Result<(), Diagnostic> {
        let ExprKind::Binary { op, left, right } = self.expr_info(id).kind else {
            return Ok(());
        };
        if op == BinaryOp::And {
            self.projection_bounds(left, transition, env)?;
            self.projection_bounds(right, transition, env)?;
            return Ok(());
        }
        if !matches!(
            op,
            BinaryOp::Ge | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Lt | BinaryOp::Eq
        ) {
            return Ok(());
        }
        for (parameter_id, value_id, reverse) in [(left, right, false), (right, left, true)] {
            let ExprKind::Binding(binding) = self.expr_info(parameter_id).kind else {
                continue;
            };
            let Some(param) = transition.params.iter().find(|p| p.binding == binding) else {
                continue;
            };
            let mut free = HashSet::new();
            self.expression_bindings(value_id, &mut free);
            if free.contains(&binding) || free.iter().any(|id| !env.contains_key(id)) {
                continue;
            }
            let (kind, offset) = match (op, reverse) {
                (BinaryOp::Eq, _) => ("exact", 0),
                (BinaryOp::Ge, false) | (BinaryOp::Le, true) => ("min", 0),
                (BinaryOp::Gt, false) => ("min", 1),
                (BinaryOp::Lt, true) => ("min", 1),
                (BinaryOp::Le, false) | (BinaryOp::Ge, true) => ("max", 0),
                (BinaryOp::Lt, false) => ("max", -1),
                (BinaryOp::Gt, true) => ("max", -1),
                _ => unreachable!(),
            };
            let value = self.projection_value(value_id, env)?;
            self.line(format!("if let Ok(value)=(||->Result<ProjectionValue,ErrorCode>{{Ok({value})}})(){{if let Some(value)=value.offset({offset}){{projection_bound(&mut bounds,{},{},value);}}}}",quote(&param.name),quote(kind)));
            break;
        }
        Ok(())
    }

    fn projections(&mut self) -> Result<(), Diagnostic> {
        self.line(include_str!("../templates/projections.rs"));
        self.named_projection_helpers();
        for projection in self.program.projections.clone() {
            let params = projection
                .params
                .iter()
                .map(|p| {
                    format!(
                        ",{}:{}",
                        self.binding(p.binding),
                        self.value_rust_type(&p.ty)
                    )
                })
                .collect::<String>();
            let env = projection
                .params
                .iter()
                .map(|p| (p.binding, self.binding(p.binding)))
                .collect::<Environment>();
            self.line(format!("pub fn project_{}(state:&State{params})->Result<ProjectionFrame,ErrorCode>{{let pre=state;let mut frame=ProjectionFrame{{projection:{},allowed:false,reason:\"\",cells:Vec::new(),affordances:Vec::new()}};",projection.name,quote(&projection.name)));
            for param in &projection.params {
                let valid = self.value_validate(&self.binding(param.binding), &param.ty);
                if valid != "true" {
                    self.line(format!(
                        "if !({valid}){{return Err(ErrorCode::InvalidEncoding);}}"
                    ));
                }
            }
            let condition = self.expr(projection.allow_if, &env)?;
            self.line(format!("if !(||->Result<bool,ErrorCode>{{Ok({condition})}})().unwrap_or(false){{frame.reason=\"allow_if evaluated to false\";return Ok(frame);}}frame.allowed=true;"));
            for slice in &projection.expose {
                let table = self.table(slice.column.table).clone();
                let column = &table.columns[slice.column.index];
                let object = format!("state.{}", self.field(table.id));
                let value = self.wrap_projection_value(
                    &format!("{object}.col_{}[slot]", column.name),
                    &column.ty,
                    false,
                );
                let table_name = quote(&table.name);
                let column_name = quote(&column.name);
                if let Some(row) = slice.row {
                    let key = self.expr(row, &env)?;
                    self.line(format!("{{let requested={key};let slot=(requested&{}) as usize;let id=(({object}.generation[slot] as u64)<<{})|slot as u64;let live=table_exists_{}(&{object},requested);frame.cells.push(ProjectionCell{{table:{table_name},column:{column_name},requested:Some(requested),id,live,value:if live{{Some({value})}}else{{None}}}});}}",table.capacity-1,table.key_bits(),table.name));
                } else {
                    self.line(format!("for slot in 0..{}{{if ({object}.occupied_bitmap[slot/64]>>(slot%64))&1==1{{let id=(({object}.generation[slot] as u64)<<{})|slot as u64;frame.cells.push(ProjectionCell{{table:{table_name},column:{column_name},requested:None,id,live:true,value:Some({value})}});}}}}",table.capacity,table.key_bits()));
                }
            }
            for intent in &projection.enable_intents {
                let condition = self.expr(intent.condition, &env)?;
                let transition = self.program.transition(intent.transition).clone();
                self.line(format!("{{let enabled=(||->Result<bool,ErrorCode>{{Ok({condition})}})().unwrap_or(false);let mut bounds=Vec::new();if enabled{{"));
                for param in &transition.params {
                    self.line(format!(
                        "bounds.push(ParameterBound{{parameter:{},min:None,max:None,exact:None}});",
                        quote(&param.name)
                    ));
                }
                // Transition parameters and projection parameters are distinct
                // resolved bindings. Matching declared names expose known
                // projection arguments while preserving their identities.
                let mut bound_env = env.clone();
                for param in &transition.params {
                    if let Some(projection_param) = projection
                        .params
                        .iter()
                        .find(|p| p.name == param.name && p.ty == param.ty)
                    {
                        bound_env.insert(param.binding, self.binding(projection_param.binding));
                    }
                }
                for requirement in &transition.requires {
                    self.projection_bounds(requirement.value, &transition, &bound_env)?;
                }
                self.line(format!(
                    "}}frame.affordances.push(Affordance{{transition:{},enabled,bounds}});}}",
                    quote(&transition.name)
                ));
            }
            self.line("Ok(frame)}");
        }
        Ok(())
    }

    fn demo_main(&mut self) {
        self.line("fn main(){let mut initial=State::new();");
        let mut seeded = false;
        for table in &self.program.tables {
            if seeded || table.capacity < 2 {
                continue;
            }
            let Some(column) = table.columns.iter().find(|c| {
                matches!(
                    c.ty,
                    ValueType::Scalar(ScalarType::I64 | ScalarType::Dec64 | ScalarType::U64)
                )
            }) else {
                continue;
            };
            seeded = true;
            let object = format!("initial.{}", self.field(table.id));
            self.line(format!("{object}.occupied_bitmap[0]=3;{object}.generation[0]=1;{object}.generation[1]=1;{object}.col_{}[0]=500000000;{object}.col_{}[1]=300000000;",column.name,column.name));
            if column.tracked_sum {
                self.line(format!("{object}.agg_sum_{}=800000000;", column.name));
            }
        }
        self.line("if initial.validate().is_err(){initial=State::new();}initial.validate().expect(\"program requires an application-supplied valid initial state\");let mut state=initial.clone();let intents=vec![");
        for transition in self.program.transitions.iter().take(6) {
            let fields = transition
                .params
                .iter()
                .zip(parameter_fields(&transition.params))
                .map(|(param, name)| {
                    let value = match &param.ty {
                        ValueType::Scalar(ScalarType::Id64(table)) => {
                            format!("1u64<<KEY_BITS_{}", self.table_constants[table.0])
                        }
                        ValueType::Scalar(ScalarType::Bool) => "false".into(),
                        ValueType::Scalar(ScalarType::Bytes32) => "[0u8;32]".into(),
                        ValueType::Scalar(ScalarType::Enum(_)) => "0".into(),
                        ValueType::Scalar(ScalarType::U8 | ScalarType::U16) => "1".into(),
                        ValueType::Named(_) | ValueType::Bytes { .. } => self.value_zero(&param.ty),
                        _ => "100000000".into(),
                    };
                    format!("{name}:{value}")
                })
                .collect::<Vec<_>>()
                .join(",");
            self.line(format!(
                "Params::{}{{{fields}}},",
                self.transition_variants[transition.id.0]
            ));
        }
        self.line("];let mut wal=WAL::open(\"vera.wal\").unwrap();for rec in replay_file(\"vera.wal\").unwrap(){let intent=Params::decode(rec.transition,rec.args).unwrap();let _=intent.execute(&mut state);}for intent in &intents{let(id,args)=intent.encode();wal.append(id,args).unwrap();}wal.sync().unwrap();println!(\"wal tx_seq = {}\",wal.tx_seq);let results=run_batch(&mut state,&intents);println!(\"batch committed {}/{}\",results.iter().filter(|r|r.is_ok()).count(),intents.len());snapshot(&state,\"vera.snap\").unwrap();let records=replay_file(\"vera.wal\").unwrap();let mut replay=initial;for rec in records{let intent=Params::decode(rec.transition,rec.args).unwrap();let _=intent.execute(&mut replay);}assert_eq!(replay,state);println!(\"replay matches live state: deterministic\");}");
    }
}

fn parameter_fields(params: &[Param]) -> Vec<String> {
    let names = params.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    names_map(
        &names,
        names.iter().map(|name| ident(name)).collect(),
        "param",
    )
}

fn encode_scalar(value: &str, ty: &ScalarType) -> String {
    match ty {
        ScalarType::Bool => format!("out.push(({value}) as u8);"),
        ScalarType::Bytes32 => format!("out.extend_from_slice(&({value}));"),
        _ => format!("out.extend_from_slice(&({value}).to_le_bytes());"),
    }
}

fn decode_scalar(ty: &ScalarType) -> String {
    match ty {
        ScalarType::Bool => "r.boolean()?".into(),
        ScalarType::Bytes32 => "r.take::<32>()?".into(),
        _ => {
            let value = format!(
                "{}::from_le_bytes(r.take::<{}>()?)",
                rust_type(ty),
                ty.byte_width()
            );
            if let ScalarType::Enum(tags) = ty {
                format!("{{let v={value};if (v as usize)>={}{{return Err(ErrorCode::InvalidEncoding);}}v}}", tags.len())
            } else {
                value
            }
        }
    }
}

impl Generator<'_> {
    fn scalar_literal(&self, value: &Value, domain: &Domain) -> String {
        match value {
            Value::Bool(value) => value.to_string(),
            Value::Int(value) if domain.scalar == ScalarType::Bytes32 || domain.is_aggregate() => {
                let modulus = BigInt::from(1u8) << 256usize;
                let raw = if value.sign() == Sign::Minus {
                    value + modulus
                } else {
                    value.clone()
                };
                let (_, mut bytes) = raw.to_bytes_le();
                bytes.resize(32, 0);
                let array = bytes
                    .iter()
                    .map(|byte| format!("{byte}u8"))
                    .collect::<Vec<_>>()
                    .join(",");
                if domain.is_aggregate() {
                    format!("I256::from_le_bytes([{array}])")
                } else {
                    format!("[{array}]")
                }
            }
            Value::Int(value) => format!("{value}{}", rust_type(&domain.scalar)),
            Value::Record { .. } | Value::Variant { .. } | Value::Bytes(_) => {
                unreachable!("structured constant in scalar literal")
            }
        }
    }

    fn narrowed(
        &mut self,
        id: ExprId,
        env: &Environment,
        ty: &ValueType,
    ) -> Result<String, Diagnostic> {
        let value = self.expr(id, env)?;
        if self.expr_info(id).domain.is_aggregate() {
            let native = if scalar(ty).signed() { "i128" } else { "u128" };
            Ok(format!("{}::try_from(({value}).to_{native}().ok_or(ErrorCode::ArithOverflow)?).map_err(|_|ErrorCode::ArithOverflow)?", self.value_rust_type(ty)))
        } else {
            Ok(value)
        }
    }

    fn wide(&mut self, id: ExprId, env: &Environment) -> Result<String, Diagnostic> {
        let value = self.expr(id, env)?;
        let domain = &self.expr_info(id).domain;
        if domain.is_aggregate() {
            Ok(value)
        } else {
            let native = if domain
                .scalar_domain()
                .expect("checked scalar expression")
                .signed()
            {
                "i128"
            } else {
                "u128"
            };
            Ok(format!("I256::from_{native}(({value}) as {native})"))
        }
    }

    fn carrier(&self, value: String, expression_type: &ExprType) -> String {
        let Some(domain) = expression_type.scalar_domain() else {
            return value;
        };
        if domain.is_aggregate() && domain.width == 128 {
            let native = if domain.signed() { "i128" } else { "u128" };
            format!("{{let v={value};I256::from_{native}(v.to_{native}().ok_or(ErrorCode::ArithOverflow)?)}}")
        } else {
            value
        }
    }

    fn expr(&mut self, id: ExprId, env: &Environment) -> Result<String, Diagnostic> {
        let expression = self.expr_info(id).clone();
        let slot_read = matches!(expression.kind, ExprKind::SlotRead { .. });
        let domain = expression.domain;
        let promote = domain.is_aggregate()
            && matches!(
                expression.kind,
                ExprKind::Binding(_)
                    | ExprKind::TableGet { .. }
                    | ExprKind::SlotRead { .. }
                    | ExprKind::Fold(_)
                    | ExprKind::EffectArg { .. }
                    | ExprKind::EffectCount
                    | ExprKind::Call { .. }
                    | ExprKind::Field { .. }
                    | ExprKind::Payload { .. }
                    | ExprKind::BytesLen { .. }
                    | ExprKind::BytesGet { .. }
                    | ExprKind::Decode { .. }
            );
        let state_name = |snapshot| {
            if snapshot == Snapshot::Pre {
                "pre"
            } else {
                "state"
            }
        };
        let value = match expression.kind {
            ExprKind::BytesLen { .. }
            | ExprKind::BytesGet { .. }
            | ExprKind::BytesSlice { .. }
            | ExprKind::BytesConcat { .. }
            | ExprKind::BytesResize { .. }
            | ExprKind::Encode { .. }
            | ExprKind::CanDecode { .. }
            | ExprKind::Decode { .. } => self.bytes_expression(&expression.kind, env)?,
            ExprKind::Record { ty, fields } => {
                let TypeDefinitionKind::Record(definitions) =
                    &self.program.type_definition(ty).kind
                else {
                    unreachable!("checked record")
                };
                let names = self.record_field_names(ty);
                let mut prefix = String::from("{");
                let mut values = Vec::new();
                for (field, id) in fields {
                    let value = self.narrowed(id, env, &definitions[field.0].ty)?;
                    let local = self.fresh("field");
                    prefix.push_str(&format!("let {local}={value};"));
                    values.push(format!("{}:{local}", names[field.0]));
                }
                format!(
                    "{prefix}{}{{{}}}}}",
                    self.value_rust_type(&ValueType::Named(ty)),
                    values.join(",")
                )
            }
            ExprKind::Field { record, field } => {
                let ValueType::Named(ty) = self.expr_info(record).domain.value_type() else {
                    unreachable!("checked record")
                };
                let value = self.expr(record, env)?;
                format!("({value}).{}", self.record_field_names(ty)[field.0])
            }
            ExprKind::Variant {
                ty,
                variant,
                payload,
            } => {
                let TypeDefinitionKind::Union(variants) = &self.program.type_definition(ty).kind
                else {
                    unreachable!("checked union")
                };
                let value = self.narrowed(payload, env, &variants[variant.0].payload)?;
                format!(
                    "{}::{}({value})",
                    self.value_rust_type(&ValueType::Named(ty)),
                    self.union_variant_names(ty)[variant.0]
                )
            }
            ExprKind::IsVariant { value, variant } | ExprKind::Payload { value, variant } => {
                let ValueType::Named(ty) = self.expr_info(value).domain.value_type() else {
                    unreachable!("checked union")
                };
                let value = self.expr(value, env)?;
                let pattern = format!(
                    "{}::{}",
                    self.value_rust_type(&ValueType::Named(ty)),
                    self.union_variant_names(ty)[variant.0]
                );
                if matches!(expression.kind, ExprKind::IsVariant { .. }) {
                    format!("matches!({value},{pattern}(_))")
                } else {
                    format!("{{match {value}{{{pattern}(payload)=>payload,_=>return Err(ErrorCode::VariantAccess)}}}}")
                }
            }
            ExprKind::Call { function, args } => {
                let callee = self.program.function(function);
                let mut source = String::from("{");
                let mut values = Vec::new();
                for (argument, parameter) in args.iter().zip(&callee.params) {
                    let value = self.narrowed(*argument, env, &parameter.ty)?;
                    let local = self.fresh("argument");
                    source.push_str(&format!(
                        "let {local}:{}={value};",
                        self.value_rust_type(&parameter.ty)
                    ));
                    values.push(local);
                }
                source.push_str(&format!(
                    "vera_function_{}({})?}}",
                    function.0,
                    values.join(",")
                ));
                source
            }
            ExprKind::Constant(value) => self.value_literal(&value, &domain),
            ExprKind::Binding(binding) => env
                .get(&binding)
                .cloned()
                .ok_or_else(|| error(format!("unbound checked binding {}", binding.0)))?,
            ExprKind::Unary { op, operand } => {
                let value = self.expr(operand, env)?;
                match op {
                    UnaryOp::Not => format!("!({value})"),
                    UnaryOp::Neg => self.carrier(
                        format!("({value}).checked_neg().ok_or(ErrorCode::ArithOverflow)?"),
                        &domain,
                    ),
                }
            }
            ExprKind::Binary { op, left, right } => {
                let wide = self.expr_info(left).domain.is_aggregate()
                    || self.expr_info(right).domain.is_aggregate();
                let (a, b) = if wide {
                    (self.wide(left, env)?, self.wide(right, env)?)
                } else {
                    (self.expr(left, env)?, self.expr(right, env)?)
                };
                let at = self.fresh("a");
                let bt = self.fresh("b");
                let rt = if wide {
                    "I256".to_string()
                } else {
                    self.value_rust_type(&self.expr_info(left).domain.value_type())
                };
                let mut prefix = format!("{{let {at}:{rt}={a};let {bt}:{rt}={b};");
                if wide
                    && domain.scalar_domain().is_some_and(|d| d.width == 128)
                    && domain
                        .scalar_domain()
                        .expect("checked scalar expression")
                        .signed()
                    && matches!(op, BinaryOp::Div | BinaryOp::Rem)
                {
                    prefix.push_str(&format!("if {at}==I256::from_i128(i128::MIN)&&{bt}==I256::from_i128(-1){{return Err(ErrorCode::ArithOverflow);}}"));
                }
                let result = if op.comparison() || op.boolean() {
                    let symbol = match op {
                        BinaryOp::Eq => "==",
                        BinaryOp::Ne => "!=",
                        BinaryOp::Lt => "<",
                        BinaryOp::Le => "<=",
                        BinaryOp::Gt => ">",
                        BinaryOp::Ge => ">=",
                        BinaryOp::And => "&",
                        BinaryOp::Or => "|",
                        BinaryOp::Xor => "^",
                        _ => unreachable!(),
                    };
                    format!("{prefix}{at}{symbol}{bt}}}")
                } else if matches!(op, BinaryOp::Min | BinaryOp::Max) {
                    format!("{prefix}std::cmp::{}({at},{bt})}}", op.name())
                } else if domain.as_scalar().is_some_and(ScalarType::is_decimal)
                    && matches!(op, BinaryOp::Mul | BinaryOp::Div)
                {
                    let scale = 10u128.pow(
                        domain
                            .scalar_domain()
                            .expect("checked numeric expression")
                            .scale(),
                    );
                    if wide {
                        format!(
                            "{prefix}decimal_wide_{}({at},{bt},{scale}u128)?}}",
                            op.name()
                        )
                    } else {
                        format!("{prefix}{rt}::try_from(decimal_{}({at} as i128,{bt} as i128,{scale}i128)?).map_err(|_|ErrorCode::ArithOverflow)?}}", op.name())
                    }
                } else {
                    format!(
                        "{prefix}{at}.checked_{}({bt}).ok_or(ErrorCode::ArithOverflow)?}}",
                        op.name()
                    )
                };
                self.carrier(result, &domain)
            }
            ExprKind::Select { condition, yes, no } => {
                let condition = self.expr(condition, env)?;
                let (yes, no) = if domain.is_aggregate() {
                    (self.wide(yes, env)?, self.wide(no, env)?)
                } else {
                    (self.expr(yes, env)?, self.expr(no, env)?)
                };
                format!("(if {condition}{{{yes}}}else{{{no}}})")
            }
            ExprKind::TableGet { column, key, state }
            | ExprKind::SlotRead { column, key, state } => {
                let key = self.expr(key, env)?;
                let temp = self.fresh("read_key");
                let table = self.table(column.table);
                let field = self.field(table.id);
                let state = state_name(state);
                if slot_read {
                    format!(
                        "{{let {temp}={key};{state}.{field}.col_{}[({temp}&{}) as usize]}}",
                        self.column(column).name,
                        table.capacity - 1
                    )
                } else {
                    format!(
                        "{{let {temp}={key};table_get_{}(&{state}.{field},{temp})?}}",
                        self.column_functions[&column]
                    )
                }
            }
            ExprKind::TableExists { table, key, state } => {
                let key = self.expr(key, env)?;
                let temp = self.fresh("exists_key");
                format!(
                    "{{let {temp}={key};table_exists_{}(&{}.{},{temp})}}",
                    self.table(table).name,
                    state_name(state),
                    self.field(table)
                )
            }
            ExprKind::UniqueLookup {
                column,
                value,
                state,
                exists_only,
            } => {
                let ty = self.column(column).ty.clone();
                let value = self.narrowed(value, env, &ty)?;
                let temp = self.fresh("unique_value");
                let table = self.table(column.table);
                let field = self.field(table.id);
                let state = state_name(state);
                let lookup = format!(
                    "hash_lookup_{}(&{state}.{field},&{temp})",
                    self.column_functions[&column]
                );
                if exists_only {
                    format!(
                        "{{let {temp}:{}={value};{lookup}.is_some()}}",
                        self.value_rust_type(&ty)
                    )
                } else {
                    format!("{{let {temp}:{}={value};let slot={lookup}.ok_or(ErrorCode::MissingRow)?;(({state}.{field}.generation[slot] as u64)<<{})|slot as u64}}", self.value_rust_type(&ty), table.key_bits())
                }
            }
            ExprKind::Sum { column, state } => {
                let column_info = self.column(column);
                let value = format!(
                    "{}.{}.agg_sum_{}",
                    state_name(state),
                    self.field(column.table),
                    column_info.name
                );
                if scalar(&column_info.ty).bits() > 64 {
                    value
                } else {
                    format!(
                        "I256::from_{}({value})",
                        if scalar(&column_info.ty).signed() {
                            "i128"
                        } else {
                            "u128"
                        }
                    )
                }
            }
            ExprKind::KeyOf { value, .. } => {
                let aggregate = self.expr_info(value).domain.is_aggregate();
                let value = self.expr(value, env)?;
                if aggregate {
                    format!("({value}).0.0[0]")
                } else {
                    format!("(({value}) as u64)")
                }
            }
            ExprKind::EffectCount => {
                "(state.outbox.len().saturating_sub(pre.outbox.len()) as u64)".into()
            }
            ExprKind::EffectIs { effect, index } | ExprKind::EffectArg { effect, index, .. } => {
                let aggregate = self.expr_info(index).domain.is_aggregate();
                let index = self.expr(index, env)?;
                let index = if aggregate {
                    format!("({index}).to_u128().and_then(|i|usize::try_from(i).ok())")
                } else {
                    format!("usize::try_from({index}).ok()")
                };
                let record = format!("{index}.and_then(|i|pre.outbox.len().checked_add(i)).and_then(|i|state.outbox.get(i))");
                let variant = &self.effect_variants[effect.0];
                if let ExprKind::EffectArg { parameter, .. } = expression.kind {
                    let fields = parameter_fields(&self.program.effect(effect).params);
                    let field = &fields[parameter];
                    format!("{{match {record}{{Some(Effect::{variant}{{{field}:value,..}})=>*value,_=>return Err(ErrorCode::EffectAccess)}}}}")
                } else {
                    format!("matches!({record},Some(Effect::{variant}{{..}}))")
                }
            }
            ExprKind::Fold(fold) => self.fold(&fold, env)?,
        };
        Ok(if promote {
            let native = if domain
                .scalar_domain()
                .expect("checked scalar expression")
                .signed()
            {
                "i128"
            } else {
                "u128"
            };
            format!("I256::from_{native}(({value}) as {native})")
        } else {
            value
        })
    }

    fn fold(&mut self, fold: &Fold, env: &Environment) -> Result<String, Diagnostic> {
        // Fold blocks use their own output buffer; lexical bindings retain stable
        // arena IDs and never escape into the surrounding expression.
        let outer = std::mem::take(&mut self.out);
        let result = (|| {
            let acc = self.binding(fold.accumulator);
            let idx = self.binding(fold.index);
            let hi = self.fresh("hi");
            let acc_type = self.program.binding(fold.accumulator).ty.clone();
            let index_type = self.program.binding(fold.index).ty.clone();
            self.line("{");
            let init = self.narrowed(fold.init, env, &acc_type)?;
            self.line(format!(
                "let mut {acc}:{}={init};",
                self.value_rust_type(&acc_type)
            ));
            let lo = self.narrowed(fold.lo, env, &index_type)?;
            let hi_value = self.narrowed(fold.hi, env, &index_type)?;
            self.line(format!(
                "let mut {idx}:{}={lo};let {hi}:{}={hi_value};",
                self.value_rust_type(&index_type),
                self.value_rust_type(&index_type)
            ));
            let mut inner = env.clone();
            inner.insert(fold.accumulator, acc.clone());
            inner.insert(fold.index, idx.clone());
            self.line(format!(
                "for _ in 0..{}{{if {idx}>{hi}{{break;}}",
                fold.limit
            ));
            for statement in &fold.body {
                self.statement(statement, &mut inner)?;
            }
            let value = self.narrowed(fold.yield_value, &inner, &acc_type)?;
            self.line(format!(
                "{acc}={value};match {idx}.checked_add(1){{Some(next)=>{idx}=next,None=>break,}}"
            ));
            self.line("}");
            self.line(format!("{acc}}}"));
            Ok::<(), Diagnostic>(())
        })();
        let block = std::mem::replace(&mut self.out, outer);
        result?;
        Ok(block)
    }

    fn statement(&mut self, statement: &Stmt, env: &mut Environment) -> Result<(), Diagnostic> {
        match &statement.kind {
            StmtKind::Let { binding, value } => {
                let ty = self.program.binding(*binding).ty.clone();
                let value = self.narrowed(*value, env, &ty)?;
                let name = self.binding(*binding);
                self.line(format!("let {name}:{}={value};", self.value_rust_type(&ty)));
                env.insert(*binding, name);
            }
            StmtKind::Guard { condition, code } => {
                let condition = self.expr(*condition, env)?;
                self.line(format!(
                    "if !({condition}){{return Err(ErrorCode::{});}}",
                    self.error_variants[code]
                ));
            }
            StmtKind::TableSet { column, key, value } => {
                let ty = self.column(*column).ty.clone();
                let key = self.expr(*key, env)?;
                let value = self.narrowed(*value, env, &ty)?;
                let kt = self.fresh("key");
                let vt = self.fresh("value");
                self.line(format!(
                    "let {kt}={key};let {vt}={value};table_set_{}(&mut state.{},{kt},{vt})?;",
                    self.column_functions[column],
                    self.field(column.table)
                ));
            }
            StmtKind::Allocate {
                table,
                binding,
                fields,
            } => {
                let mut values = HashMap::new();
                for (column, id) in fields {
                    let ty = self.column(*column).ty.clone();
                    let value = self.narrowed(*id, env, &ty)?;
                    let temp = self.fresh("field");
                    self.line(format!("let {temp}:{}={value};", self.value_rust_type(&ty)));
                    values.insert(*column, temp);
                }
                let arguments = self
                    .table(*table)
                    .columns
                    .iter()
                    .map(|column| format!(",{}", values[&column.id]))
                    .collect::<String>();
                let name = self.binding(*binding);
                self.line(format!(
                    "let {name}=table_alloc_{}(&mut state.{}{arguments})?;",
                    self.table(*table).name,
                    self.field(*table)
                ));
                env.insert(*binding, name);
            }
            StmtKind::Emit { effect, args } => {
                let effect_info = self.program.effect(*effect).clone();
                let fields = parameter_fields(&effect_info.params);
                let mut assignments = Vec::new();
                for ((id, param), field) in args.iter().zip(&effect_info.params).zip(fields) {
                    let value = self.narrowed(*id, env, &param.ty)?;
                    let temp = self.fresh("effect");
                    self.line(format!(
                        "let {temp}:{}={value};",
                        self.value_rust_type(&param.ty)
                    ));
                    assignments.push(format!("{field}:{temp}"));
                }
                self.line(format!(
                    "state.outbox.push(Effect::{}{{{}}});",
                    self.effect_variants[effect.0],
                    assignments.join(",")
                ));
            }
            StmtKind::OperationCall {
                operation, args, ..
            } => {
                let operation = self.program.operation(*operation);
                let mut values = Vec::new();
                for (argument, parameter) in args.iter().zip(&operation.transition.params) {
                    let value = self.narrowed(*argument, env, &parameter.ty)?;
                    let temp = self.fresh("operation_arg");
                    self.line(format!(
                        "let {temp}:{}={value};",
                        self.value_rust_type(&parameter.ty)
                    ));
                    values.push(temp);
                }
                let suffix = values.iter().map(|v| format!(",{v}")).collect::<String>();
                self.line(format!(
                    "vera_operation_{}(state{suffix})?;",
                    operation.id.0
                ));
            }
            StmtKind::Eval(id) => {
                let value = self.expr(*id, env)?;
                self.line(format!("let _={value};"));
            }
            StmtKind::Commit => {}
        }
        Ok(())
    }

    fn function(&mut self, function: &Function) -> Result<(), Diagnostic> {
        let params = function
            .params
            .iter()
            .map(|p| {
                format!(
                    "{}:{}",
                    self.binding(p.binding),
                    self.value_rust_type(&p.ty)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        self.line(format!(
            "fn vera_function_{}({params})->Result<{},ErrorCode>{{",
            function.id.0,
            self.value_rust_type(&function.result_type)
        ));
        let env = function
            .params
            .iter()
            .map(|p| (p.binding, self.binding(p.binding)))
            .collect::<Environment>();
        for require in &function.requires {
            let condition = self.expr(require.value, &env)?;
            self.line(format!(
                "if !({condition}){{return Err(ErrorCode::FunctionPrecondition);}}"
            ));
        }
        let result = self.narrowed(function.body, &env, &function.result_type)?;
        self.line(format!("Ok({result})}}"));
        Ok(())
    }

    fn operation(&mut self, operation: &Operation) -> Result<(), Diagnostic> {
        let transition = &operation.transition;
        let params = transition
            .params
            .iter()
            .map(|p| {
                format!(
                    ",{}:{}",
                    self.binding(p.binding),
                    self.value_rust_type(&p.ty)
                )
            })
            .collect::<String>();
        self.line(format!("fn vera_operation_{}(state:&mut State{params})->Result<(),ErrorCode>{{let entry=state.clone();let pre=&entry;", operation.id.0));
        let mut env = transition
            .params
            .iter()
            .map(|p| (p.binding, self.binding(p.binding)))
            .collect::<Environment>();
        for requirement in &transition.requires {
            let condition = self.expr(requirement.value, &env)?;
            self.line(format!(
                "if !({condition}){{return Err(ErrorCode::OperationPrecondition);}}"
            ));
        }
        for statement in &transition.body {
            self.statement(statement, &mut env)?;
        }
        self.line("Ok(())}");
        Ok(())
    }

    fn transition(&mut self, index: usize) -> Result<(), Diagnostic> {
        let transition = self.program.transitions[index].clone();
        let params = transition
            .params
            .iter()
            .map(|p| {
                format!(
                    ",{}:{}",
                    self.binding(p.binding),
                    self.value_rust_type(&p.ty)
                )
            })
            .collect::<String>();
        let args = transition
            .params
            .iter()
            .map(|p| format!(",{}", self.binding(p.binding)))
            .collect::<String>();
        let checks = transition
            .params
            .iter()
            .filter_map(|p| {
                let valid = self.value_validate(&self.binding(p.binding), &p.ty);
                (valid != "true")
                    .then(|| format!("if !({valid}){{return Err(ErrorCode::InvalidEncoding);}}"))
            })
            .collect::<String>();
        self.line(format!("pub fn {}(state:&mut State{params})->Result<(),ErrorCode>{{{checks}let mut staged=state.clone();let outcome=transition_{}(&mut staged,state{args});if outcome.is_ok(){{*state=staged;}}outcome}}",self.transition_functions[index],transition.name));
        self.line(format!(
            "fn transition_{}(state:&mut State,pre:&State{params})->Result<(),ErrorCode>{{",
            transition.name
        ));
        let mut env = transition
            .params
            .iter()
            .map(|p| (p.binding, self.binding(p.binding)))
            .collect::<Environment>();
        for requirement in &transition.requires {
            let condition = self.expr(requirement.value, &env)?;
            self.line(format!(
                "if !({condition}){{return Err(ErrorCode::Precondition);}}"
            ));
        }
        for statement in &transition.body {
            self.statement(statement, &mut env)?;
        }
        self.line("Ok(())}");
        Ok(())
    }

    fn postconditions(&mut self) -> Result<(), Diagnostic> {
        self.line("/// A false postcondition, or a trap while evaluating it. This does not indicate whether execution was verified.");
        self.line("#[derive(Clone,Debug,PartialEq,Eq)] pub struct PostconditionFailure { pub name: &'static str, pub cause: Option<ErrorCode> }");
        self.line("impl Params { /// Evaluate this intent's named ensures against an actual committed result and its transaction-start state. Does not replay the intent, validate invariants, or alter execution admission.
 pub fn check_postconditions(&self,pre:&State,state:&State)->Result<(),PostconditionFailure>{");
        let target = if self.program.transitions.is_empty() {
            "*self"
        } else {
            "self"
        };
        self.line(format!("match {target}{{"));
        for transition in &self.program.transitions {
            let fields = parameter_fields(&transition.params);
            let names = transition
                .params
                .iter()
                .zip(&fields)
                .map(|(param, field)| format!("{field}:{}", self.binding(param.binding)))
                .collect::<Vec<_>>();
            self.line(format!(
                "Self::{}{{{}}}=>{{",
                self.transition_variants[transition.id.0],
                names.join(",")
            ));
            let env = transition
                .params
                .iter()
                .map(|param| (param.binding, format!("(*{})", self.binding(param.binding))))
                .collect::<Environment>();
            for ensure in &transition.ensures {
                let value = self.expr(ensure.value, &env)?;
                let name = quote(&ensure.name);
                self.line(format!("match (||->Result<bool,ErrorCode>{{Ok({value})}})(){{Ok(true)=>{{}},Ok(false)=>return Err(PostconditionFailure{{name:{name},cause:None}}),Err(cause)=>return Err(PostconditionFailure{{name:{name},cause:Some(cause)}})}}"));
            }
            self.line("Ok(())},");
        }
        self.line("}}}");
        Ok(())
    }

    fn runtime(&mut self, use_rt: bool) {
        self.line("#[derive(Clone,Debug,PartialEq,Eq)] pub enum Params{");
        for transition in &self.program.transitions {
            let names = parameter_fields(&transition.params);
            let fields = transition
                .params
                .iter()
                .zip(names)
                .map(|(p, name)| format!("{name}:{}", self.value_rust_type(&p.ty)))
                .collect::<Vec<_>>()
                .join(",");
            self.line(format!(
                "{}{{{fields}}},",
                self.transition_variants[transition.id.0]
            ));
        }
        self.line("}");
        let target = if self.program.transitions.is_empty() {
            "*self"
        } else {
            "self"
        };
        self.line(format!("impl Params{{pub fn execute(&self,state:&mut State)->Result<(),ErrorCode>{{match {target}{{"));
        for transition in &self.program.transitions {
            let names = parameter_fields(&transition.params);
            let args = names
                .iter()
                .map(|name| format!(",*{name}"))
                .collect::<String>();
            self.line(format!(
                "Self::{}{{{}}}=>{}(state{args}),",
                self.transition_variants[transition.id.0],
                names.join(","),
                self.transition_functions[transition.id.0]
            ));
        }
        self.line("}}");
        self.line(format!("pub fn encode(&self)->(u32,Vec<u8>){{let mut out=Vec::new();out.extend_from_slice(&PROGRAM_FINGERPRINT);let transition=match {target}{{"));
        for transition in &self.program.transitions {
            let names = parameter_fields(&transition.params);
            self.line(format!(
                "Self::{}{{{}}}=>{{",
                self.transition_variants[transition.id.0],
                names.join(",")
            ));
            for (param, name) in transition.params.iter().zip(names) {
                self.line(self.value_encode(&format!("*{name}"), &param.ty));
            }
            self.line(format!("{}u32}},", transition.id.0));
        }
        self.line("};(transition,out)}");
        self.line("pub fn decode(transition:u32,args:impl AsRef<[u8]>)->Result<Self,ErrorCode>{let mut r=Reader::new(args.as_ref());if r.take::<32>()?!=PROGRAM_FINGERPRINT{return Err(ErrorCode::InvalidEncoding);}let value=match transition{");
        for transition in &self.program.transitions {
            self.line(format!(
                "{}=>Self::{}{{",
                transition.id.0, self.transition_variants[transition.id.0]
            ));
            for (param, name) in transition
                .params
                .iter()
                .zip(parameter_fields(&transition.params))
            {
                self.line(format!("{name}:{},", self.value_decode(&param.ty)));
            }
            self.line("},");
        }
        self.line("_=>return Err(ErrorCode::InvalidEncoding),};r.done()?;Ok(value)}}");
        self.line(include_str!("../templates/batch.rs"));
        if use_rt {
            self.line("pub use ::vera_rt::{IoUringWAL as WAL,IntentRecord,replay_file,atomic_write,checksum};");
        } else {
            self.line("mod wal{");
            self.line(include_str!("../templates/wal.rs"));
            self.line("} pub use wal::{WAL,IntentRecord,replay_file,atomic_write,checksum};");
        }
    }
}
