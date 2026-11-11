//! Exact syntax literals. Contextual typing and range checks belong to analysis.

use std::fmt;

use num_bigint::BigInt;
use serde::{ser::SerializeMap, Serialize, Serializer};

/// Canonical, arbitrary-precision signed integer text. No host-width narrowing.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IntegerLiteral(String);

impl IntegerLiteral {
    pub fn parse(source: &str) -> Result<Self, String> {
        let source = source.replace('_', "");
        let (negative, unsigned) = source
            .strip_prefix('-')
            .map_or((false, source.as_str()), |s| (true, s));
        let (radix, digits) = unsigned
            .strip_prefix("0x")
            .or_else(|| unsigned.strip_prefix("0X"))
            .map_or((10, unsigned), |s| (16, s));
        let mut value = BigInt::parse_bytes(digits.as_bytes(), radix)
            .ok_or_else(|| "invalid integer literal".to_string())?;
        if negative {
            value = -value;
        }
        Ok(Self(value.to_str_radix(10)))
    }

    pub fn as_decimal(&self) -> &str {
        &self.0
    }

    pub fn negated(&self) -> Self {
        if self.0 == "0" {
            return self.clone();
        }
        Self(
            self.0
                .strip_prefix('-')
                .map_or_else(|| format!("-{}", self.0), str::to_owned),
        )
    }
}

impl fmt::Display for IntegerLiteral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for IntegerLiteral {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_map(Some(1))?;
        out.serialize_entry("integer", &self.0)?;
        out.end()
    }
}

/// A base-ten coefficient and scale, including signed zero and trailing zeros.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DecimalLiteral {
    pub negative: bool,
    pub digits: String,
    pub scale: u32,
}

impl DecimalLiteral {
    pub fn parse(source: &str) -> Result<Self, String> {
        let source = source.replace('_', "");
        let (negative, unsigned) = source
            .strip_prefix('-')
            .map_or((false, source.as_str()), |s| (true, s));
        let (whole, fraction) = unsigned
            .split_once('.')
            .ok_or_else(|| "decimal literal requires a fractional part".to_string())?;
        if fraction.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("invalid decimal literal".into());
        }
        let coefficient = format!("{whole}{fraction}");
        let digits = coefficient.trim_start_matches('0');
        Ok(Self {
            negative,
            digits: if digits.is_empty() { "0" } else { digits }.into(),
            scale: u32::try_from(fraction.len())
                .map_err(|_| "decimal scale exceeds supported source size".to_string())?,
        })
    }

    pub fn negated(&self) -> Self {
        Self {
            negative: !self.negative,
            ..self.clone()
        }
    }
}

impl Serialize for DecimalLiteral {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Decimal<'a> {
            negative: bool,
            digits: &'a str,
            scale: u32,
        }
        let mut out = serializer.serialize_map(Some(1))?;
        out.serialize_entry(
            "decimal",
            &Decimal {
                negative: self.negative,
                digits: &self.digits,
                scale: self.scale,
            },
        )?;
        out.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_preserve_all_256_bits_and_more_until_typing() {
        let value = IntegerLiteral::parse(
            "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        )
        .unwrap();
        assert_eq!(
            value.as_decimal(),
            "115792089237316195423570985008687907853269984665640564039457584007913129639935"
        );
        assert_eq!(value.negated().negated(), value);
        assert_eq!(
            IntegerLiteral::parse("00_0123").unwrap().as_decimal(),
            "123"
        );
        assert_eq!(
            IntegerLiteral::parse("0").unwrap().negated().as_decimal(),
            "0"
        );
    }

    #[test]
    fn decimals_keep_precision_scale_and_signed_zero() {
        let value = DecimalLiteral::parse("0001.00000000000000000100").unwrap();
        assert_eq!(value.digits, "100000000000000000100");
        assert_eq!(value.scale, 20);
        assert_eq!(value.negated().negated(), value);
        let zero = DecimalLiteral::parse("-0.000").unwrap();
        assert!(zero.negative);
        assert_eq!(zero.digits, "0");
        assert_eq!(zero.scale, 3);
        assert_eq!(DecimalLiteral::parse(".5").unwrap().digits, "5");
    }

    #[test]
    fn malformed_literals_return_errors() {
        for text in ["", "0x", "g", "12x"] {
            assert!(IntegerLiteral::parse(text).is_err(), "{text}");
        }
        for text in ["", "1", "1.", "1.2.3", "-.x"] {
            assert!(DecimalLiteral::parse(text).is_err(), "{text}");
        }
    }
}
