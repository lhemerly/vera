//! Compatibility with the canonical Python compiler's stable program identity.
//!
//! The development AST JSON deliberately wraps arbitrary-size numbers. Durable
//! identities instead retain the original AST shape and Python json.dumps'
//! sorted keys, ASCII escaping, and default whitespace separators.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::ast::Module;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Canonical {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Canonical>),
    Object(BTreeMap<String, Canonical>),
}

impl Canonical {
    pub(super) fn object<const N: usize>(members: [(&str, Canonical); N]) -> Self {
        Self::Object(
            members
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
        )
    }

    fn ast(value: Json) -> Self {
        match value {
            Json::Null => Self::Null,
            Json::Bool(value) => Self::Bool(value),
            Json::Number(value) => Self::Number(value.to_string()),
            Json::String(value) => Self::String(value),
            Json::Array(values) => Self::Array(values.into_iter().map(Self::ast).collect()),
            Json::Object(mut members) => {
                if members.len() == 1 {
                    if let Some(Json::String(value)) = members.get("integer") {
                        return Self::Number(value.clone());
                    }
                    if let Some(Json::Object(decimal)) = members.get("decimal") {
                        let negative = decimal["negative"].as_bool().unwrap_or(false);
                        let digits = decimal["digits"].as_str().unwrap_or("0");
                        let scale = decimal["scale"].as_u64().unwrap_or(0);
                        return Self::object([(
                            "value",
                            Self::String(decimal_string(negative, digits, scale)),
                        )]);
                    }
                }
                Self::Object(
                    std::mem::take(&mut members)
                        .into_iter()
                        .map(|(name, value)| (name, Self::ast(value)))
                        .collect(),
                )
            }
        }
    }

    fn append(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => out.push_str(value),
            Self::String(value) => append_string(value, out),
            Self::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    value.append(out);
                }
                out.push(']');
            }
            Self::Object(values) => {
                out.push('{');
                for (index, (key, value)) in values.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    append_string(key, out);
                    out.push_str(": ");
                    value.append(out);
                }
                out.push('}');
            }
        }
    }

    pub(super) fn digest(&self) -> [u8; 32] {
        let mut source = String::new();
        self.append(&mut source);
        Sha256::digest(source.as_bytes()).into()
    }
}

fn append_string(value: &str, out: &mut String) {
    use std::fmt::Write;

    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(ch),
            _ => {
                for unit in ch.encode_utf16(&mut [0; 2]) {
                    write!(out, "\\u{unit:04x}").expect("writing to a String is infallible");
                }
            }
        }
    }
    out.push('"');
}

/// Decimal.__str__ uses fixed notation when the adjusted exponent is at least
/// -6, preserving trailing zero precision, and scientific notation otherwise.
fn decimal_string(negative: bool, digits: &str, scale: u64) -> String {
    let sign = if negative { "-" } else { "" };
    let adjusted = digits.len() as i128 - i128::from(scale) - 1;
    if scale == 0 {
        format!("{sign}{digits}")
    } else if adjusted >= -6 {
        let split = digits.len() as i128 - i128::from(scale);
        if split > 0 {
            let split = split as usize;
            format!("{sign}{}.{}", &digits[..split], &digits[split..])
        } else {
            format!("{sign}0.{}{digits}", "0".repeat((-split) as usize))
        }
    } else if digits.len() == 1 {
        format!("{sign}{digits}E{adjusted:+}")
    } else {
        format!("{sign}{}.{}E{adjusted:+}", &digits[..1], &digits[1..])
    }
}

pub(super) fn program(module: &Module) -> [u8; 32] {
    Canonical::object([
        ("ast", Canonical::ast(module.canonical_json())),
        ("language", Canonical::String("VERA-1.1.1".into())),
    ])
    .digest()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_source;

    fn hex(bytes: [u8; 32]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn empty_and_named_modules_match_independent_python_hash_vectors() {
        assert_eq!(
            hex(program(&parse_source("").unwrap())),
            "290c2a375a345a06e8d55352127af8089d2e9ea778b36d156403465c625d9736"
        );
        assert_eq!(
            hex(program(&parse_source("module Example;").unwrap())),
            "7f091913aa7f4abb70d768a6f7349011d8d0c7c42af7d49dd6a65eb4a3b9b102"
        );
    }

    #[test]
    fn exact_decimal_and_unicode_identity_match_python() {
        let source = "module Exact;table T capacity 2 {key id:id64<T>;col v:dec128;col symbol:sym32;} transition make() reads [] writes [T[*].v,T[*].symbol] body {let %k:id64<T>=table_alloc(T,{v:-0.0000000,symbol:\"á🦀\\n\"});commit;}";
        assert_eq!(
            hex(program(&parse_source(source).unwrap())),
            "0c329b55ff04d03fdfb26242c7ce4945da32a18d879d5393906311f9d553f5e6"
        );
        assert_eq!(
            program(&parse_source(source).unwrap()),
            program(
                &parse_source(&format!("// source positions are not identity\n{source}")).unwrap()
            )
        );
    }

    #[test]
    fn decimal_display_preserves_python_notation_boundaries() {
        for (negative, digits, scale, expected) in [
            (false, "1", 6, "0.000001"),
            (false, "1", 7, "1E-7"),
            (true, "0", 7, "-0E-7"),
            (false, "1000", 3, "1.000"),
            (false, "123", 9, "1.23E-7"),
            (
                true,
                "10000000000000000000000000001",
                18,
                "-10000000000.000000000000000001",
            ),
        ] {
            assert_eq!(decimal_string(negative, digits, scale), expected);
        }
    }
}
