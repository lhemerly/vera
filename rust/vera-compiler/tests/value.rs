use num_bigint::BigInt;
use serde_json::json;
use vera_compiler::{
    ast::LiteralValue,
    ir::{BinaryOp, UnaryOp},
    literal::{DecimalLiteral, IntegerLiteral},
    types::{Domain, ScalarType},
    value::{self, Value},
};

fn int(value: i128) -> Value {
    Value::Int(BigInt::from(value))
}

#[test]
fn scalar_boundaries_and_widened_aggregate_domains_are_distinct() {
    let scalar = Domain::scalar(ScalarType::U8);
    assert!(value::checked_binary(BinaryOp::Add, &int(255), &int(1), &scalar).is_err());
    assert!(value::checked_binary(BinaryOp::Sub, &int(0), &int(1), &scalar).is_err());
    let wide = Domain::aggregate(ScalarType::U8);
    assert_eq!(
        value::checked_binary(BinaryOp::Add, &int(255), &int(1), &wide).unwrap(),
        int(256)
    );
    assert!(value::coerce(&int(256), &ScalarType::U8).is_err());
    let literal = LiteralValue::Integer(IntegerLiteral::parse("1000").unwrap());
    assert!(value::literal_value(&literal, &ScalarType::U8).is_err());
    assert_eq!(
        value::literal_in_domain(&literal, &wide).unwrap(),
        int(1000)
    );
}

#[test]
fn signed_division_truncates_and_signed_minimum_traps() {
    let domain = Domain::scalar(ScalarType::I64);
    assert_eq!(
        value::checked_binary(BinaryOp::Div, &int(-3), &int(2), &domain).unwrap(),
        int(-1)
    );
    assert_eq!(
        value::checked_binary(BinaryOp::Rem, &int(-3), &int(2), &domain).unwrap(),
        int(-1)
    );
    for op in [BinaryOp::Div, BinaryOp::Rem] {
        assert!(value::checked_binary(op, &int(i64::MIN as i128), &int(-1), &domain).is_err());
    }
    assert!(value::checked_unary(UnaryOp::Neg, &int(i64::MIN as i128), &domain).is_err());
}

#[test]
fn decimal_scaling_is_exact_and_rejects_fractional_remainders() {
    let domain = Domain::scalar(ScalarType::Dec64);
    assert_eq!(
        value::checked_binary(BinaryOp::Mul, &int(100_000_000), &int(30_000_000), &domain).unwrap(),
        int(30_000_000)
    );
    assert_eq!(
        value::checked_binary(
            BinaryOp::Div,
            &int(-300_000_000),
            &int(200_000_000),
            &domain
        )
        .unwrap(),
        int(-150_000_000)
    );
    assert!(value::checked_binary(BinaryOp::Mul, &int(1), &int(1), &domain).is_err());
    assert!(
        value::checked_binary(BinaryOp::Div, &int(100_000_000), &int(300_000_000), &domain)
            .is_err()
    );
    let exact =
        LiteralValue::Decimal(DecimalLiteral::parse("-10000000000.000000000000000001").unwrap());
    assert_eq!(
        value::literal_value(&exact, &ScalarType::Dec128).unwrap(),
        int(-10000000000000000000000000001)
    );
}

#[test]
fn typed_json_preserves_bytes_symbols_and_full_width_integers() {
    let bytes = "0x0100000000000000000000000000000000000000000000000000000000000000";
    let value = value::from_json(&ScalarType::Bytes32, &json!(bytes)).unwrap();
    assert_eq!(value, int(1));
    assert_eq!(value.to_json(&ScalarType::Bytes32), json!(bytes));
    assert_eq!(value::symbol_id("abc"), 0xbf1678ba);
    assert_eq!(
        value::from_json(&ScalarType::Sym32, &json!("abc")).unwrap(),
        int(0xbf1678ba)
    );
    assert!(value::from_json(
        &ScalarType::Bytes32,
        &json!(format!("0x{}", "€".repeat(21) + "x"))
    )
    .is_err());
    let huge = Value::Int(ScalarType::Dec128.max_value());
    assert_eq!(
        huge.to_json(&ScalarType::Dec128).to_string(),
        "170141183460469231731687303715884105727"
    );
}

#[test]
fn canonical_scalar_bytes_validate_lengths_and_domains() {
    assert!(value::to_le_bytes(&Value::Int(BigInt::from(1u8) << 63usize), 8, true).is_err());
    assert!(value::to_le_bytes(&Value::Int(-(BigInt::from(1u8) << 63usize) - 1), 8, true).is_err());
    assert!(value::to_le_bytes(&int(0), 0, false).is_err());
    for ty in [
        ScalarType::Bool,
        ScalarType::U64,
        ScalarType::I64,
        ScalarType::Dec128,
        ScalarType::Bytes32,
    ] {
        let value = if ty.is_bool() {
            Value::Bool(true)
        } else {
            Value::Int(ty.max_value())
        };
        let bytes = value::to_le_bytes(&value, ty.byte_width(), ty.signed()).unwrap();
        assert_eq!(value::from_le_bytes(&bytes, &ty).unwrap(), value);
    }
    assert!(value::from_le_bytes(&[2], &ScalarType::Bool).is_err());
    assert!(value::from_le_bytes(&[3], &ScalarType::Enum(vec!["A".into(), "B".into()])).is_err());
    assert!(value::from_le_bytes(&[0], &ScalarType::Dec128).is_err());
}

#[test]
fn malformed_public_decimal_ast_returns_diagnostics_without_panicking() {
    for digits in ["", "🔥", "12x0", "-1"] {
        let literal = LiteralValue::Decimal(DecimalLiteral {
            negative: false,
            digits: digits.into(),
            scale: 10,
        });
        assert!(value::literal_value(&literal, &ScalarType::Dec64).is_err());
    }
}
