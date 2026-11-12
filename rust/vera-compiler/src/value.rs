//! Exact scalar values and the shared checked arithmetic semantics.
use crate::{
    ast::LiteralValue,
    ir::{BinaryOp, CheckedProgram, TypeDefinitionKind, TypeId, UnaryOp, VariantId},
    types::{Domain, ScalarType, ValueType},
};
use num_bigint::{BigInt, Sign};
use num_traits::{Signed, ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Bytes(Vec<u8>),
    Record {
        ty: TypeId,
        fields: Vec<Value>,
    },
    Variant {
        ty: TypeId,
        tag: VariantId,
        payload: Box<Value>,
    },
    Bool(bool),
    Int(BigInt),
}
impl Value {
    pub fn zero(ty: &ScalarType) -> Self {
        if ty.is_bool() {
            Self::Bool(false)
        } else {
            Self::Int(BigInt::zero())
        }
    }
    pub fn int(&self) -> Result<&BigInt, String> {
        match self {
            Self::Int(v) => Ok(v),
            _ => Err("expected an integer/scaled value".into()),
        }
    }
    pub fn boolean(&self) -> Result<bool, String> {
        match self {
            Self::Bool(v) => Ok(*v),
            _ => Err("expected a boolean value".into()),
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.int().ok()?.to_u64()
    }
    pub fn to_json(&self, ty: &ScalarType) -> serde_json::Value {
        match self {
            Self::Bytes(_) | Self::Record { .. } | Self::Variant { .. } => {
                panic!("structured JSON requires value_to_json with its type arena")
            }
            Self::Bool(v) => serde_json::Value::Bool(*v),
            Self::Int(v) => {
                if matches!(ty, ScalarType::Bytes32) {
                    let (_, mut bytes) = v.to_bytes_le();
                    bytes.resize(32, 0);
                    serde_json::Value::String(format!(
                        "0x{}",
                        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    ))
                } else {
                    serde_json::from_str(&v.to_string())
                        .expect("BigInt produces valid JSON integer text")
                }
            }
        }
    }
}

pub fn coerce(value: &Value, ty: &ScalarType) -> Result<Value, String> {
    if ty.is_bool() {
        value.boolean()?;
        return Ok(value.clone());
    }
    let number = value.int()?;
    if number < &ty.min_value() || number > &ty.max_value() {
        Err(format!("value is outside {ty}"))
    } else {
        Ok(value.clone())
    }
}
pub fn in_domain(value: &BigInt, domain: &Domain) -> bool {
    value >= &domain.min_value() && value <= &domain.max_value()
}
pub fn symbol_id(text: &str) -> u32 {
    let hash = Sha256::digest(text.as_bytes());
    u32::from_le_bytes(hash[..4].try_into().expect("SHA-256 has four bytes"))
}

fn normalized_literal(literal: &LiteralValue, ty: &ScalarType) -> Result<Value, String> {
    let value = match literal {
        LiteralValue::Bool(v) => Value::Bool(*v),
        LiteralValue::Integer(v) => Value::Int(
            BigInt::parse_bytes(v.as_decimal().as_bytes(), 10).ok_or("invalid integer literal")?,
        ),
        LiteralValue::String(text) => match ty {
            ScalarType::Sym32 => Value::Int(BigInt::from(symbol_id(text))),
            ScalarType::Enum(tags) => Value::Int(BigInt::from(
                tags.iter()
                    .position(|s| s == text)
                    .ok_or("unknown enum tag")?,
            )),
            _ => return Err(format!("string literal is not a {ty}")),
        },
        LiteralValue::Decimal(v) => {
            if v.digits.is_empty() || !v.digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid decimal coefficient".into());
            }
            if !ty.is_decimal() {
                return Err("fractional literals require a decimal type".into());
            }
            let mut coefficient = if v.scale > ty.scale() {
                let excess = (v.scale - ty.scale()) as usize;
                if v.digits == "0" {
                    BigInt::zero()
                } else {
                    if excess > v.digits.len()
                        || v.digits[v.digits.len() - excess..]
                            .bytes()
                            .any(|b| b != b'0')
                    {
                        return Err("fractional precision exceeds scalar scale".into());
                    }
                    let keep = &v.digits[..v.digits.len() - excess];
                    BigInt::parse_bytes(keep.as_bytes(), 10).unwrap_or_else(BigInt::zero)
                }
            } else {
                BigInt::parse_bytes(v.digits.as_bytes(), 10).ok_or("invalid decimal coefficient")?
                    * BigInt::from(10u8).pow(ty.scale() - v.scale)
            };
            if v.negative {
                coefficient = -coefficient;
            }
            Value::Int(coefficient)
        }
    };
    Ok(value)
}

