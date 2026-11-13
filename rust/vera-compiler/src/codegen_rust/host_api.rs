//! Stable, checked host entrypoints for pure functions. Metadata describes an
//! interface, not whether it was proved or whether a host consumes its results.
use super::*;
use serde_json::{json, Value as JsonValue};

pub fn rust_api_manifest(program: &CheckedProgram) -> JsonValue {
    let generator = Generator::new(program);
    json!({
        "schema_version": 1,
        "program_fingerprint": crate::persistence::program_fingerprint(program).iter().map(|byte|format!("{byte:02x}")).collect::<String>(),
        "target": "rust",
        "scope": "pure_functions",
        "verification": "not_asserted_by_interface_metadata",
        "functions": program.functions().iter().map(|function| json!({
            "name": function.name,
            "rust_path": format!("functions::call_{}", function.name),
            "parameters": function.params.iter().enumerate().map(|(index, parameter)| json!({
                "name": parameter.name,
                "rust_name": format!("arg{index}"),
                "vera_type": program.value_type_name(&parameter.ty),
                "rust_type": generator.value_rust_type(&parameter.ty),
                "representation": representation(program, &parameter.ty),
            })).collect::<Vec<_>>(),
            "result": {
                "vera_type": program.value_type_name(&function.result_type),
                "rust_type": generator.value_rust_type(&function.result_type),
                "representation": representation(program, &function.result_type),
            },
            "error_type": "ErrorCode",
            "host_domain_validation": true,
        })).collect::<Vec<_>>(),
        "named_types": program.types().iter().map(|definition| {
            let mut metadata = json!({"name": definition.name, "rust_type": generator.value_rust_type(&ValueType::Named(definition.id))});
            match &definition.kind {
                TypeDefinitionKind::Record(fields) => {
                    metadata["kind"] = json!("record");
                    metadata["fields"] = json!(fields.iter().zip(generator.record_field_names(definition.id)).map(|(field, rust_name)| json!({"name":field.name,"rust_name":rust_name,"vera_type":program.value_type_name(&field.ty),"rust_type":generator.value_rust_type(&field.ty),"representation":representation(program,&field.ty)})).collect::<Vec<_>>());
                }
                TypeDefinitionKind::Union(variants) => {
                    metadata["kind"] = json!("union");
                    metadata["variants"] = json!(variants.iter().zip(generator.union_variant_names(definition.id)).enumerate().map(|(tag,(variant,rust_name))| json!({"name":variant.name,"rust_name":rust_name,"tag":tag,"vera_type":program.value_type_name(&variant.payload),"rust_type":generator.value_rust_type(&variant.payload),"representation":representation(program,&variant.payload)})).collect::<Vec<_>>());
                }
            }
            metadata
        }).collect::<Vec<_>>(),
        "notes": [
            "Paths are relative to the generated Rust module.",
            "Invalid host value representations return InvalidEncoding before function evaluation.",
            "Requires are checked at runtime; ensures require proof admission and are not rechecked by these wrappers.",
            "Decimals use scaled integer carriers; enums use declaration-order u8 tags; typed handles use u64 carriers.",
            "A successful call does not establish that its result affects host behavior."
        ],
    })
}

fn representation(program: &CheckedProgram, ty: &ValueType) -> JsonValue {
    match ty {
        ValueType::Bytes { capacity } => {
            json!({"kind":"bounded_bytes", "capacity":capacity, "canonical_zero_padding":true})
        }
        ValueType::Named(id) => {
            json!({"kind":"named", "definition":program.type_definition(*id).name})
        }
        ValueType::Scalar(ScalarType::Enum(tags)) => {
            json!({"kind":"enum", "tags":tags.iter().enumerate().map(|(value,name)|json!({"name":name,"value":value})).collect::<Vec<_>>()})
        }
        ValueType::Scalar(ScalarType::Id64(table)) => {
            json!({"kind":"typed_handle", "table":program.table(*table).name})
        }
        ValueType::Scalar(ty) if ty.is_decimal() => {
            json!({"kind":"scaled_integer", "decimal_places":ty.scale(), "scale_factor":10u64.pow(ty.scale())})
        }
        ValueType::Scalar(_) => json!({"kind":"native_scalar"}),
    }
}

impl Generator<'_> {
    pub(super) fn host_api(&mut self) {
        self.line(format!(
            "pub const VERA_HOST_API_JSON:&str={};",
            quote(&rust_api_manifest(self.program).to_string())
        ));
        self.line("/// Stable pure-function entrypoints. Consume successful values in observable host behavior.");
        self.line("pub mod functions { use super::*;");
        for function in self.program.functions() {
            let params = function
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| format!("arg{i}:{}", self.value_rust_type(&p.ty)))
                .collect::<Vec<_>>()
                .join(",");
            self.line("#[must_use = \"Handle errors and consume the verified result in the host behavior; calling alone is not integration.\"]");
            self.line(format!(
                "pub fn call_{}({params})->Result<{},ErrorCode>{{",
                function.name,
                self.value_rust_type(&function.result_type)
            ));
            for (i, parameter) in function.params.iter().enumerate() {
                let valid = self.value_validate(&format!("arg{i}"), &parameter.ty);
                if valid != "true" {
                    self.line(format!(
                        "if !({valid}){{return Err(ErrorCode::InvalidEncoding);}}"
                    ));
                }
            }
            let args = (0..function.params.len())
                .map(|i| format!("arg{i}"))
                .collect::<Vec<_>>()
                .join(",");
            self.line(format!("super::vera_function_{}({args})}}", function.id.0));
        }
        self.line("}");
    }
}
