//! Exact scalar values and the shared checked arithmetic semantics.
use crate::{
    ast::LiteralValue,
    ir::{BinaryOp, UnaryOp},
    types::{Domain, ScalarType},
};
use num_bigint::{BigInt, Sign};
use num_traits::{Signed, ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Value {
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