pub fn literal_value(literal: &LiteralValue, ty: &ScalarType) -> Result<Value, String> {
    coerce(&normalized_literal(literal, ty)?, ty)
}
pub fn literal_in_domain(literal: &LiteralValue, domain: &Domain) -> Result<Value, String> {
    let value = normalized_literal(literal, &domain.scalar)?;
    if domain.scalar.is_bool() {
        value.boolean()?;
        return Ok(value);
    }
    if in_domain(value.int()?, domain) {
        Ok(value)
    } else {
        Err("literal exceeds its evaluation domain".into())
    }
}

pub fn checked_unary(op: UnaryOp, value: &Value, domain: &Domain) -> Result<Value, String> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!value.boolean()?)),
        UnaryOp::Neg => {
            let result = -value.int()?;
            if in_domain(&result, domain) {
                Ok(Value::Int(result))
            } else {
                Err("arithmetic overflow".into())
            }
        }
    }
}

pub fn checked_binary(
    op: BinaryOp,
    a: &Value,
    b: &Value,
    domain: &Domain,
) -> Result<Value, String> {
    if op.boolean() {
        let (a, b) = (a.boolean()?, b.boolean()?);
        return Ok(Value::Bool(match op {
            BinaryOp::And => a & b,
            BinaryOp::Or => a | b,
            BinaryOp::Xor => a ^ b,
            _ => unreachable!(),
        }));
    }
    if op.comparison() {
        if matches!(op, BinaryOp::Eq | BinaryOp::Ne) {
            let equal = a == b;
            return Ok(Value::Bool(if op == BinaryOp::Eq { equal } else { !equal }));
        }
        let (a, b) = (a.int()?, b.int()?);
        return Ok(Value::Bool(match op {
            BinaryOp::Lt => a < b,
            BinaryOp::Le => a <= b,
            BinaryOp::Gt => a > b,
            BinaryOp::Ge => a >= b,
            _ => unreachable!(),
        }));
    }
    let (a, b) = (a.int()?, b.int()?);
    let result = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Min => a.min(b).clone(),
        BinaryOp::Max => a.max(b).clone(),
        BinaryOp::Mul => {
            let product = a * b;
            let scale = BigInt::from(10u8).pow(domain.scale());
            if !(&product % &scale).is_zero() {
                return Err("inexact decimal multiplication".into());
            }
            product / scale
        }
        BinaryOp::Div | BinaryOp::Rem => {
            if b.is_zero()
                || (domain.signed() && a == &domain.min_value() && b == &BigInt::from(-1))
            {
                return Err("invalid division/remainder domain".into());
            }
            if op == BinaryOp::Rem {
                a % b
            } else {
                let numerator = a * BigInt::from(10u8).pow(domain.scale());
                if domain.scalar.is_decimal() && !(&numerator % b).is_zero() {
                    return Err("inexact decimal division".into());
                }
                numerator / b
            }
        }
        _ => return Err("invalid arithmetic operation".into()),
    };
    if in_domain(&result, domain) {
        Ok(Value::Int(result))
    } else {
        Err("arithmetic overflow".into())
    }
}

pub fn low_u64(value: &Value) -> u64 {
    match value {
        Value::Bytes(_) | Value::Record { .. } | Value::Variant { .. } => {
            panic!("structured values require canonical hashing")
        }
        Value::Bool(v) => u64::from(*v),
        Value::Int(v) => {
            let mask = (BigInt::from(1u8) << 64usize) - 1u8;
            (v & mask).to_u64().expect("masked integer fits u64")
        }
    }
}

