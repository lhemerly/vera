//! Nominal value lowering and canonical structured codecs. Native representation
//! and wire representation are deliberately separate contracts.
use super::*;

impl Generator<'_> {
    pub(super) fn value_rust_type(&self, ty: &ValueType) -> String {
        match ty {
            ValueType::Scalar(ty) => rust_type(ty).into(),
            ValueType::Named(id) => format!("VeraValue_{}", self.program.type_definition(*id).name),
        }
    }

    pub(super) fn value_zero(&self, ty: &ValueType) -> String {
        match ty {
            ValueType::Scalar(ty) => zero(ty).into(),
            ValueType::Named(_) => format!("{}::zero()", self.value_rust_type(ty)),
        }
    }

    pub(super) fn value_validate(&self, value: &str, ty: &ValueType) -> String {
        match ty {
            ValueType::Scalar(ScalarType::Enum(tags)) => {
                format!("(({value}) as usize)<{}", tags.len())
            }
            ValueType::Scalar(_) => "true".into(),
            ValueType::Named(id) => format!("vera_valid_type_{}(&({value}))", id.0),
        }
    }

    pub(super) fn value_encode(&self, value: &str, ty: &ValueType) -> String {
        match ty {
            ValueType::Scalar(ty) => encode_scalar(value, ty),
            ValueType::Named(id) => format!("vera_encode_type_{}(&({value}),&mut out);", id.0),
        }
    }

    pub(super) fn value_decode(&self, ty: &ValueType) -> String {
        match ty {
            ValueType::Scalar(ty) => decode_scalar(ty),
            ValueType::Named(id) => format!("vera_decode_type_{}(&mut r)?", id.0),
        }
    }

    pub(super) fn record_field_names(&self, id: TypeId) -> Vec<String> {
        let TypeDefinitionKind::Record(fields) = &self.program.type_definition(id).kind else {
            unreachable!("checked record type")
        };
        let names = fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        names_map(
            &names,
            names.iter().map(|name| ident(name)).collect(),
            "field",
        )
    }

    pub(super) fn union_variant_names(&self, id: TypeId) -> Vec<String> {
        let TypeDefinitionKind::Union(variants) = &self.program.type_definition(id).kind else {
            unreachable!("checked union type")
        };
        let names = variants.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
        names_map(
            &names,
            names.iter().map(|name| variant(name)).collect(),
            "variant",
        )
    }

    pub(super) fn value_literal(&self, value: &Value, ty: &ExprType) -> String {
        match value {
            Value::Record { ty, fields } => {
                let TypeDefinitionKind::Record(definitions) =
                    &self.program.type_definition(*ty).kind
                else {
                    unreachable!()
                };
                let names = self.record_field_names(*ty);
                let members = fields
                    .iter()
                    .zip(definitions)
                    .zip(names)
                    .map(|((value, field), name)| {
                        format!(
                            "{name}:{}",
                            self.value_literal(
                                value,
                                &match &field.ty {
                                    ValueType::Scalar(s) =>
                                        ExprType::Scalar(Domain::scalar(s.clone())),
                                    ValueType::Named(t) => ExprType::Named(*t),
                                }
                            )
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{}{{{members}}}",
                    self.value_rust_type(&ValueType::Named(*ty))
                )
            }
            Value::Variant { ty, tag, payload } => {
                let TypeDefinitionKind::Union(variants) = &self.program.type_definition(*ty).kind
                else {
                    unreachable!()
                };
                let payload_type = match &variants[tag.0].payload {
                    ValueType::Scalar(s) => ExprType::Scalar(Domain::scalar(s.clone())),
                    ValueType::Named(t) => ExprType::Named(*t),
                };
                format!(
                    "{}::{}({})",
                    self.value_rust_type(&ValueType::Named(*ty)),
                    self.union_variant_names(*ty)[tag.0],
                    self.value_literal(payload, &payload_type)
                )
            }
            _ => self.scalar_literal(value, ty.scalar_domain().expect("scalar constant domain")),
        }
    }

    pub(super) fn named_types(&mut self) {
        for definition in self.program.types() {
            let ty = ValueType::Named(definition.id);
            let name = self.value_rust_type(&ty);
            match &definition.kind {
                TypeDefinitionKind::Record(fields) => {
                    let names = self.record_field_names(definition.id);
                    self.line(format!(
                        "#[derive(Clone,Copy,Debug,PartialEq,Eq)] #[repr(C)] pub struct {name}{{"
                    ));
                    for (field, field_name) in fields.iter().zip(&names) {
                        self.line(format!(
                            "pub {field_name}:{},",
                            self.value_rust_type(&field.ty)
                        ));
                    }
                    self.line("}");
                    self.line(format!("impl {name}{{pub const fn zero()->Self{{Self{{"));
                    for (field, field_name) in fields.iter().zip(&names) {
                        self.line(format!("{field_name}:{},", self.value_zero(&field.ty)));
                    }
                    self.line("}}}");
                    self.line(format!(
                        "fn vera_valid_type_{}(value:&{name})->bool{{",
                        definition.id.0
                    ));
                    self.line(
                        fields
                            .iter()
                            .zip(&names)
                            .map(|(field, field_name)| {
                                self.value_validate(&format!("value.{field_name}"), &field.ty)
                            })
                            .collect::<Vec<_>>()
                            .join(" && "),
                    );
                    if fields.is_empty() {
                        self.line("true");
                    }
                    self.line("}");
                    self.line(format!(
                        "fn vera_encode_type_{}(value:&{name},mut out:&mut Vec<u8>){{",
                        definition.id.0
                    ));
                    for (field, field_name) in fields.iter().zip(&names) {
                        self.line(self.value_encode(&format!("value.{field_name}"), &field.ty));
                    }
                    self.line("}");
                    self.line(format!("fn vera_decode_type_{}(mut r:&mut Reader<'_>)->Result<{name},ErrorCode>{{Ok({name}{{",definition.id.0));
                    for (field, field_name) in fields.iter().zip(&names) {
                        self.line(format!("{field_name}:{},", self.value_decode(&field.ty)));
                    }
                    self.line("})}");
                }
                TypeDefinitionKind::Union(variants) => {
                    let names = self.union_variant_names(definition.id);
                    self.line(format!(
                        "#[derive(Clone,Copy,Debug,PartialEq,Eq)] #[repr(C,u32)] pub enum {name}{{"
                    ));
                    for (variant, variant_name) in variants.iter().zip(&names) {
                        self.line(format!(
                            "{variant_name}({}),",
                            self.value_rust_type(&variant.payload)
                        ));
                    }
                    self.line("}");
                    self.line(format!(
                        "impl {name}{{pub const fn zero()->Self{{Self::{}({})}}}}",
                        names[0],
                        self.value_zero(&variants[0].payload)
                    ));
                    self.line(format!(
                        "fn vera_valid_type_{}(value:&{name})->bool{{match value{{",
                        definition.id.0
                    ));
                    for (variant, variant_name) in variants.iter().zip(&names) {
                        self.line(format!(
                            "{name}::{variant_name}(payload)=>{},",
                            self.value_validate("*payload", &variant.payload)
                        ));
                    }
                    self.line("}}");
                    let total = self
                        .program
                        .value_wire_width(&ty)
                        .expect("checked wire layout");
                    self.line(format!(
                        "fn vera_encode_type_{}(value:&{name},mut out:&mut Vec<u8>){{match value{{",
                        definition.id.0
                    ));
                    for (variant, variant_name) in variants.iter().zip(&names) {
                        let pad = total
                            - 4
                            - self
                                .program
                                .value_wire_width(&variant.payload)
                                .expect("checked payload layout");
                        self.line(format!("{name}::{variant_name}(payload)=>{{out.extend_from_slice(&{}u32.to_le_bytes());{}out.resize(out.len()+{pad},0);}},",variant.id.0,self.value_encode("*payload",&variant.payload)));
                    }
                    self.line("}}");
                    self.line(format!("fn vera_decode_type_{}(mut r:&mut Reader<'_>)->Result<{name},ErrorCode>{{let tag=u32::from_le_bytes(r.take::<4>()?);match tag{{",definition.id.0));
                    for (variant, variant_name) in variants.iter().zip(&names) {
                        let pad = total
                            - 4
                            - self
                                .program
                                .value_wire_width(&variant.payload)
                                .expect("checked payload layout");
                        self.line(format!("{}=>{{let payload={};if r.take::<{pad}>()?.iter().any(|&byte|byte!=0){{return Err(ErrorCode::InvalidEncoding);}}Ok({name}::{variant_name}(payload))}},",variant.id.0,self.value_decode(&variant.payload)));
                    }
                    self.line("_=>Err(ErrorCode::InvalidEncoding),}} ");
                }
            }
        }
    }

    pub(super) fn named_projection_helpers(&mut self) {
        for definition in self.program.types() {
            let name = self.value_rust_type(&ValueType::Named(definition.id));
            self.line(format!(
                "fn vera_project_type_{}(value:{name})->ProjectionValue{{",
                definition.id.0
            ));
            match &definition.kind {
                TypeDefinitionKind::Record(fields) => {
                    let names = self.record_field_names(definition.id);
                    self.line(format!(
                        "ProjectionValue::Record{{ty:{},fields:vec![",
                        quote(&definition.name)
                    ));
                    for (field, name) in fields.iter().zip(&names) {
                        self.line(format!(
                            "({},{}),",
                            quote(&field.name),
                            self.wrap_projection_value(&format!("value.{name}"), &field.ty, false)
                        ));
                    }
                    self.line("]}");
                }
                TypeDefinitionKind::Union(variants) => {
                    let names = self.union_variant_names(definition.id);
                    self.line("match value{");
                    for (variant, variant_name) in variants.iter().zip(&names) {
                        self.line(format!("{name}::{variant_name}(payload)=>ProjectionValue::Variant{{ty:{},tag:{},payload:Box::new({})}},",quote(&definition.name),quote(&variant.name),self.wrap_projection_value("payload",&variant.payload,false)));
                    }
                    self.line("}");
                }
            }
            self.line("}");
        }
    }
}
