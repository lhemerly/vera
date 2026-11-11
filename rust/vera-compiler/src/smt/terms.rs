//! Total bit-vector terms and straight-line path conditions used by the verifier.

use num_bigint::BigInt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Sort {
    Bool,
    Bits(u32),
}

impl Sort {
    pub fn smt(self) -> String {
        match self {
            Self::Bool => "Bool".into(),
            Self::Bits(width) => format!("(_ BitVec {width})"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Term {
    pub text: String,
    pub sort: Sort,
}

impl Term {
    pub fn boolean(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            sort: Sort::Bool,
        }
    }

    pub fn bits(text: impl Into<String>, width: u32) -> Self {
        Self {
            text: text.into(),
            sort: Sort::Bits(width),
        }
    }

    pub fn width(&self) -> u32 {
        match self.sort {
            Sort::Bits(width) => width,
            Sort::Bool => 1,
        }
    }
}

pub(super) fn boolean(value: bool) -> String {
    if value { "true" } else { "false" }.into()
}

pub(super) fn bv(value: impl Into<BigInt>, width: u32) -> String {
    let modulus = BigInt::from(1_u8) << width;
    let value = value.into();
    let value = ((value % &modulus) + &modulus) % &modulus;
    format!("(_ bv{} {width})", value.to_str_radix(10))
}

pub(super) fn and(items: impl IntoIterator<Item = String>) -> String {
    let mut out = Vec::new();
    for item in items {
        if item == "false" {
            return "false".into();
        }
        if item != "true" {
            out.push(item);
        }
    }
    match out.len() {
        0 => "true".into(),
        1 => out.pop().unwrap(),
        _ => format!("(and {})", out.join(" ")),
    }
}

pub(super) fn or(items: impl IntoIterator<Item = String>) -> String {
    let mut out = Vec::new();
    for item in items {
        if item == "true" {
            return "true".into();
        }
        if item != "false" {
            out.push(item);
        }
    }
    match out.len() {
        0 => "false".into(),
        1 => out.pop().unwrap(),
        _ => format!("(or {})", out.join(" ")),
    }
}

pub(super) fn not(value: &str) -> String {
    match value {
        "true" => "false".into(),
        "false" => "true".into(),
        _ => format!("(not {value})"),
    }
}

pub(super) fn eq(left: &str, right: &str) -> String {
    if left == right {
        "true".into()
    } else {
        format!("(= {left} {right})")
    }
}

pub(super) fn ite(condition: &str, yes: &str, no: &str) -> String {
    if yes == no {
        return yes.into();
    }
    match condition {
        "true" => yes.into(),
        "false" => no.into(),
        _ => format!("(ite {condition} {yes} {no})"),
    }
}

pub(super) fn implies(condition: &str, value: &str) -> String {
    or([not(condition), value.into()])
}

pub(super) fn extend(value: &str, from: u32, to: u32, signed: bool) -> String {
    if from == to {
        return value.into();
    }
    assert!(from < to, "SMT extension cannot narrow");
    let operation = if signed { "sign_extend" } else { "zero_extend" };
    format!("((_ {operation} {}) {value})", to - from)
}

pub(super) fn truncate(value: &str, from: u32, to: u32) -> String {
    if from == to {
        return value.into();
    }
    assert!(from > to, "SMT truncation cannot widen");
    format!("((_ extract {} 0) {value})", to - 1)
}

pub(super) fn fits(value: &str, width: u32, target: u32, signed: bool) -> String {
    if width == target {
        return "true".into();
    }
    if width < target {
        return "true".into();
    }
    eq(
        value,
        &extend(&truncate(value, width, target), target, width, signed),
    )
}

pub(super) fn cmp(operation: &str, left: &str, right: &str, signed: bool) -> String {
    match operation {
        "eq" => eq(left, right),
        "ne" => not(&eq(left, right)),
        other => {
            let prefix = if signed { "bvs" } else { "bvu" };
            format!("({prefix}{other} {left} {right})")
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Path {
    pub live: String,
    pub trapped: String,
    pub active: String,
}

impl Default for Path {
    fn default() -> Self {
        Self {
            live: "true".into(),
            trapped: "false".into(),
            active: "true".into(),
        }
    }
}

#[derive(Default)]
pub(super) struct Script {
    pub declarations: Vec<String>,
    pub assertions: Vec<String>,
    serial: usize,
}

impl Script {
    pub fn declare(&mut self, name: &str, sort: Sort) -> Term {
        self.declarations
            .push(format!("(declare-const {name} {})", sort.smt()));
        Term {
            text: name.into(),
            sort,
        }
    }

    pub fn assert(&mut self, value: impl Into<String>) {
        self.assertions.push(value.into());
    }

    pub fn bind(&mut self, stem: &str, value: impl Into<String>, sort: Sort) -> Term {
        let name = format!("v_{}_{}", stem, self.serial);
        self.serial += 1;
        let term = self.declare(&name, sort);
        self.assert(eq(&name, &value.into()));
        term
    }

    pub fn guard(&mut self, path: &mut Path, condition: &str) {
        path.live = self
            .bind(
                "path",
                and([path.live.clone(), implies(&path.active, condition)]),
                Sort::Bool,
            )
            .text;
    }

    pub fn trap_if(&mut self, path: &mut Path, failure: &str) {
        let reached = and([path.live.clone(), path.active.clone(), failure.into()]);
        path.trapped = self
            .bind("trap", or([path.trapped.clone(), reached]), Sort::Bool)
            .text;
        self.guard(path, &not(failure));
    }

    pub fn query(&self, assertion: &str, timeout_ms: u32) -> String {
        let mut lines = vec![
            "(set-logic QF_AUFBV)".into(),
            format!("(set-option :timeout {timeout_ms})"),
        ];
        lines.extend(self.declarations.iter().cloned());
        lines.extend(self.assertions.iter().map(|a| format!("(assert {a})")));
        lines.push(format!("(assert {assertion})"));
        lines.join("\n") + "\n"
    }
}

/// A total arithmetic value and its source-language domain predicate.
/// Operands must already have the operation's carrier width.
pub(super) fn arithmetic(
    operation: &str,
    a: &str,
    b: &str,
    width: u32,
    signed: bool,
    scale: u32,
) -> (String, String) {
    let wide = width * 2;
    let aa = extend(a, width, wide, signed);
    let bb = extend(b, width, wide, signed);
    match operation {
        "add" | "sub" => {
            let value = format!("(bv{operation} {aa} {bb})");
            (
                truncate(&value, wide, width),
                fits(&value, wide, width, signed),
            )
        }
        "mul" if scale > 0 => {
            let product = format!("(bvmul {aa} {bb})");
            let divisor = bv(BigInt::from(10_u8).pow(scale), wide);
            let value = format!("(bvsdiv {product} {divisor})");
            let exact = eq(&format!("(bvsrem {product} {divisor})"), &bv(0, wide));
            (
                truncate(&value, wide, width),
                and([exact, fits(&value, wide, width, signed)]),
            )
        }
        "mul" => {
            let value = format!("(bvmul {aa} {bb})");
            (
                truncate(&value, wide, width),
                fits(&value, wide, width, signed),
            )
        }
        "div" if scale > 0 => {
            let factor = bv(BigInt::from(10_u8).pow(scale), wide);
            let numerator = format!("(bvmul {aa} {factor})");
            let value = format!("(bvsdiv {numerator} {bb})");
            let nonzero = not(&eq(b, &bv(0, width)));
            let exact = eq(&format!("(bvsrem {numerator} {bb})"), &bv(0, wide));
            (
                truncate(&value, wide, width),
                and([nonzero, exact, fits(&value, wide, width, signed)]),
            )
        }
        "div" | "rem" => {
            let op = match (operation, signed) {
                ("div", true) => "bvsdiv",
                ("div", false) => "bvudiv",
                ("rem", true) => "bvsrem",
                _ => "bvurem",
            };
            let nonzero = not(&eq(b, &bv(0, width)));
            let signed_overflow = if signed {
                and([
                    eq(a, &bv(-(BigInt::from(1_u8) << (width - 1)), width)),
                    eq(b, &bv(-1, width)),
                ])
            } else {
                "false".into()
            };
            (
                format!("({op} {a} {b})"),
                and([nonzero, not(&signed_overflow)]),
            )
        }
        "min" | "max" => {
            let order = cmp(if operation == "min" { "le" } else { "ge" }, a, b, signed);
            (ite(&order, a, b), "true".into())
        }
        _ => unreachable!("typed arithmetic operation {operation}"),
    }
}