pub fn from_json(ty: &ScalarType, json: &serde_json::Value) -> Result<Value, String> {
    if ty.is_bool() {
        return json
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "boolean argument required".into());
    }
    let text = match json {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return Err("scalar argument must be an integer or typed string".into()),
    };
    if matches!(ty, ScalarType::Sym32) && json.is_string() {
        return Ok(Value::Int(BigInt::from(symbol_id(&text))));
    }
    if let ScalarType::Enum(tags) = ty {
        if let Some(index) = tags.iter().position(|s| s == &text) {
            return Ok(Value::Int(BigInt::from(index)));
        }
    }
    if matches!(ty, ScalarType::Bytes32) {
        if let Some(hex) = text.strip_prefix("0x") {
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("bytes32 requires exactly 64 hex digits".into());
            }
            let mut bytes = Vec::with_capacity(32);
            for i in (0..64).step_by(2) {
                bytes.push(
                    u8::from_str_radix(&hex[i..i + 2], 16)
                        .map_err(|_| "invalid bytes32 hex value")?,
                );
            }
            return Ok(Value::Int(BigInt::from_bytes_le(Sign::Plus, &bytes)));
        }
    }
    let value = if ty.is_decimal() && text.contains('.') {
        let literal = crate::literal::DecimalLiteral::parse(&text)?;
        literal_value(&LiteralValue::Decimal(literal), ty)?
    } else {
        let value = crate::literal::IntegerLiteral::parse(&text)?;
        Value::Int(
            BigInt::parse_bytes(value.as_decimal().as_bytes(), 10)
                .ok_or("invalid integer argument")?,
        )
    };
    coerce(&value, ty)
}

pub fn to_le_bytes(value: &Value, width: usize, signed: bool) -> Result<Vec<u8>, String> {
    if !(1..=32).contains(&width) {
        return Err("invalid scalar wire width".into());
    }
    if let Value::Bool(v) = value {
        return if width == 1 {
            Ok(vec![u8::from(*v)])
        } else {
            Err("invalid boolean width".into())
        };
    }
    let n = value.int()?;
    if n.is_negative() && !signed {
        return Err("negative unsigned value".into());
    }
    let modulus = BigInt::from(1u8) << (width * 8);
    if signed {
        let bound = &modulus >> 1;
        if n < &(-&bound) || n >= &bound {
            return Err("value exceeds signed wire width".into());
        }
    }
    let raw = if n.is_negative() {
        n + &modulus
    } else {
        n.clone()
    };
    if raw.is_negative() || raw >= modulus {
        return Err("value exceeds wire width".into());
    }
    let (_, mut bytes) = raw.to_bytes_le();
    bytes.resize(width, 0);
    Ok(bytes)
}

pub fn from_le_bytes(bytes: &[u8], ty: &ScalarType) -> Result<Value, String> {
    if bytes.len() != ty.byte_width() {
        return Err("invalid scalar wire width".into());
    }
    if ty.is_bool() {
        return match bytes[0] {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            _ => Err("invalid boolean encoding".into()),
        };
    }
    let mut value = BigInt::from_bytes_le(Sign::Plus, bytes);
    if ty.signed() && bytes.last().is_some_and(|b| b & 128 != 0) {
        value -= BigInt::from(1u8) << (bytes.len() * 8);
    }
    coerce(&Value::Int(value), ty)
}

pub fn validate_value(
    program: &CheckedProgram,
    value: &Value,
    ty: &ValueType,
) -> Result<(), String> {
    match ty {
        ValueType::Bytes { capacity } => match value {
            Value::Bytes(bytes) if (bytes.len() as u128) <= u128::from(*capacity) => Ok(()),
            Value::Bytes(_) => Err("byte length exceeds capacity".into()),
            _ => Err("expected bytes".into()),
        },
        ValueType::Scalar(t) => {
            if t.is_bool() {
                value.boolean()?;
            } else {
                let number = value.int()?;
                if number < &t.min_value() || number > &t.max_value() {
                    return Err(format!("value is outside {t}"));
                }
            }
            Ok(())
        }
        ValueType::Named(id) => match (&program.type_definition(*id).kind, value) {
            (TypeDefinitionKind::Record(defs), Value::Record { ty: actual, fields })
                if actual == id && fields.len() == defs.len() =>
            {
                for (field, def) in fields.iter().zip(defs) {
                    validate_value(program, field, &def.ty)?;
                }
                Ok(())
            }
            (
                TypeDefinitionKind::Union(defs),
                Value::Variant {
                    ty: actual,
                    tag,
                    payload,
                },
            ) if actual == id => {
                let def = defs.get(tag.0).ok_or("unknown union tag")?;
                validate_value(program, payload, &def.payload)
            }
            _ => Err(format!(
                "value does not match nominal type {}",
                program.type_definition(*id).name
            )),
        },
    }
}

