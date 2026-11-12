//! Bounded byte storage and shared canonical wire-codec lowering.
use super::*;

impl Generator<'_> {
    pub(super) fn collect_byte_types(&mut self) -> Result<(), Diagnostic> {
        let mut types = BTreeSet::new();
        for binding in self.program.bindings() {
            types.insert(binding.ty.clone());
        }
        for expression in self.program.expressions() {
            types.insert(expression.domain.value_type());
        }
        for table in self.program.tables() {
            for column in &table.columns {
                types.insert(column.ty.clone());
            }
        }
        for function in self.program.functions() {
            types.insert(function.result_type.clone());
        }
        for definition in self.program.types() {
            match &definition.kind {
                TypeDefinitionKind::Record(fields) => {
                    for field in fields {
                        types.insert(field.ty.clone());
                    }
                }
                TypeDefinitionKind::Union(variants) => {
                    for variant in variants {
                        types.insert(variant.payload.clone());
                    }
                }
            }
        }
        fn visit(
            program: &CheckedProgram,
            ty: &ValueType,
            seen: &mut BTreeSet<ValueType>,
            order: &mut Vec<ValueType>,
        ) {
            if !seen.insert(ty.clone()) {
                return;
            }
            if let ValueType::Named(id) = ty {
                match &program.type_definition(*id).kind {
                    TypeDefinitionKind::Record(fields) => {
                        for field in fields {
                            visit(program, &field.ty, seen, order);
                        }
                    }
                    TypeDefinitionKind::Union(variants) => {
                        for variant in variants {
                            visit(program, &variant.payload, seen, order);
                        }
                    }
                }
            }
            order.push(ty.clone());
        }
        let mut seen = BTreeSet::new();
        for expression in self.program.expressions() {
            if let ExprKind::Encode { ty, .. }
            | ExprKind::CanDecode { ty, .. }
            | ExprKind::Decode { ty, .. } = &expression.kind
            {
                visit(self.program, ty, &mut seen, &mut self.codec_types);
            }
        }
        for ty in &self.codec_types {
            let width = self
                .program
                .value_wire_width(ty)
                .map_err(|message| Diagnostic {
                    code: "D_GPU_RESOURCE".into(),
                    message,
                    span: Span::default(),
                })?;
            self.byte_capacities.insert(width as u64);
        }
        for ty in types {
            if let ValueType::Bytes { capacity } = ty {
                self.byte_capacities.insert(capacity);
            }
        }
        // These are actual target indexing/length representation limits, not
        // policy budgets. No fixed capacity threshold is imposed.
        if self
            .byte_capacities
            .iter()
            .any(|n| *n > u64::from(u32::MAX))
        {
            return Err(Diagnostic {
                code: "D_GPU_RESOURCE".into(),
                message: "GPU byte buffers and canonical wire slots must fit 32-bit indexing"
                    .into(),
                span: Span::default(),
            });
        }
        Ok(())
    }

    pub(super) fn byte_helpers(&mut self) {
        for n in self.byte_capacities.clone() {
            let ty = format!("VeraB{n}");
            let stem = format!("vera_bytes_{n}");
            self.output.function(&format!("{stem}_zero"), &ty, &[], &format!("{ty} value;value.len=0u;for(u32 i=0u;i<{}u;i=i+1u){{value.data[i]=0u;}}return value;", n.max(1)));
            self.output.function(
                &format!("{stem}_result"),
                &format!("VeraBR{n}"),
                &[("value".into(), ty.clone()), ("ok".into(), "bool".into())],
                &format!("VeraBR{n} result;result.value=value;result.ok=ok;return result;"),
            );
            self.output.function(&format!("{stem}_accepts"), "bool", &[("value".into(),ty.clone())], &format!("if(value.len>{n}u){{return false;}}for(u32 i=0u;i<{n}u;i=i+1u){{if(value.data[i]>255u){{return false;}}if(i>=value.len&&value.data[i]!=0u){{return false;}}}}return true;"));
            self.output.function(&format!("{stem}_eq"), "bool", &[("a".into(),ty.clone()),("b".into(),ty.clone())], "if(a.len!=b.len){return false;}for(u32 i=0u;i<a.len;i=i+1u){if(a.data[i]!=b.data[i]){return false;}}return true;");
            self.output.function(&format!("{stem}_feed"), "V", &[("input_".into(),"V".into()),("value".into(),ty.clone())], &format!("V h=input_;for(u32 i=0u;i<4u;i=i+1u){{h=vera_fnv_byte(h,(value.len>>(i*8u))&255u);}}for(u32 i=0u;i<4u;i=i+1u){{h=vera_fnv_byte(h,0u);}}for(u32 i=0u;i<{n}u;i=i+1u){{u32 b=0u;if(i<value.len){{b=value.data[i];}}h=vera_fnv_byte(h,b);}}return h;"));
            self.output.function(
                &format!("{stem}_hash"),
                "V",
                &[("value".into(), ty)],
                &format!(
                    "return vera_mix({stem}_feed({},value));",
                    Self::constant(&BigInt::from(14695981039346656037u64))
                ),
            );
        }
    }

    pub(super) fn byte_samplers(&mut self) {
        for n in self.byte_capacities.clone() {
            // Avoid n+1 overflow at the genuine u32 maximum capacity.
            let length = if n == u64::from(u32::MAX) {
                "xoshiro_next(ctx)".into()
            } else {
                format!("xoshiro_next(ctx)%{}u", n + 1)
            };
            let seed = if n == 0 {
                String::new()
            } else {
                "if(seed_){value.len=1u;value.data[0]=slot_&255u;}".into()
            };
            self.context_function(&format!("vera_bytes_{n}_sample"), &format!("VeraB{n}"), vec![("seed_".into(),"bool".into()),("slot_".into(),"u32".into())], &format!("VeraB{n} value=vera_bytes_{n}_zero();{seed}if(!seed_){{value.len={length};for(u32 i=0u;i<value.len;i=i+1u){{value.data[i]=xoshiro_next(ctx)&255u;}}}}return value;"));
        }
    }

    fn codec_name(&self, ty: &ValueType) -> String {
        format!(
            "vera_codec_{}",
            self.codec_types
                .iter()
                .position(|candidate| candidate == ty)
                .expect("collected codec schema")
        )
    }

    pub(super) fn codec_helpers(&mut self) {
        for ty in self.codec_types.clone() {
            let width = self
                .program
                .value_wire_width(&ty)
                .expect("checked wire width");
            let buffer = format!("VeraB{width}");
            let stem = self.codec_name(&ty);
            let failure = Self::result(&ty, &Self::zero(&ty), "false");
            let mut enc =
                format!("{buffer} encoded_=vera_bytes_{width}_zero();encoded_.len={width}u;");
            let mut dec=format!("if(!vera_bytes_{width}_accepts(input_)||input_.len!={width}u){{return {failure};}}{} value={};",Self::gpu_type(&ty),Self::zero(&ty));
            match &ty {
                ValueType::Scalar(scalar) => {
                    for byte in 0..width {
                        enc.push_str(&format!(
                            "encoded_.data[{byte}]=(value.w[{}]>>{}u)&255u;",
                            byte / 4,
                            8 * (byte % 4)
                        ));
                        dec.push_str(&format!(
                            "value.w[{}]=value.w[{}]|(input_.data[{byte}]<<{}u);",
                            byte / 4,
                            byte / 4,
                            8 * (byte % 4)
                        ));
                    }
                    dec.push_str(&format!(
                        "value=v_norm(value,{}u,{});if(!({})){{return {failure};}}",
                        scalar.bits(),
                        Self::bool_text(scalar.signed()),
                        Self::accepts("value", &ty)
                    ));
                }
                ValueType::Bytes { capacity } => {
                    enc.push_str("for(u32 i=0u;i<4u;i=i+1u){encoded_.data[i]=(value.len>>(8u*i))&255u;}for(u32 i=0u;i<value.len;i=i+1u){encoded_.data[8u+i]=value.data[i];}");
                    dec.push_str(&format!("for(u32 i=0u;i<4u;i=i+1u){{value.len=value.len|(input_.data[i]<<(8u*i));}}for(u32 i=4u;i<8u;i=i+1u){{if(input_.data[i]!=0u){{return {failure};}}}}if(value.len>{capacity}u){{return {failure};}}for(u32 i=0u;i<{capacity}u;i=i+1u){{if(i<value.len){{value.data[i]=input_.data[8u+i];}}else{{if(input_.data[8u+i]!=0u){{return {failure};}}}}}}"));
                }
                ValueType::Named(id) => match &self.program.type_definition(*id).kind {
                    TypeDefinitionKind::Record(fields) => {
                        let mut offset = 0;
                        for field in fields {
                            self.codec_child(
                                &field.ty,
                                &format!("value.f_{}", field.id.0),
                                offset,
                                &failure,
                                &mut enc,
                                &mut dec,
                            );
                            offset += self.program.value_wire_width(&field.ty).unwrap();
                        }
                    }
                    TypeDefinitionKind::Union(variants) => {
                        enc.push_str(
                            "for(u32 i=0u;i<4u;i=i+1u){encoded_.data[i]=(value.tag>>(8u*i))&255u;}",
                        );
                        dec.push_str(&format!("for(u32 i=0u;i<4u;i=i+1u){{value.tag=value.tag|(input_.data[i]<<(8u*i));}}if(value.tag>={}u){{return {failure};}}",variants.len()));
                        for variant in variants {
                            enc.push_str(&format!("if(value.tag=={}u){{", variant.id.0));
                            dec.push_str(&format!("if(value.tag=={}u){{", variant.id.0));
                            self.codec_child(
                                &variant.payload,
                                &format!("value.p{}", variant.id.0),
                                4,
                                &failure,
                                &mut enc,
                                &mut dec,
                            );
                            let end = 4 + self.program.value_wire_width(&variant.payload).unwrap();
                            dec.push_str(&format!("for(u32 i={end}u;i<{width}u;i=i+1u){{if(input_.data[i]!=0u){{return {failure};}}}}"));
                            enc.push('}');
                            dec.push('}');
                        }
                    }
                },
            }
            enc.push_str("return encoded_;");
            dec.push_str(&format!("return {};", Self::result(&ty, "value", "true")));
            self.output.function(
                &format!("{stem}_encode"),
                &buffer,
                &[("value".into(), Self::gpu_type(&ty))],
                &enc,
            );
            self.output.function(
                &format!("{stem}_decode"),
                &Self::result_type(&ty),
                &[("input_".into(), buffer)],
                &dec,
            );
        }
    }

    fn codec_child(
        &self,
        ty: &ValueType,
        access: &str,
        offset: usize,
        failure: &str,
        enc: &mut String,
        dec: &mut String,
    ) {
        let width = self.program.value_wire_width(ty).unwrap();
        let stem = self.codec_name(ty);
        // Braces scope local names. Shared child helpers avoid schema-DAG expansion.
        enc.push_str(&format!("{{VeraB{width} child={stem}_encode({access});for(u32 i=0u;i<{width}u;i=i+1u){{encoded_.data[{offset}u+i]=child.data[i];}}}}"));
        dec.push_str(&format!("{{VeraB{width} child=vera_bytes_{width}_zero();child.len={width}u;for(u32 i=0u;i<{width}u;i=i+1u){{child.data[i]=input_.data[{offset}u+i];}}{} decoded={stem}_decode(child);if(!decoded.ok){{return {failure};}}{access}=decoded.value;}}",Self::result_type(ty)));
    }

    pub(super) fn byte_expression(&mut self, id: ExprId, env: &Environment) -> String {
        let expression = self.program.expr(id);
        let c = self.context();
        let failure = self.failure_return();
        let trap = format!("{c}.trap=1u;{failure}");
        match &expression.kind {
            ExprKind::BytesLen { value } => {
                let value = self.expression(*value, env);
                self.temporary("V", format!("v_small({value}.len)"))
            }
            ExprKind::BytesGet { value, index } => {
                let value = self.expression(*value, env);
                let index = self.expression(*index, env);
                self.push(format!(
                    "if(v_ucmp({index},v_small({value}.len))!=0u){{{trap}}}"
                ));
                self.temporary("V", format!("v_small({value}.data[{index}.w[0]])"))
            }
            ExprKind::BytesSlice {
                value,
                start,
                count,
                capacity,
            } => {
                let value = self.expression(*value, env);
                let start = self.expression(*start, env);
                let count = self.expression(*count, env);
                self.push(format!("if(v_ucmp({start},v_small({value}.len))==2u){{{trap}}}if(v_ucmp({count},v_small({value}.len-{start}.w[0]))==2u||v_ucmp({count},v_small({capacity}u))==2u){{{trap}}}"));
                let encoded_ = self.temporary(
                    &format!("VeraB{capacity}"),
                    format!("vera_bytes_{capacity}_zero()"),
                );
                self.push(format!("{encoded_}.len={count}.w[0];for(u32 i=0u;i<{encoded_}.len;i=i+1u){{{encoded_}.data[i]={value}.data[{start}.w[0]+i];}}"));
                encoded_
            }
            ExprKind::BytesConcat {
                left,
                right,
                capacity,
            } => {
                let left = self.expression(*left, env);
                let right = self.expression(*right, env);
                self.push(format!("if({left}.len>{capacity}u){{{trap}}}if({right}.len>{capacity}u-{left}.len){{{trap}}}"));
                let encoded_ = self.temporary(
                    &format!("VeraB{capacity}"),
                    format!("vera_bytes_{capacity}_zero()"),
                );
                self.push(format!("{encoded_}.len={left}.len+{right}.len;for(u32 i=0u;i<{left}.len;i=i+1u){{{encoded_}.data[i]={left}.data[i];}}for(u32 i=0u;i<{right}.len;i=i+1u){{{encoded_}.data[{left}.len+i]={right}.data[i];}}"));
                encoded_
            }
            ExprKind::BytesResize { value, capacity } => {
                let value = self.expression(*value, env);
                self.push(format!("if({value}.len>{capacity}u){{{trap}}}"));
                let encoded_ = self.temporary(
                    &format!("VeraB{capacity}"),
                    format!("vera_bytes_{capacity}_zero()"),
                );
                self.push(format!("{encoded_}.len={value}.len;for(u32 i=0u;i<{encoded_}.len;i=i+1u){{{encoded_}.data[i]={value}.data[i];}}"));
                encoded_
            }
            ExprKind::Encode { value, ty } => {
                let value = self.typed_expression(*value, env, ty);
                let stem = self.codec_name(ty);
                let result = Self::gpu_type(&expression.domain.value_type());
                self.temporary(&result, format!("{stem}_encode({value})"))
            }
            ExprKind::Decode { value, ty } | ExprKind::CanDecode { value, ty } => {
                let value = self.expression(*value, env);
                let width = self.program.value_wire_width(ty).unwrap();
                let stem = self.codec_name(ty);
                let result = self.temporary(
                    &Self::result_type(ty),
                    Self::result(ty, &Self::zero(ty), "false"),
                );
                self.push(format!("if({value}.len=={width}u){{"));
                let buffer = self.temporary(
                    &format!("VeraB{width}"),
                    format!("vera_bytes_{width}_zero()"),
                );
                self.push(format!("{buffer}.len={width}u;for(u32 i=0u;i<{width}u;i=i+1u){{{buffer}.data[i]={value}.data[i];}}{result}={stem}_decode({buffer});}}"));
                if matches!(expression.kind, ExprKind::CanDecode { .. }) {
                    self.temporary("V", format!("v_bool({result}.ok)"))
                } else {
                    self.push(format!("if(!{result}.ok){{{trap}}}"));
                    self.temporary(&Self::gpu_type(ty), format!("{result}.value"))
                }
            }
            _ => unreachable!(),
        }
    }
}
