//! Bounded logical bytes and source-visible canonical codecs.
use super::*;

pub(super) fn collect_source_codecs(program: &CheckedProgram) -> BTreeMap<ValueType, usize> {
    program
        .expressions()
        .iter()
        .filter_map(|expr| match &expr.kind {
            ExprKind::Encode { ty, .. }
            | ExprKind::CanDecode { ty, .. }
            | ExprKind::Decode { ty, .. } => Some(ty.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .enumerate()
        .map(|(i, ty)| (ty, i))
        .collect()
}
impl Generator<'_> {
    fn source_codec_index(&self, ty: &ValueType) -> usize {
        self.source_codec_ids[ty]
    }

    pub(super) fn source_codecs(&mut self) {
        for (ty, index) in self.source_codec_ids.clone() {
            let rt = self.value_rust_type(&ty);
            let width = self
                .program
                .value_wire_width(&ty)
                .expect("checked codec width");
            self.line(format!("fn vera_source_encode_{index}(value:{rt})->Result<VeraBytes<{width}>,ErrorCode>{{let mut out=Vec::new();{}VeraBytes::from_slice(&out)}}",self.value_encode("value",&ty)));
            self.line(format!("fn vera_source_decode_{index}(data:&[u8])->Result<{rt},ErrorCode>{{let mut r=Reader::new(data);let value={};r.done()?;Ok(value)}}",self.value_decode(&ty)));
        }
    }

    fn bytes_index_cast(&self, id: ExprId, value: String) -> String {
        if self.expr_info(id).domain.is_aggregate() {
            format!("u64::try_from(({value}).to_u128().ok_or(ErrorCode::BytesBounds)?).map_err(|_|ErrorCode::BytesBounds)?")
        } else {
            value
        }
    }

    pub(super) fn bytes_expression(
        &mut self,
        kind: &ExprKind,
        env: &Environment,
    ) -> Result<String, Diagnostic> {
        Ok(match kind {
            ExprKind::BytesLen { value } => format!("({}).len", self.expr(*value, env)?),
            ExprKind::BytesGet { value, index } => {
                let value = self.expr(*value, env)?;
                let raw_index = self.expr(*index, env)?;
                let index = self.bytes_index_cast(*index, raw_index);
                format!("{{let bytes={value};let index={index};bytes.get(index)?}}")
            }
            ExprKind::BytesSlice {
                value,
                start,
                count,
                capacity,
            } => {
                let value = self.expr(*value, env)?;
                let start_expr = self.expr(*start, env)?;
                let count_expr = self.expr(*count, env)?;
                let start = self.bytes_index_cast(*start, "start_raw".into());
                let count = self.bytes_index_cast(*count, "count_raw".into());
                format!("{{let bytes={value};let start_raw={start_expr};let count_raw={count_expr};let start={start};let count={count};bytes.slice::<{capacity}>(start,count)?}}")
            }
            ExprKind::BytesConcat {
                left,
                right,
                capacity,
            } => {
                let left = self.expr(*left, env)?;
                let right = self.expr(*right, env)?;
                format!(
                    "{{let left={left};let right={right};left.concat::<{capacity},_>(&right)?}}"
                )
            }
            ExprKind::BytesResize { value, capacity } => {
                format!("({}).resize::<{capacity}>()?", self.expr(*value, env)?)
            }
            ExprKind::Encode { value, ty } => {
                let value = self.narrowed(*value, env, ty)?;
                format!(
                    "vera_source_encode_{}({value})?",
                    self.source_codec_index(ty)
                )
            }
            ExprKind::CanDecode { value, ty } => format!(
                "vera_source_decode_{}(({}).logical()).is_ok()",
                self.source_codec_index(ty),
                self.expr(*value, env)?
            ),
            ExprKind::Decode { value, ty } => format!(
                "vera_source_decode_{}(({}).logical()).map_err(|_|ErrorCode::Decode)?",
                self.source_codec_index(ty),
                self.expr(*value, env)?
            ),
            _ => unreachable!("byte expression dispatch"),
        })
    }
}