pub fn coerce_value(
    program: &CheckedProgram,
    value: &Value,
    ty: &ValueType,
) -> Result<Value, String> {
    validate_value(program, value, ty)?;
    Ok(value.clone())
}
pub fn zero_value(program: &CheckedProgram, ty: &ValueType) -> Value {
    match ty {
        ValueType::Bytes { .. } => Value::Bytes(Vec::new()),
        ValueType::Scalar(t) => Value::zero(t),
        ValueType::Named(id) => match &program.type_definition(*id).kind {
            TypeDefinitionKind::Record(fields) => Value::Record {
                ty: *id,
                fields: fields.iter().map(|f| zero_value(program, &f.ty)).collect(),
            },
            TypeDefinitionKind::Union(variants) => Value::Variant {
                ty: *id,
                tag: VariantId(0),
                payload: Box::new(zero_value(program, &variants[0].payload)),
            },
        },
    }
}
pub fn value_to_json(program: &CheckedProgram, value: &Value, ty: &ValueType) -> serde_json::Value {
    match ty {
        ValueType::Bytes { .. } => {
            let Value::Bytes(bytes) = value else {
                panic!("validated bytes required for JSON conversion")
            };
            serde_json::Value::String(format!(
                "0x{}",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ))
        }
        ValueType::Scalar(t) => value.to_json(t),
        ValueType::Named(id) => match (&program.type_definition(*id).kind, value) {
            (TypeDefinitionKind::Record(defs), Value::Record { fields, .. }) => {
                serde_json::Value::Object(
                    defs.iter()
                        .zip(fields)
                        .map(|(d, v)| (d.name.clone(), value_to_json(program, v, &d.ty)))
                        .collect(),
                )
            }
            (TypeDefinitionKind::Union(defs), Value::Variant { tag, payload, .. }) => {
                serde_json::json!({"tag":defs[tag.0].name,"payload":value_to_json(program,payload,&defs[tag.0].payload)})
            }
            _ => panic!("validated value required for JSON conversion"),
        },
    }
}
pub fn value_from_json(
    program: &CheckedProgram,
    json: &serde_json::Value,
    ty: &ValueType,
) -> Result<Value, String> {
    match ty {
        ValueType::Bytes { capacity } => {
            let text = json
                .as_str()
                .ok_or("bytes require a 0x-prefixed hexadecimal string")?;
            let hex = text
                .strip_prefix("0x")
                .ok_or("bytes require a 0x-prefixed hexadecimal string")?;
            if (hex.len() / 2) as u128 > u128::from(*capacity) {
                return Err("byte length exceeds capacity".into());
            }
            let value = Value::Bytes(parse_hex_bytes(hex)?);
            validate_value(program, &value, ty)?;
            Ok(value)
        }
        ValueType::Scalar(t) => from_json(t, json),
        ValueType::Named(id) => match &program.type_definition(*id).kind {
            TypeDefinitionKind::Record(defs) => {
                let obj = json.as_object().ok_or("record requires an object")?;
                if obj.len() != defs.len() {
                    return Err("record fields differ from its declaration".into());
                }
                Ok(Value::Record {
                    ty: *id,
                    fields: defs
                        .iter()
                        .map(|d| {
                            value_from_json(
                                program,
                                obj.get(&d.name)
                                    .ok_or_else(|| format!("missing field {}", d.name))?,
                                &d.ty,
                            )
                        })
                        .collect::<Result<Vec<_>, String>>()?,
                })
            }
            TypeDefinitionKind::Union(defs) => {
                let obj = json.as_object().ok_or("union requires a tagged object")?;
                if obj.len() != 2 {
                    return Err("union object requires exactly tag and payload".into());
                }
                let name = obj
                    .get("tag")
                    .and_then(|v| v.as_str())
                    .ok_or("union requires a string tag")?;
                let def = defs
                    .iter()
                    .find(|d| d.name == name)
                    .ok_or("unknown union tag")?;
                let payload = value_from_json(
                    program,
                    obj.get("payload").ok_or("union requires payload")?,
                    &def.payload,
                )?;
                Ok(Value::Variant {
                    ty: *id,
                    tag: def.id,
                    payload: Box::new(payload),
                })
            }
        },
    }
}
pub fn encode_value(
    program: &CheckedProgram,
    value: &Value,
    ty: &ValueType,
) -> Result<Vec<u8>, String> {
    validate_value(program, value, ty)?;
    fn encode(
        program: &CheckedProgram,
        value: &Value,
        ty: &ValueType,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        match ty {
            ValueType::Bytes { capacity } => {
                let Value::Bytes(bytes) = value else {
                    return Err("expected bytes".into());
                };
                out.extend(
                    u64::try_from(bytes.len())
                        .map_err(|_| "byte length exceeds u64")?
                        .to_le_bytes(),
                );
                out.extend(bytes);
                let padding = usize::try_from(*capacity)
                    .map_err(|_| "byte capacity exceeds usize")?
                    .checked_sub(bytes.len())
                    .ok_or("byte length exceeds capacity")?;
                out.resize(
                    out.len()
                        .checked_add(padding)
                        .ok_or("wire length exceeds usize")?,
                    0,
                );
            }
            ValueType::Scalar(t) => out.extend(to_le_bytes(value, t.byte_width(), t.signed())?),
            ValueType::Named(id) => match (&program.type_definition(*id).kind, value) {
                (TypeDefinitionKind::Record(defs), Value::Record { fields, .. }) => {
                    for (def, value) in defs.iter().zip(fields) {
                        encode(program, value, &def.ty, out)?;
                    }
                }
                (TypeDefinitionKind::Union(defs), Value::Variant { tag, payload, .. }) => {
                    let start = out.len();
                    out.extend(
                        u32::try_from(tag.0)
                            .map_err(|_| "union tag exceeds u32 wire representation")?
                            .to_le_bytes(),
                    );
                    encode(program, payload, &defs[tag.0].payload, out)?;
                    out.resize(
                        start
                            .checked_add(program.value_wire_width(ty)?)
                            .ok_or("wire length exceeds usize")?,
                        0,
                    );
                }
                _ => return Err("invalid structured value".into()),
            },
        };
        Ok(())
    }
    let width = program.value_wire_width(ty)?;
    let mut out = Vec::new();
    out.try_reserve_exact(width)
        .map_err(|_| "cannot allocate canonical value encoding")?;
    encode(program, value, ty, &mut out)?;
    Ok(out)
}
pub fn decode_value(
    program: &CheckedProgram,
    bytes: &[u8],
    ty: &ValueType,
) -> Result<Value, String> {
    if bytes.len() != program.value_wire_width(ty)? {
        return Err("canonical value width mismatch".into());
    }
    match ty {
        ValueType::Bytes { capacity } => {
            let len = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            if len > *capacity {
                return Err("byte length exceeds capacity".into());
            }
            let len = usize::try_from(len).map_err(|_| "byte length exceeds usize")?;
            if bytes[8 + len..].iter().any(|b| *b != 0) {
                return Err("noncanonical byte padding".into());
            }
            let mut value = Vec::new();
            value
                .try_reserve_exact(len)
                .map_err(|_| "cannot allocate decoded bytes")?;
            value.extend_from_slice(&bytes[8..8 + len]);
            Ok(Value::Bytes(value))
        }
        ValueType::Scalar(t) => from_le_bytes(bytes, t),
        ValueType::Named(id) => match &program.type_definition(*id).kind {
            TypeDefinitionKind::Record(defs) => {
                let mut pos = 0usize;
                let mut fields = Vec::new();
                for def in defs {
                    let width = program.value_wire_width(&def.ty)?;
                    fields.push(decode_value(program, &bytes[pos..pos + width], &def.ty)?);
                    pos += width;
                }
                Ok(Value::Record { ty: *id, fields })
            }
            TypeDefinitionKind::Union(defs) => {
                let tag = VariantId(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize);
                let def = defs.get(tag.0).ok_or("invalid canonical union tag")?;
                let width = program.value_wire_width(&def.payload)?;
                let payload = decode_value(program, &bytes[4..4 + width], &def.payload)?;
                if bytes[4 + width..].iter().any(|b| *b != 0) {
                    return Err("noncanonical union padding".into());
                }
                Ok(Value::Variant {
                    ty: *id,
                    tag,
                    payload: Box::new(payload),
                })
            }
        },
    }
}

/// Parse exact hexadecimal bytes without integer conversion or leading-zero loss.
pub fn parse_hex_bytes(text: &str) -> Result<Vec<u8>, String> {
    if !text.is_ascii() || !text.len().is_multiple_of(2) {
        return Err("hexadecimal bytes require an even number of ASCII hex digits".into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(text.len() / 2)
        .map_err(|_| "cannot allocate hexadecimal bytes")?;
    for pair in text.as_bytes().chunks_exact(2) {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        };
        bytes.push(
            digit(pair[0]).ok_or("invalid hexadecimal byte")? * 16
                + digit(pair[1]).ok_or("invalid hexadecimal byte")?,
        );
    }
    Ok(bytes)
}
