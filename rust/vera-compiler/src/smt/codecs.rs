//! Definitional canonical codecs over byte sequences; no round-trip axioms.
use super::*;

pub(super) fn empty() -> String {
    "(as seq.empty (Seq (_ BitVec 8)))".into()
}
pub(super) fn concat(parts: Vec<String>) -> String {
    match parts.len() {
        0 => empty(),
        1 => parts[0].clone(),
        _ => format!("(seq.++ {})", parts.join(" ")),
    }
}
fn extract(value: &str, start: impl std::fmt::Display, count: impl std::fmt::Display) -> String {
    format!("(seq.extract {value} {start} {count})")
}
fn encode_bits(value: &str, width: u32) -> String {
    concat(
        (0..width / 8)
            .map(|i| format!("(seq.unit ((_ extract {} {}) {value}))", i * 8 + 7, i * 8))
            .collect(),
    )
}
fn decode_bits(value: &str, width: u32) -> String {
    let mut out = format!("(seq.nth {value} {})", width / 8 - 1);
    for i in (0..width / 8 - 1).rev() {
        out = format!("(concat {out} (seq.nth {value} {i}))");
    }
    out
}

impl Encoding<'_> {
    /// A zero sequence is uniquely defined by its length and the language
    /// (00)*. Constrain only the padding lengths used by valid encoders, rather
    /// than materializing a capacity-sized object in every solver model.
    fn zero_bytes(&mut self, count: impl std::fmt::Display) -> String {
        if !self.zero_padding_declared {
            self.script
                .declaration("(declare-fun zero_pad (Int) (Seq (_ BitVec 8)))".into());
            self.script.declaration(format!(
                "(define-fun zero_pad_valid ((n Int)) Bool (and (= (seq.len (zero_pad n)) n) {}))",
                all_zero("(zero_pad n)")
            ));
            self.zero_padding_declared = true;
        }
        format!("(zero_pad {count})")
    }
    /// Ground the exact padding relation for every used schema application.
    /// No decoder validity or round-trip property is assumed here.
    pub(super) fn encoder_padding_facts(&mut self, ty: &ValueType, value: &str) -> LowerResult<()> {
        let id = self.encoder_padding_helper(ty)?;
        self.script.assert(implies(
            &valid_value(ty, value),
            &format!("(padding_parts_{id} {value})"),
        ));
        Ok(())
    }
    fn encoder_padding_helper(&mut self, ty: &ValueType) -> LowerResult<usize> {
        let id = self.encoder(ty)?;
        if self.codec_padding_helpers.contains(&id) {
            return Ok(id);
        }
        let wire = format!("(encode_{id} x)");
        let width = self.program.value_wire_width(ty).expect("checked width");
        // Constructor-boundary consequences keep the sequence solver from
        // rediscovering concatenation/extraction lengths by capacity casework.
        let mut facts = vec![format!("(= (seq.len {wire}) {width})")];
        match ty {
            ValueType::Bytes { capacity } => {
                facts.push(format!("(zero_pad_valid (- {capacity} (seq.len x)))"));
                facts.push(eq(
                    &format!("(ubv_to_int {})", decode_bits(&wire, 64)),
                    "(seq.len x)",
                ));
                facts.push(eq(&extract(&wire, 8, "(seq.len x)"), "x"));
                facts.push(all_zero(&extract(
                    &wire,
                    "(+ 8 (seq.len x))",
                    format!("(- {capacity} (seq.len x))"),
                )));
            }
            ValueType::Named(ty_id) => match &self.program.type_definition(*ty_id).kind.clone() {
                TypeDefinitionKind::Record(fields) => {
                    let mut offset = 0;
                    for field in fields {
                        let child = self.encoder_padding_helper(&field.ty)?;
                        facts.push(format!(
                            "(padding_parts_{child} ({} x))",
                            selector(*ty_id, field.id.0)
                        ));
                        let value = format!("({} x)", selector(*ty_id, field.id.0));
                        let child_width = self
                            .program
                            .value_wire_width(&field.ty)
                            .expect("checked width");
                        facts.push(eq(
                            &extract(&wire, offset, child_width),
                            &format!("(encode_{child} {value})"),
                        ));
                        offset += child_width;
                    }
                }
                TypeDefinitionKind::Union(variants) => {
                    let width = self.program.value_wire_width(ty).expect("checked width");
                    for variant in variants {
                        let child = self.encoder_padding_helper(&variant.payload)?;
                        let child_width = self
                            .program
                            .value_wire_width(&variant.payload)
                            .expect("checked width");
                        let padding = width - 4 - child_width;
                        let mut branch = vec![format!(
                            "(padding_parts_{child} ({} x))",
                            payload_selector(*ty_id, variant.id)
                        )];
                        let value = format!("({} x)", payload_selector(*ty_id, variant.id));
                        branch.push(eq(&decode_bits(&wire, 32), &bv(variant.id.0, 32)));
                        branch.push(eq(
                            &extract(&wire, 4, child_width),
                            &format!("(encode_{child} {value})"),
                        ));
                        branch.push(all_zero(&extract(&wire, 4 + child_width, padding)));
                        if padding > 0 {
                            branch.push(format!("(zero_pad_valid {padding})"));
                        }
                        facts.push(implies(
                            &format!("((_ is {}) x)", constructor(*ty_id, Some(variant.id))),
                            &and(branch),
                        ));
                    }
                }
            },
            ValueType::Scalar(_) => {}
        }
        self.script.declaration(format!(
            "(define-fun padding_parts_{id} ((x {})) Bool {})",
            value_sort(ty).smt(),
            and(facts)
        ));
        self.codec_padding_helpers.insert(id);
        self.check_script_budget(self.transition.span)?;
        Ok(id)
    }
    fn codec_id(&mut self, ty: &ValueType) -> usize {
        let next = self.codecs.len();
        *self.codecs.entry(ty.clone()).or_insert(next)
    }
    /// Emit encoders only on demand: validation must not create unused N-byte
    /// zero objects. These are exact definitions, independent of decoder laws.
    pub(super) fn encoder(&mut self, ty: &ValueType) -> LowerResult<usize> {
        let id = self.codec_id(ty);
        if self.codec_encoders.contains(&id) {
            return Ok(id);
        }
        self.script.enable_datatypes();
        let encoded = match ty {
            ValueType::Scalar(scalar) => encode_bits(
                if scalar.is_bool() {
                    "(ite x #x01 #x00)"
                } else {
                    "x"
                },
                u32::from(scalar.bits()),
            ),
            ValueType::Bytes { capacity } => {
                let zeros = self.zero_bytes(format!("(- {capacity} (seq.len x))"));
                // A valid bounded value needs only these header bits. Extending
                // that bounded conversion is exactly the u64 LE wire length,
                // while avoiding a 64-bit BV/Int bridge for tiny capacities.
                // Invalid (over-capacity) values are rejected before Encode.
                let bits = (64 - capacity.leading_zeros()).max(1);
                let length = extend(
                    &format!("((_ int_to_bv {bits}) (seq.len x))"),
                    bits,
                    64,
                    false,
                );
                concat(vec![encode_bits(&length, 64), "x".into(), zeros])
            }
            ValueType::Named(ty_id) => match &self.program.type_definition(*ty_id).kind.clone() {
                TypeDefinitionKind::Record(fields) => {
                    let mut parts = Vec::new();
                    for f in fields {
                        let child = self.encoder(&f.ty)?;
                        parts.push(format!("(encode_{child} ({} x))", selector(*ty_id, f.id.0)));
                    }
                    concat(parts)
                }
                TypeDefinitionKind::Union(variants) => {
                    let width = self.program.value_wire_width(ty).expect("checked width");
                    let mut encoded = empty();
                    for v in variants.iter().rev() {
                        let child = self.encoder(&v.payload)?;
                        let child_width = self
                            .program
                            .value_wire_width(&v.payload)
                            .expect("checked width");
                        let padding = width - 4 - child_width;
                        let zeros = if padding == 0 {
                            empty()
                        } else {
                            self.zero_bytes(padding)
                        };
                        let branch = concat(vec![
                            encode_bits(&bv(v.id.0, 32), 32),
                            format!("(encode_{child} ({} x))", payload_selector(*ty_id, v.id)),
                            zeros,
                        ]);
                        encoded = ite(
                            &format!("((_ is {}) x)", constructor(*ty_id, Some(v.id))),
                            &branch,
                            &encoded,
                        );
                    }
                    encoded
                }
            },
        };
        self.script.declaration(format!(
            "(define-fun encode_{id} ((x {})) (Seq (_ BitVec 8)) {encoded})",
            value_sort(ty).smt()
        ));
        self.codec_encoders.insert(id);
        self.check_script_budget(self.transition.span)?;
        Ok(id)
    }
    /// Decoder validity examines the actual input. A zero-padding regular
    /// language avoids constructing a capacity-sized object for invalid inputs.
    pub(super) fn codec(&mut self, ty: &ValueType) -> LowerResult<usize> {
        let id = self.codec_id(ty);
        if self.codec_decoders.contains(&id) {
            return Ok(id);
        }
        self.script.enable_datatypes();
        let width = self
            .program
            .value_wire_width(ty)
            .map_err(|e| diagnostic("D_VERIFY_LOWER", e, self.transition.span))?;
        let (decoded, valid) = match ty {
            ValueType::Scalar(scalar) => {
                let decoded = decode_bits("s", u32::from(scalar.bits()));
                if scalar.is_bool() {
                    (
                        eq(&decoded, &bv(1, 8)),
                        format!("(bvule {decoded} {})", bv(1, 8)),
                    )
                } else {
                    let valid = valid_value(ty, &decoded);
                    (decoded, valid)
                }
            }
            ValueType::Bytes { capacity } => {
                let len = format!("(ubv_to_int {})", decode_bits("s", 64));
                let padding = extract("s", format!("(+ 8 {len})"), format!("(- {capacity} {len})"));
                (
                    extract("s", 8, &len),
                    and([format!("(<= {len} {capacity})"), all_zero(&padding)]),
                )
            }
            ValueType::Named(ty_id) => match &self.program.type_definition(*ty_id).kind.clone() {
                TypeDefinitionKind::Record(fields) => {
                    let mut dec = Vec::new();
                    let mut valid = Vec::new();
                    let mut offset = 0usize;
                    for f in fields {
                        let child = self.codec(&f.ty)?;
                        let width = self.program.value_wire_width(&f.ty).expect("checked width");
                        let chunk = extract("s", offset, width);
                        dec.push(format!("(decode_{child} {chunk})"));
                        valid.push(format!("(decode_valid_{child} {chunk})"));
                        offset += width;
                    }
                    (application(&constructor(*ty_id, None), &dec), and(valid))
                }
                TypeDefinitionKind::Union(variants) => {
                    let mut decoded = zero_term(ty);
                    let mut valid = Vec::new();
                    for v in variants.iter().rev() {
                        let child = self.codec(&v.payload)?;
                        let child_width = self
                            .program
                            .value_wire_width(&v.payload)
                            .expect("checked width");
                        let chunk = extract("s", 4, child_width);
                        let tag = eq(&decode_bits("s", 32), &bv(v.id.0, 32));
                        decoded = ite(
                            &tag,
                            &application(
                                &constructor(*ty_id, Some(v.id)),
                                &[format!("(decode_{child} {chunk})")],
                            ),
                            &decoded,
                        );
                        valid.push(and([
                            tag,
                            format!("(decode_valid_{child} {chunk})"),
                            all_zero(&extract("s", 4 + child_width, width - 4 - child_width)),
                        ]));
                    }
                    (decoded, or(valid))
                }
            },
        };
        self.script.declaration(format!(
            "(define-fun decode_{id} ((s (Seq (_ BitVec 8)))) {} {decoded})",
            value_sort(ty).smt()
        ));
        self.script.declaration(format!(
            "(define-fun decode_valid_{id} ((s (Seq (_ BitVec 8)))) Bool {})",
            and([format!("(= (seq.len s) {width})"), valid])
        ));
        self.codec_decoders.insert(id);
        self.check_script_budget(self.transition.span)?;
        Ok(id)
    }
}
fn all_zero(sequence: &str) -> String {
    format!("(seq.in.re {sequence} (re.* (seq.to.re (seq.unit #x00))))")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prove(script: &str) {
        let solver = Solver::new();
        let mut params = z3::Params::new();
        params.set_u32("timeout", 5_000);
        solver.set_params(&params);
        solver.from_string(script);
        assert_eq!(
            solver.check(),
            SatResult::Unsat,
            "primitive consequence not proved: {script}\n{:?}",
            solver.get_reason_unknown()
        );
    }
    #[test]
    fn length_header_facts_follow_actual_bit_encoder_and_decoder_at_boundaries() {
        for capacity in [
            0,
            1,
            7,
            8,
            15,
            255,
            256,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            u64::MAX,
        ] {
            let bits = (64 - capacity.leading_zeros()).max(1);
            let value = extend(&format!("((_ int_to_bv {bits}) n)"), bits, 64, false);
            let encoded = encode_bits(&value, 64);
            let decoded = decode_bits(&encoded, 64);
            prove(&format!("(declare-const n Int)(assert (and (<= 0 n) (<= n {capacity})))(assert (not (= (ubv_to_int {decoded}) n)))"));
        }
    }
    #[test]
    fn zero_padding_relation_defines_the_canonical_sequence() {
        for length in [0, 1, 2, 8, 32] {
            let canonical = concat(vec!["(seq.unit #x00)".into(); length]);
            prove(&format!(
                "(declare-const padding (Seq (_ BitVec 8)))(assert (= (seq.len padding) {length}))(assert {})(assert (not (= padding {canonical})))",
                all_zero("padding")
            ));
        }
    }
    #[test]
    fn sequence_constructor_boundary_and_zero_padding_facts_are_theorems() {
        let declarations="(declare-const a (Seq (_ BitVec 8)))(declare-const b (Seq (_ BitVec 8)))(declare-const c (Seq (_ BitVec 8)))(declare-const n Int)";
        for law in [
            "(= (seq.len (seq.++ a b c)) (+ (seq.len a) (seq.len b) (seq.len c)))",
            "(= (seq.extract (seq.++ a b c) 0 (seq.len a)) a)",
            "(= (seq.extract (seq.++ a b c) (seq.len a) (seq.len b)) b)",
            "(= (seq.extract (seq.++ a b c) (+ (seq.len a) (seq.len b)) (seq.len c)) c)",
            "(=> (and (<= 0 n) (<= n (seq.len c))) (= (seq.len (seq.extract c 0 n)) n))",
        ] {
            prove(&format!("{declarations}(assert (not {law}))"));
        }
        prove(&format!(
            "{declarations}(assert {})(assert (and (<= 0 n) (<= n (seq.len c))))(assert (not {}))",
            all_zero("c"),
            all_zero("(seq.extract c 0 n)")
        ));
        prove(&format!(
            "{declarations}(assert {})(assert {})(assert (not {}))",
            all_zero("a"),
            all_zero("b"),
            all_zero("(seq.++ a b)")
        ));
        prove(&format!("(assert (not {}))", all_zero("(seq.unit #x00)")));
    }
}
