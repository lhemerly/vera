"""Rust backend with exact scalars, atomic transactions and ordered MVCC execution.

Every transition executes against a private snapshot and publishes its whole
result only after successful completion.  Concurrent speculative executions
never share mutable state; ordered validation/re-execution preserves intent
log semantics for both static and data-dependent footprints.
"""

from __future__ import annotations

import hashlib
import dataclasses
import json
from pathlib import Path
from typing import Dict, Optional

from . import ast_nodes as A
from .errors import VeraError
from .semantic import Program, _TypeChecker, _resolve_type_ref
from .types import BOOL, I64, MAX_PROBES, ScalarType


_RUST_KEYWORDS = set(
    "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while async await dyn abstract become box do final macro override priv typeof unsized virtual yield try gen".split()
)


def _ident(name: str) -> str:
    if name in ("self", "Self", "super", "crate"):
        return "vera_" + name
    return "r#" + name if name in _RUST_KEYWORDS else name


def _const_name(name: str) -> str:
    return "v_" + name.removeprefix("%") if name.startswith("%") else _ident(name)


def _zero_rust(ctype: ScalarType) -> str:
    if ctype.is_bool:
        return "false"
    if ctype.kind == "bytes32":
        return "[0u8; 32]"
    if ctype.kind in ("i256", "u256"):
        return ctype.rust_type + "::ZERO"
    return "0"


def _rust_type(ctype: ScalarType) -> str:
    return ctype.rust_type


_WIDE_RUST = r"""
// Fixed 256-bit integers keep dec128 intermediates and aggregates exact.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(32))]
pub struct U256(pub [u64; 4]);
impl U256 {
    pub const ZERO: Self = Self([0; 4]);
    pub fn from_u128(v: u128) -> Self {
        Self([v as u64, (v >> 64) as u64, 0, 0])
    }
    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
    fn bit(self, i: usize) -> bool {
        (self.0[i / 64] >> (i % 64)) & 1 != 0
    }
    fn set_bit(&mut self, i: usize) {
        self.0[i / 64] |= 1u64 << (i % 64);
    }
    fn wrapping_add(self, r: Self) -> Self {
        let mut out = [0; 4];
        let mut carry = 0u128;
        for i in 0..4 {
            let v = self.0[i] as u128 + r.0[i] as u128 + carry;
            out[i] = v as u64;
            carry = v >> 64;
        }
        Self(out)
    }
    pub fn checked_add(self, r: Self) -> Option<Self> {
        let out = self.wrapping_add(r);
        if out < self {
            None
        } else {
            Some(out)
        }
    }
    fn wrapping_sub(self, r: Self) -> Self {
        let mut out = [0; 4];
        let mut borrow = false;
        for i in 0..4 {
            let (a, b) = self.0[i].overflowing_sub(r.0[i]);
            let (v, c) = a.overflowing_sub(borrow as u64);
            out[i] = v;
            borrow = b || c;
        }
        Self(out)
    }
    pub fn checked_sub(self, r: Self) -> Option<Self> {
        if self < r {
            None
        } else {
            Some(self.wrapping_sub(r))
        }
    }
    pub fn checked_mul(self, r: Self) -> Option<Self> {
        let mut out = [0u64; 8];
        for i in 0..4 {
            let mut carry = 0u128;
            for j in 0..4 {
                let k = i + j;
                let v = self.0[i] as u128 * r.0[j] as u128 + out[k] as u128 + carry;
                out[k] = v as u64;
                carry = v >> 64;
            }
            let mut k = i + 4;
            while carry != 0 && k < 8 {
                let v = out[k] as u128 + carry;
                out[k] = v as u64;
                carry = v >> 64;
                k += 1;
            }
        }
        if out[4..].iter().any(|&x| x != 0) {
            None
        } else {
            Some(Self(out[..4].try_into().unwrap()))
        }
    }
    pub fn div_rem(self, r: Self) -> Option<(Self, Self)> {
        if r.is_zero() {
            return None;
        }
        let mut q = Self::ZERO;
        let mut rem = Self::ZERO;
        for i in (0..256).rev() {
            let carry = rem.0[3] >> 63 != 0;
            for j in (0..4).rev() {
                rem.0[j] = (rem.0[j] << 1)
                    | if j > 0 {
                        rem.0[j - 1] >> 63
                    } else {
                        self.bit(i) as u64
                    };
            }
            if carry || rem >= r {
                rem = rem.wrapping_sub(r);
                q.set_bit(i);
            }
        }
        Some((q, rem))
    }
    pub fn checked_div(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.0)
    }
    pub fn checked_rem(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.1)
    }
    pub fn to_u128(self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            None
        } else {
            Some(self.0[0] as u128 | ((self.0[1] as u128) << 64))
        }
    }
    pub fn to_le_bytes(self) -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, v) in self.0.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    pub fn from_le_bytes(b: [u8; 32]) -> Self {
        let mut w = [0; 4];
        for i in 0..4 {
            w[i] = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        }
        Self(w)
    }
}
impl Ord for U256 {
    fn cmp(&self, r: &Self) -> std::cmp::Ordering {
        for i in (0..4).rev() {
            let c = self.0[i].cmp(&r.0[i]);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        std::cmp::Ordering::Equal
    }
}
impl PartialOrd for U256 {
    fn partial_cmp(&self, r: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(r))
    }
}
impl std::fmt::Display for U256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut n = *self;
        if n.is_zero() {
            return write!(f, "0");
        }
        let mut digits = Vec::new();
        while !n.is_zero() {
            let (q, r) = n.div_rem(Self::from_u128(10)).unwrap();
            digits.push(b'0' + r.0[0] as u8);
            n = q;
        }
        for d in digits.iter().rev() {
            write!(f, "{}", *d as char)?;
        }
        Ok(())
    }
}
impl std::fmt::Debug for U256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct I256(pub U256);
impl I256 {
    pub const ZERO: Self = Self(U256::ZERO);
    pub fn from_i128(v: i128) -> Self {
        Self(U256([
            v as u64,
            (v >> 64) as u64,
            if v < 0 { u64::MAX } else { 0 },
            if v < 0 { u64::MAX } else { 0 },
        ]))
    }
    pub fn from_u128(v: u128) -> Self {
        Self(U256::from_u128(v))
    }
    pub fn negative(self) -> bool {
        self.0 .0[3] >> 63 != 0
    }
    fn magnitude(self) -> U256 {
        if self.negative() {
            U256::ZERO.wrapping_sub(self.0)
        } else {
            self.0
        }
    }
    fn signed_magnitude(m: U256, negative: bool) -> Option<Self> {
        let high = m.0[3];
        if (!negative && high >> 63 != 0)
            || (negative
                && (high > 0x8000000000000000
                    || (high == 0x8000000000000000 && m.0[..3].iter().any(|&x| x != 0))))
        {
            return None;
        }
        Some(Self(if negative {
            U256::ZERO.wrapping_sub(m)
        } else {
            m
        }))
    }
    pub fn checked_add(self, r: Self) -> Option<Self> {
        let v = Self(self.0.wrapping_add(r.0));
        if self.negative() == r.negative() && v.negative() != self.negative() {
            None
        } else {
            Some(v)
        }
    }
    pub fn checked_sub(self, r: Self) -> Option<Self> {
        let v = Self(self.0.wrapping_sub(r.0));
        if self.negative() != r.negative() && v.negative() != self.negative() {
            None
        } else {
            Some(v)
        }
    }
    pub fn checked_neg(self) -> Option<Self> {
        Self::ZERO.checked_sub(self)
    }
    pub fn checked_mul(self, r: Self) -> Option<Self> {
        Self::signed_magnitude(
            self.magnitude().checked_mul(r.magnitude())?,
            self.negative() != r.negative(),
        )
    }
    pub fn div_rem(self, r: Self) -> Option<(Self, Self)> {
        let (q, m) = self.magnitude().div_rem(r.magnitude())?;
        Some((
            Self::signed_magnitude(q, self.negative() != r.negative())?,
            Self::signed_magnitude(m, self.negative())?,
        ))
    }
    pub fn checked_div(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.0)
    }
    pub fn checked_rem(self, r: Self) -> Option<Self> {
        self.div_rem(r).map(|x| x.1)
    }
    pub fn to_i128(self) -> Option<i128> {
        let n = self.magnitude().to_u128()?;
        if self.negative() {
            if n == (1u128 << 127) {
                Some(i128::MIN)
            } else if n <= i128::MAX as u128 {
                Some(-(n as i128))
            } else {
                None
            }
        } else if n <= i128::MAX as u128 {
            Some(n as i128)
        } else {
            None
        }
    }
    pub fn to_u128(self) -> Option<u128> {
        if self.negative() {
            None
        } else {
            self.0.to_u128()
        }
    }
    pub fn to_le_bytes(self) -> [u8; 32] {
        self.0.to_le_bytes()
    }
    pub fn from_le_bytes(b: [u8; 32]) -> Self {
        Self(U256::from_le_bytes(b))
    }
}
impl Ord for I256 {
    fn cmp(&self, r: &Self) -> std::cmp::Ordering {
        if self.negative() != r.negative() {
            if self.negative() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        } else {
            self.0.cmp(&r.0)
        }
    }
}
impl PartialOrd for I256 {
    fn partial_cmp(&self, r: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(r))
    }
}
impl std::fmt::Display for I256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.negative() {
            write!(f, "-")?;
        }
        std::fmt::Display::fmt(&self.magnitude(), f)
    }
}
impl std::fmt::Debug for I256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
fn wide_product(a: U256, b: U256) -> [u64; 8] {
    let mut out = [0u64; 8];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let k = i + j;
            let v = a.0[i] as u128 * b.0[j] as u128 + out[k] as u128 + carry;
            out[k] = v as u64;
            carry = v >> 64;
        }
        let mut k = i + 4;
        while carry != 0 && k < 8 {
            let v = out[k] as u128 + carry;
            out[k] = v as u64;
            carry = v >> 64;
            k += 1;
        }
    }
    out
}
fn wide_quotient(n: [u64; 8], d: U256) -> Option<([u64; 8], U256)> {
    if d.is_zero() {
        return None;
    }
    let mut q = [0u64; 8];
    let mut rem = U256::ZERO;
    for i in (0..512).rev() {
        let carry = rem.0[3] >> 63 != 0;
        for j in (0..4).rev() {
            rem.0[j] = (rem.0[j] << 1)
                | if j > 0 {
                    rem.0[j - 1] >> 63
                } else {
                    (n[i / 64] >> (i % 64)) & 1
                };
        }
        if carry || rem >= d {
            rem = rem.wrapping_sub(d);
            q[i / 64] |= 1u64 << (i % 64);
        }
    }
    Some((q, rem))
}
fn decimal_wide_result(n: [u64; 8], d: U256, negative: bool) -> Result<I256, ErrorCode> {
    let (q, r) = wide_quotient(n, d).ok_or(ErrorCode::ArithOverflow)?;
    if !r.is_zero() || q[4..].iter().any(|&x| x != 0) {
        return Err(ErrorCode::ArithOverflow);
    }
    I256::signed_magnitude(U256(q[..4].try_into().unwrap()), negative)
        .ok_or(ErrorCode::ArithOverflow)
}
fn decimal_wide_mul(a: I256, b: I256, scale: u128) -> Result<I256, ErrorCode> {
    decimal_wide_result(
        wide_product(a.magnitude(), b.magnitude()),
        U256::from_u128(scale),
        a.negative() != b.negative(),
    )
}
fn decimal_wide_div(a: I256, b: I256, scale: u128) -> Result<I256, ErrorCode> {
    decimal_wide_result(
        wide_product(a.magnitude(), U256::from_u128(scale)),
        b.magnitude(),
        a.negative() != b.negative(),
    )
}
fn decimal_mul(a: i128, b: i128, scale: i128) -> Result<i128, ErrorCode> {
    decimal_wide_mul(I256::from_i128(a), I256::from_i128(b), scale as u128)?
        .to_i128()
        .ok_or(ErrorCode::ArithOverflow)
}
fn decimal_div(a: i128, b: i128, scale: i128) -> Result<i128, ErrorCode> {
    decimal_wide_div(I256::from_i128(a), I256::from_i128(b), scale as u128)?
        .to_i128()
        .ok_or(ErrorCode::ArithOverflow)
}
"""


class RustGenerator:
    def __init__(self, prog: Program):
        if prog.diags.has_errors():
            raise VeraError(
                "D_CODEGEN", "cannot generate Rust for a program with semantic errors"
            )
        self.prog = prog
        self.lines = []
        self.type_env: Dict[str, ScalarType] = {}
        self.fresh = 0
        self.table_fields = self._name_map(
            prog.tables,
            lambda n: "table_" + n if n.lower() == "outbox" else _ident(n.lower()),
            "table",
        )
        self.table_constants = self._name_map(prog.tables, lambda n: n.upper(), "TABLE")
        self.transition_variants = self._name_map(
            prog.transitions, self._base_variant, "Intent"
        )
        self.effect_variants = self._name_map(
            prog.effects, self._base_variant, "Effect"
        )
        reserved = {
            "main",
            "run_batch",
            "run_batch_serial",
            "restore",
            "snapshot",
            "mix64",
            "decimal_mul",
            "decimal_div",
            "checksum",
            "replay_file",
            "atomic_write",
            "wide_product",
            "wide_quotient",
            "decimal_wide_result",
            "decimal_wide_mul",
            "decimal_wide_div",
        }
        reserved.update("transition_" + name for name in prog.transitions)
        reserved.update("project_" + name for name in prog.projections)
        for tn, ti in prog.tables.items():
            reserved.update({f"table_exists_{tn}", f"table_alloc_{tn}"})
            for cn in ti.columns:
                reserved.update(
                    {
                        f"table_get_{tn}_{cn}",
                        f"table_set_{tn}_{cn}",
                        f"hash_bucket_{tn}_{cn}",
                        f"hash_lookup_{tn}_{cn}",
                        f"hash_exists_{tn}_{cn}",
                        f"hash_insert_{tn}_{cn}",
                    }
                )
        self.transition_functions = self._name_map(
            prog.transitions,
            lambda n: "vera_transition_" + n if n in reserved else _ident(n),
            "public_transition",
        )
        self.error_codes = [
            "ArithOverflow",
            "MissingRow",
            "TableFull",
            "GenerationExhausted",
            "Precondition",
            "DuplicateKey",
            "IndexSaturated",
            "InvalidEncoding",
        ]
        for node in _walk_stmts(prog.module):
            if isinstance(node, A.Guard) and node.code not in self.error_codes:
                self.error_codes.append(node.code)

    def line(self, text=""):
        self.lines.append(text)

    def _fresh(self, stem):
        self.fresh += 1
        return f"_vera_{stem}_{self.fresh}"

    @staticmethod
    def _name_map(names, base, prefix):
        names = list(names)
        bases = {name: base(name) for name in names}
        used = set(bases.values())
        result = {}
        for name in names:
            if list(bases.values()).count(bases[name]) == 1:
                result[name] = bases[name]
            else:
                candidate = prefix + "_" + name
                while candidate in used:
                    candidate += "_"
                result[name] = candidate
                used.add(candidate)
        return result

    def _field(self, table):
        return self.table_fields[table]

    @staticmethod
    def _base_variant(name):
        return (
            "V_" + name
            if name == "Self"
            else "".join(p[:1].upper() + p[1:] for p in name.split("_"))
        )

    def _constant(self, table):
        return self.table_constants[table]

    def _function(self, transition):
        return self.transition_functions[transition]

    def _variant(self, name):
        return self.transition_variants.get(name, self._base_variant(name))

    def _program_fingerprint(self):
        def canonical(value):
            if dataclasses.is_dataclass(value):
                return {
                    "node": type(value).__name__,
                    **{
                        f.name: canonical(getattr(value, f.name))
                        for f in dataclasses.fields(value)
                        if f.name not in ("line", "col")
                    },
                }
            if isinstance(value, (list, tuple)):
                return [canonical(x) for x in value]
            if isinstance(value, bytes):
                return {"bytes": value.hex()}
            if isinstance(value, (str, int, float, bool)) or value is None:
                return value
            return {"value": str(value)}

        data = {"language": "VERA-1.1.1", "ast": canonical(self.prog.module)}
        return list(hashlib.sha256(json.dumps(data, sort_keys=True).encode()).digest())

    def _param_field(self, name, params):
        return self._name_map([p.name for p in params], _ident, "param")[name]

    def _schema(self):
        schema = {
            "tables": [
                (
                    t,
                    ti.layout.capacity,
                    [
                        (c, str(ci.type), ci.tracked_sum, ci.unique)
                        for c, ci in ti.columns.items()
                    ],
                )
                for t, ti in self.prog.tables.items()
            ],
            "effects": [
                (n, [(p.name, str(self._resolved(p.type))) for p in e.params])
                for n, e in self.prog.effects.items()
            ],
        }
        return list(
            hashlib.sha256(json.dumps(schema, sort_keys=True).encode()).digest()
        )

    def emit(self, use_rt=False, include_main=True):
        self.line(
            "// Generated by VERA 1.1.0. Atomic snapshots; checked arithmetic; canonical codecs."
        )
        self.line(
            "#![allow(non_camel_case_types, non_snake_case, dead_code, unused_variables, unused_mut, unused_parens, clippy::all)]"
        )
        self.line("use std::io::Write;")
        self._emit_constants()
        self.line("#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum ErrorCode {")
        for code in self.error_codes:
            self.line(f"    {_ident(code)},")
        self.line("}")
        self.line(_WIDE_RUST)
        self._emit_codec_helpers()
        self._emit_effects()
        for table in self.prog.tables:
            self._emit_table(table)
        self._emit_accessors()
        self._emit_state()
        for transition in self.prog.transitions:
            self._emit_transition(transition)
        self._emit_runtime(use_rt)
        self._emit_snapshot()
        self._emit_projections()
        if include_main:
            self._emit_demo_main()
        return "\n".join(self.lines) + "\n"

    def _emit_constants(self):
        for tname, ti in self.prog.tables.items():
            self.line(
                f"pub const CAP_{self._constant(tname)}: usize = {ti.layout.capacity};"
            )
            self.line(
                f"pub const KEY_BITS_{self._constant(tname)}: u32 = {ti.layout.key_bits};"
            )
            self.line(
                f"pub const GEN_BITS_{self._constant(tname)}: u32 = {ti.layout.generation_bits};"
            )
        self.line("pub const STATE_SCHEMA: [u8;32] = " + repr(self._schema()) + ";")
        self.line(
            "pub const PROGRAM_FINGERPRINT: [u8;32] = "
            + repr(self._program_fingerprint())
            + ";"
        )

    def _emit_codec_helpers(self):
        self.line(r"""
struct Reader<'a> { data:&'a [u8], pos:usize }
impl<'a> Reader<'a> {
    fn new(data:&'a [u8])->Self {Self{data,pos:0}}
    fn take<const N:usize>(&mut self)->Result<[u8;N],ErrorCode>{let end=self.pos.checked_add(N).ok_or(ErrorCode::InvalidEncoding)?;let b=self.data.get(self.pos..end).ok_or(ErrorCode::InvalidEncoding)?;self.pos=end;Ok(b.try_into().unwrap())}
    fn boolean(&mut self)->Result<bool,ErrorCode>{match self.take::<1>()?[0]{0=>Ok(false),1=>Ok(true),_=>Err(ErrorCode::InvalidEncoding)}}
    fn done(&self)->Result<(),ErrorCode>{if self.pos==self.data.len(){Ok(())}else{Err(ErrorCode::InvalidEncoding)}}
}
fn mix64(z:u64)->u64{let z=z^(z>>30);let z=z.wrapping_mul(0xBF58476D1CE4E5B9);let z=z^(z>>27);let z=z.wrapping_mul(0x94D049BB133111EB);z^(z>>31)}
""")

    def _resolved(self, ref):
        t = _resolve_type_ref(self.prog, ref, "Rust code generation")
        if t is None:
            raise VeraError("D_CODEGEN", "unresolved type in Rust generation")
        return t

    def _emit_effects(self):
        self.line("#[derive(Clone, Debug, PartialEq, Eq)] pub enum Effect {")
        for name, eff in self.prog.effects.items():
            fields = ", ".join(
                f"{self._param_field(p.name, eff.params)}: {_rust_type(self._resolved(p.type))}"
                for p in eff.params
            )
            self.line(f"    {self.effect_variants[name]} {{ {fields} }},")
        self.line("}")

    def _agg_rust(self, ctype):
        return "I256" if ctype.width > 64 else ("i128" if ctype.is_signed else "u128")

    def _agg_zero(self, ctype):
        return "I256::ZERO" if ctype.width > 64 else "0"

    def _agg_cast(self, value, ctype):
        if ctype.width > 64:
            return f"I256::from_i128({value})"
        return f"({value} as {self._agg_rust(ctype)})"

    def _layout_fields(self, tname):
        ti = self.prog.tables[tname]
        fields = []
        for idx, f in enumerate(ti.layout.fields):
            if f.pad_before:
                fields.append(
                    (f"_pad{idx}", f"[u8; {f.pad_before}]", f"[0; {f.pad_before}]")
                )
            if f.name == "occupied_bitmap":
                n = (ti.layout.capacity + 63) // 64
                rt, init = f"[u64; {n}]", f"[0; {n}]"
            elif f.name == "generation":
                rt, init = f"[u32; {ti.layout.capacity}]", f"[0; {ti.layout.capacity}]"
            elif f.name.startswith("col_"):
                ci = ti.columns[f.name[4:]]
                rt, init = (
                    f"[{_rust_type(ci.type)}; {ti.layout.capacity}]",
                    f"[{_zero_rust(ci.type)}; {ti.layout.capacity}]",
                )
            elif f.name.startswith("agg_sum_"):
                ci = ti.columns[f.name[8:]]
                rt, init = self._agg_rust(ci.type), self._agg_zero(ci.type)
            elif f.name.startswith("hash_") and f.name.endswith("_key"):
                n = ti.layout.unique_hashes[f.name[5:-4]]
                rt, init = f"[u64; {n}]", f"[0; {n}]"
            elif f.name.startswith("hash_") and f.name.endswith("_used"):
                n = (ti.layout.unique_hashes[f.name[5:-5]] + 63) // 64
                rt, init = f"[u64; {n}]", f"[0; {n}]"
            else:
                raise VeraError("D_CODEGEN", f"unknown physical layout field {f.name}")
            fields.append((f.name, rt, init))
        if ti.layout.fields:
            last = ti.layout.fields[-1]
            tail = ti.layout.size - last.offset - last.size
            if tail:
                fields.append(("_pad_end", f"[u8; {tail}]", f"[0; {tail}]"))
        return fields

    def _emit_table(self, tname):
        self.line("#[derive(Clone, Debug, PartialEq, Eq)] #[repr(C, align(64))]")
        self.line(f"pub struct Table_{tname} {{")
        for name, rt, init in self._layout_fields(tname):
            self.line(f"    pub {name}: {rt},")
        self.line("}")

    def _emit_accessors(self):
        for tn, ti in self.prog.tables.items():
            cap, kb, gb = (
                ti.layout.capacity,
                ti.layout.key_bits,
                ti.layout.generation_bits,
            )
            high_check = f"key>>{kb + gb}==0 && " if kb + gb < 64 else ""
            self.line(
                f"fn table_exists_{tn}(t:&Table_{tn},key:u64)->bool {{let slot=(key&{cap - 1}) as usize; {high_check}(t.occupied_bitmap[slot/64]>>(slot%64))&1==1 && t.generation[slot]==(key>>{kb}) as u32}}"
            )
            for cn, ci in ti.columns.items():
                rt = _rust_type(ci.type)
                self.line(
                    f"fn table_get_{tn}_{cn}(t:&Table_{tn},key:u64)->Result<{rt},ErrorCode>{{if !table_exists_{tn}(t,key){{return Err(ErrorCode::MissingRow);}}Ok(t.col_{cn}[(key&{cap - 1}) as usize])}}"
                )
                self.line(
                    f"fn table_set_{tn}_{cn}(t:&mut Table_{tn},key:u64,value:{rt})->Result<(),ErrorCode>{{if !table_exists_{tn}(t,key){{return Err(ErrorCode::MissingRow);}}let slot=(key&{cap - 1}) as usize;let old=t.col_{cn}[slot];"
                )
                if ci.tracked_sum:
                    self.line(
                        f"    t.agg_sum_{cn}=t.agg_sum_{cn}.checked_sub({self._agg_cast('old', ci.type)}).and_then(|n|n.checked_add({self._agg_cast('value', ci.type)})).ok_or(ErrorCode::ArithOverflow)?;"
                    )
                self.line(f"    t.col_{cn}[slot]=value;Ok(())}}")
                if ci.unique:
                    self._emit_hash(tn, cn, ci)
            fields = ", ".join(
                f"{_ident(cn)}:{_rust_type(ci.type)}" for cn, ci in ti.columns.items()
            )
            self.line(
                f"fn table_alloc_{tn}(t:&mut Table_{tn}{', ' + fields if fields else ''})->Result<u64,ErrorCode>{{"
            )
            self.line(
                f"    let slot=(0..{cap}).find(|&i|(t.occupied_bitmap[i/64]>>(i%64))&1==0).ok_or(ErrorCode::TableFull)?;"
            )
            self.line(
                f"    let gen=t.generation[slot];if gen>={(1 << gb) - 1}u32{{return Err(ErrorCode::GenerationExhausted);}}"
            )
            for cn, ci in ti.columns.items():
                if ci.tracked_sum:
                    self.line(
                        f"    t.agg_sum_{cn}=t.agg_sum_{cn}.checked_add({self._agg_cast(_ident(cn), ci.type)}).ok_or(ErrorCode::ArithOverflow)?;"
                    )
                self.line(f"    t.col_{cn}[slot]={_ident(cn)};")
            self.line(
                "    t.generation[slot]=gen+1;t.occupied_bitmap[slot/64]|=1u64<<(slot%64);"
            )
            for cn, ci in ti.columns.items():
                if ci.unique:
                    self.line(f"    hash_insert_{tn}_{cn}(t,&{_ident(cn)},slot)?;")
            self.line(f"    Ok((((gen+1) as u64)<<{kb})|slot as u64)}}")

    def _emit_hash(self, tn, cn, ci):
        ti = self.prog.tables[tn]
        B = ti.layout.unique_hashes[cn]
        rt = _rust_type(ci.type)
        if ci.type.kind == "bytes32":
            hash_expr = "u64::from_le_bytes(value[..8].try_into().unwrap())"
        else:
            hash_expr = "*value as u64"
        self.line(
            f"fn hash_bucket_{tn}_{cn}(value:&{rt})->usize{{(mix64({hash_expr})&{B - 1}) as usize}}"
        )
        self.line(
            f"fn hash_lookup_{tn}_{cn}(t:&Table_{tn},value:&{rt})->Option<usize>{{let mut pos=hash_bucket_{tn}_{cn}(value);for _ in 0..{MAX_PROBES}{{if (t.hash_{cn}_used[pos/64]>>(pos%64))&1==0{{return None;}}let slot=t.hash_{cn}_key[pos] as usize;if slot<{ti.layout.capacity}&&(t.occupied_bitmap[slot/64]>>(slot%64))&1==1&&t.col_{cn}[slot]==*value{{return Some(slot);}}pos=(pos+1)&{B - 1};}}None}}"
        )
        self.line(
            f"fn hash_exists_{tn}_{cn}(t:&Table_{tn},value:&{rt})->bool{{hash_lookup_{tn}_{cn}(t,value).is_some()}}"
        )
        self.line(
            f"fn hash_insert_{tn}_{cn}(t:&mut Table_{tn},value:&{rt},slot:usize)->Result<(),ErrorCode>{{"
        )
        self.line(
            f"    if hash_exists_{tn}_{cn}(t,value){{return Err(ErrorCode::DuplicateKey);}}let mut incoming=slot;let mut pos=hash_bucket_{tn}_{cn}(value);let mut distance=0usize;"
        )
        self.line(
            f"    for _ in 0..{MAX_PROBES}{{if (t.hash_{cn}_used[pos/64]>>(pos%64))&1==0{{t.hash_{cn}_used[pos/64]|=1u64<<(pos%64);t.hash_{cn}_key[pos]=incoming as u64;return Ok(());}}"
        )
        self.line(
            f"        let resident=t.hash_{cn}_key[pos] as usize;let home=hash_bucket_{tn}_{cn}(&t.col_{cn}[resident]);let resident_distance=(pos+{B}-home)&{B - 1};if resident_distance<distance{{t.hash_{cn}_key[pos]=incoming as u64;incoming=resident;distance=resident_distance;}}pos=(pos+1)&{B - 1};distance+=1;}}Err(ErrorCode::IndexSaturated)}}"
        )

    def _emit_state(self):
        self.line("#[derive(Clone, Debug, PartialEq, Eq)] pub struct State {")
        for tn in self.prog.tables:
            self.line(f"    pub {self._field(tn)}: Table_{tn},")
        self.line("    pub outbox: Vec<Effect>,\n}")
        self.line("impl Default for State {fn default()->Self{Self::new()}}")
        self.line("impl State {pub fn new()->Self {Self {")
        for tn in self.prog.tables:
            self.line(f"    {self._field(tn)}: Table_{tn}{{")
            for name, rt, init in self._layout_fields(tn):
                self.line(f"        {name}:{init},")
            self.line("    },")
        self.line("    outbox:Vec::new(),\n}}}")

    def _emit_transition(self, name):
        ti = self.prog.transitions[name]
        params = ", ".join(
            f"p_{p.name}:{_rust_type(ti.param_types[p.name])}" for p in ti.decl.params
        )
        suffix = ", " + params if params else ""
        args = ", ".join("p_" + p.name for p in ti.decl.params)
        checks = "".join(
            f"if (p_{p.name} as usize)>={len(ti.param_types[p.name].enum_tags)}{{return Err(ErrorCode::InvalidEncoding);}}"
            for p in ti.decl.params
            if ti.param_types[p.name].kind == "enum"
        )
        self.line(
            f"pub fn {self._function(name)}(state:&mut State{suffix})->Result<(),ErrorCode>{{{checks}let mut staged=state.clone();let outcome=transition_{name}(&mut staged,state{', ' + args if args else ''});if outcome.is_ok(){{*state=staged;}}outcome}}"
        )
        self.line(
            f"fn transition_{name}(state:&mut State,pre:&State{suffix})->Result<(),ErrorCode>{{"
        )
        self.type_env = dict(ti.param_types)
        env = {p.name: "p_" + p.name for p in ti.decl.params}
        for _, req in ti.decl.requires:
            self.line(
                f"    if !({self._expr(req, env, 'pre', BOOL)}){{return Err(ErrorCode::Precondition);}}"
            )
        for stmt in ti.decl.body:
            self._stmt(stmt, env)
        self.line("    Ok(())\n}")

    def _stmt(self, stmt, env, state="post"):
        if isinstance(stmt, A.Let):
            ct = self._resolved(stmt.type)
            value = self._expr(stmt.value, env, state, ct)
            if self._has_agg(stmt.value):
                value = self._narrow_wide(value, ct)
            name = _const_name(stmt.name)
            self.line(f"    let {name}:{_rust_type(ct)}={value};")
            env[stmt.name] = name
            self.type_env[stmt.name] = ct
        elif isinstance(stmt, A.Guard):
            self.line(
                f"    if !({self._expr(stmt.cond, env, state, BOOL)}){{return Err(ErrorCode::{_ident(stmt.code)});}}"
            )
        elif isinstance(stmt, A.TableSet):
            ct = self.prog.tables[stmt.table].columns[stmt.column].type
            value = self._expr(stmt.value, env, state, ct)
            if self._has_agg(stmt.value):
                value = self._narrow_wide(value, ct)
            key = self._expr(stmt.key, env, state)
            kt, vt = self._fresh("key"), self._fresh("value")
            self.line(
                f"    let {kt}={key};let {vt}={value};table_set_{stmt.table}_{stmt.column}(&mut state.{self._field(stmt.table)},{kt},{vt})?;"
            )
        elif isinstance(stmt, A.TableAlloc):
            ti = self.prog.tables[stmt.table]
            fields = dict(stmt.fields)
            field_temps = {}
            for cn, expression in stmt.fields:
                ci = ti.columns[cn]
                value = self._expr(expression, env, state, ci.type)
                if self._has_agg(expression):
                    value = self._narrow_wide(value, ci.type)
                temp = self._fresh("field")
                self.line(f"    let {temp}:{_rust_type(ci.type)}={value};")
                field_temps[cn] = temp
            temps = [field_temps[cn] for cn in ti.columns]
            name = _const_name(stmt.name)
            self.line(
                f"    let {name}=table_alloc_{stmt.table}(&mut state.{self._field(stmt.table)}{', ' + ', '.join(temps) if temps else ''})?;"
            )
            env[stmt.name] = name
            self.type_env[stmt.name] = ti.key_type
        elif isinstance(stmt, A.Emit):
            fields = []
            for arg, param in zip(stmt.args, self.prog.effects[stmt.effect].params):
                ct = self._resolved(param.type)
                v = self._expr(arg, env, state, ct)
                if self._has_agg(arg):
                    v = self._narrow_wide(v, ct)
                temp = self._fresh("effect")
                self.line(f"    let {temp}:{_rust_type(ct)}={v};")
                fields.append(
                    f"{self._param_field(param.name, self.prog.effects[stmt.effect].params)}:{temp}"
                )
            self.line(
                f"    state.outbox.push(Effect::{self.effect_variants[stmt.effect]}{{{', '.join(fields)}}});"
            )
        elif isinstance(stmt, A.Yield):
            self.line(f"    let _={self._expr(stmt.value, env, state)};")
        elif isinstance(stmt, A.Commit):
            pass
        else:
            raise VeraError(
                "D_CODEGEN",
                f"unsupported Rust statement {type(stmt).__name__}",
                stmt.line,
                stmt.col,
            )

    def _type(self, expr, expected=None):
        inferred = getattr(self.prog, "expr_types", {}).get(id(expr))
        if expected is not None and isinstance(expr, A.Literal):
            return expected
        if inferred is not None:
            return inferred
        return _TypeChecker(self.prog, dict(self.type_env)).type_of(expr, expected)

    def _has_agg(self, expr):
        if isinstance(expr, A.SumAgg):
            return True
        if isinstance(expr, A.Binary):
            return expr.op not in (
                "eq",
                "ne",
                "lt",
                "le",
                "gt",
                "ge",
                "and",
                "or",
                "xor",
            ) and (self._has_agg(expr.left) or self._has_agg(expr.right))
        if isinstance(expr, A.Unary):
            return expr.op != "not" and self._has_agg(expr.operand)
        if isinstance(expr, A.Select):
            return self._has_agg(expr.a) or self._has_agg(expr.b)
        return False

    def _aggregate_width(self, expr):
        if isinstance(expr, A.SumAgg):
            return (
                256
                if self.prog.tables[expr.table].columns[expr.column].type.width > 64
                else 128
            )
        if isinstance(expr, A.Binary):
            return max(
                self._aggregate_width(expr.left), self._aggregate_width(expr.right)
            )
        if isinstance(expr, A.Unary):
            return self._aggregate_width(expr.operand)
        if isinstance(expr, A.Select):
            return max(self._aggregate_width(expr.a), self._aggregate_width(expr.b))
        return 0

    def _check_carrier(self, value, expr, ctype):
        if self._aggregate_width(expr) == 128:
            native = "i128" if ctype.is_signed else "u128"
            return f"{{let v={value};I256::from_{native}(v.to_{native}().ok_or(ErrorCode::ArithOverflow)?)}}"
        return value

    def _wide(self, expr, env, state, ctype):
        value = self._expr(expr, env, state, ctype)
        if self._has_agg(expr):
            return value
        return f"I256::from_{'i128' if ctype.is_signed else 'u128'}(({value}) as {'i128' if ctype.is_signed else 'u128'})"

    def _narrow_wide(self, value, ct):
        base = f"({value}).to_{'i128' if ct.is_signed else 'u128'}().ok_or(ErrorCode::ArithOverflow)?"
        return (
            f"{_rust_type(ct)}::try_from({base}).map_err(|_|ErrorCode::ArithOverflow)?"
        )

    def _expr(self, expr, env, state, expected: Optional[ScalarType] = None):
        current = "pre" if state == "pre" else "state"
        if isinstance(expr, A.Literal):
            return self._literal(expr, self._type(expr, expected))
        if isinstance(expr, A.Var):
            if expr.name in env:
                return env[expr.name]
            ct = self._type(expr, expected)
            if ct.kind == "enum" and expr.name in ct.enum_tags:
                return f"{ct.enum_tags.index(expr.name)}u8"
            raise VeraError(
                "D_CODEGEN", f"unknown Rust variable {expr.name}", expr.line, expr.col
            )
        if isinstance(expr, (A.TableGet, A.TableAccess)):
            key = self._expr(expr.key, env, state)
            temp = self._fresh("read_key")
            if isinstance(expr, A.TableAccess):
                cap = self.prog.tables[expr.table].layout.capacity
                return f"{{let {temp}={key};{current}.{self._field(expr.table)}.col_{expr.column}[({temp}&{cap - 1}) as usize]}}"
            return f"{{let {temp}={key};table_get_{expr.table}_{expr.column}(&{current}.{self._field(expr.table)},{temp})?}}"
        if isinstance(expr, A.TableExists):
            temp = self._fresh("exists_key")
            return f"{{let {temp}={self._expr(expr.key, env, state)};table_exists_{expr.table}(&{current}.{self._field(expr.table)},{temp})}}"
        if isinstance(expr, (A.TableLookup, A.TableExistsUnique)):
            ct = self.prog.tables[expr.table].columns[expr.key_column].type
            k = self._expr(expr.key, env, state, ct)
            temp = self._fresh("unique_value")
            lookup = f"hash_lookup_{expr.table}_{expr.key_column}(&{current}.{self._field(expr.table)},&{temp})"
            if isinstance(expr, A.TableExistsUnique):
                return f"{{let {temp}:{_rust_type(ct)}={k};{lookup}.is_some()}}"
            kb = self.prog.tables[expr.table].layout.key_bits
            return f"{{let {temp}:{_rust_type(ct)}={k};let slot={lookup}.ok_or(ErrorCode::MissingRow)?;(({current}.{self._field(expr.table)}.generation[slot] as u64)<<{kb})|slot as u64}}"
        if isinstance(expr, A.Old):
            cap = self.prog.tables[expr.access.table].layout.capacity
            key = self._expr(expr.access.key, env, "pre")
            return f"pre.{self._field(expr.access.table)}.col_{expr.access.column}[(({key})&{cap - 1}) as usize]"
        if isinstance(expr, A.SumAgg):
            ct = self.prog.tables[expr.table].columns[expr.column].type
            obj = "pre" if expr.old or state == "pre" else "state"
            value = f"{obj}.{self._field(expr.table)}.agg_sum_{expr.column}"
            if ct.width > 64:
                return value
            return f"I256::from_{'i128' if ct.is_signed else 'u128'}({value})"
        if isinstance(expr, A.KeyOf):
            value = self._expr(expr.expr, env, state)
            return f"(({value}) as u64)"
        if isinstance(expr, A.Select):
            ct = self._type(expr, expected)
            a = (
                self._wide(expr.a, env, state, ct)
                if self._has_agg(expr)
                else self._expr(expr.a, env, state, ct)
            )
            b = (
                self._wide(expr.b, env, state, ct)
                if self._has_agg(expr)
                else self._expr(expr.b, env, state, ct)
            )
            return f"(if {self._expr(expr.cond, env, state, BOOL)}{{{a}}}else{{{b}}})"
        if isinstance(expr, A.Unary):
            ct = self._type(expr, expected)
            v = self._expr(expr.operand, env, state, ct)
            if expr.op == "not":
                return f"!({v})"
            if expr.op == "neg":
                result = f"({v}).checked_neg().ok_or(ErrorCode::ArithOverflow)?"
                return (
                    self._check_carrier(result, expr, ct)
                    if self._has_agg(expr)
                    else result
                )
        if isinstance(expr, A.Binary):
            op = expr.op
            if op in ("and", "or", "xor"):
                a = self._expr(expr.left, env, state, BOOL)
                b = self._expr(expr.right, env, state, BOOL)
                sym = {"and": "&", "or": "|", "xor": "^"}[op]
                return f"(({a}){sym}({b}))"
            comparison = op in ("eq", "ne", "lt", "le", "gt", "ge")
            ct = (
                _TypeChecker(self.prog, dict(self.type_env))._infer_equality_target(
                    expr, None
                )
                if comparison
                else self._type(expr, expected)
            )
            if ct is None:
                ct = self._type(expr.left)
            wide = self._has_agg(expr.left) or self._has_agg(expr.right)
            a = (
                self._wide(expr.left, env, state, ct)
                if wide
                else self._expr(expr.left, env, state, ct)
            )
            b = (
                self._wide(expr.right, env, state, ct)
                if wide
                else self._expr(expr.right, env, state, ct)
            )
            at, bt = self._fresh("a"), self._fresh("b")
            rt = "I256" if wide else _rust_type(ct)
            prefix = f"{{let {at}:{rt}={a};let {bt}:{rt}={b};"
            if comparison:
                sym = {
                    "eq": "==",
                    "ne": "!=",
                    "lt": "<",
                    "le": "<=",
                    "gt": ">",
                    "ge": ">=",
                }[op]
                return prefix + f"{at}{sym}{bt}}}"
            if op in ("min", "max"):
                return prefix + f"std::cmp::{op}({at},{bt})}}"
            if op in ("add", "sub", "mul", "div", "rem"):
                if ct.is_decimal and op in ("mul", "div"):
                    if wide:
                        calc = f"decimal_wide_{op}({at},{bt},{10**ct.scale}u128)?"
                        result = prefix + calc + "}"
                        return self._check_carrier(result, expr, ct)
                    calc = (
                        f"decimal_{op}({at} as i128,{bt} as i128,{10**ct.scale}i128)?"
                    )
                    return (
                        prefix
                        + f"{rt}::try_from({calc}).map_err(|_|ErrorCode::ArithOverflow)?}}"
                    )
                result = (
                    prefix
                    + f"{at}.checked_{op}({bt}).ok_or(ErrorCode::ArithOverflow)?}}"
                )
                return self._check_carrier(result, expr, ct) if wide else result
        if isinstance(expr, A.Fold):
            return self._fold_expr(expr, env, state)
        raise VeraError(
            "D_CODEGEN",
            f"unsupported Rust expression {type(expr).__name__}",
            getattr(expr, "line", 0),
            getattr(expr, "col", 0),
        )

    def _literal(self, expr, ct):
        from .types import literal_value

        value = getattr(self.prog, "literal_values", {}).get(id(expr), None)
        if value is None:
            value = literal_value(expr.value, ct)
        if ct.is_bool:
            return "true" if value else "false"
        if ct.kind == "bytes32":
            return "[" + ",".join(f"{x}u8" for x in value) + "]"
        return str(value) + _rust_type(ct)

    def _fold_expr(self, fold, env, state):
        saved_lines, saved_types = self.lines, dict(self.type_env)
        self.lines = []
        acc, idx, hi = self._fresh("acc"), self._fresh("idx"), self._fresh("hi")
        ct = self._resolved(fold.acc_type)
        it = self._type(fold.lo)
        self.line("{")
        init = self._expr(fold.init, env, state, ct)
        if self._has_agg(fold.init):
            init = self._narrow_wide(init, ct)
        self.line(f"let mut {acc}:{_rust_type(ct)}={init};")
        lo_value = self._expr(fold.lo, env, state, it)
        hi_value = self._expr(fold.hi, env, state, it)
        if self._has_agg(fold.lo):
            lo_value = self._narrow_wide(lo_value, it)
        if self._has_agg(fold.hi):
            hi_value = self._narrow_wide(hi_value, it)
        self.line(
            f"let mut {idx}:{_rust_type(it)}={lo_value};let {hi}:{_rust_type(it)}={hi_value};"
        )
        inner = dict(env)
        inner[fold.acc_name], inner[fold.index_name] = acc, idx
        self.type_env[fold.acc_name], self.type_env[fold.index_name] = ct, it
        self.line(f"for _ in 0..{fold.n}{{if {idx}>{hi}{{break;}}")
        for stmt in fold.body:
            self._stmt(stmt, inner, state)
        value = self._expr(fold.yield_expr, inner, state, ct)
        if self._has_agg(fold.yield_expr):
            value = self._narrow_wide(value, ct)
        self.line(f"{acc}={value};")
        self.line(f"match {idx}.checked_add(1){{Some(next)=>{idx}=next,None=>break,}}")
        self.line("}")
        self.line(acc + "}")
        result = "\n".join(self.lines)
        self.lines, self.type_env = saved_lines, saved_types
        return result

    def _encode_scalar(self, value, ct, out="out"):
        if ct.is_bool:
            return f"{out}.push(({value}) as u8);"
        if ct.kind == "bytes32":
            return f"{out}.extend_from_slice(&({value}));"
        return f"{out}.extend_from_slice(&({value}).to_le_bytes());"

    def _decode_scalar(self, ct, reader="r"):
        if ct.is_bool:
            return f"{reader}.boolean()?"
        if ct.kind == "bytes32":
            return f"{reader}.take::<32>()?"
        rt = _rust_type(ct)
        val = f"{rt}::from_le_bytes({reader}.take::<{ct.size}>()?)"
        if ct.kind == "enum":
            return f"{{let v={val};if (v as usize)>={len(ct.enum_tags)}{{return Err(ErrorCode::InvalidEncoding);}}v}}"
        return val

    def _projection_value(self, expr, env, ctype=None):
        ct = ctype or self._type(expr)
        value = self._expr(expr, env, "post", ct)
        if ct.is_bool:
            return f"ProjectionValue::Bool({value})"
        if ct.kind == "bytes32":
            return f"ProjectionValue::Bytes32({value})"
        if self._has_agg(expr):
            return f"ProjectionValue::Integer({value})"
        return f"ProjectionValue::Integer(I256::from_{'i128' if ct.is_signed else 'u128'}(({value}) as {'i128' if ct.is_signed else 'u128'}))"

    def _free_vars(self, expr):
        if isinstance(expr, A.Var):
            ct = getattr(self.prog, "expr_types", {}).get(id(expr))
            return (
                set()
                if ct is not None and ct.kind == "enum" and expr.name in ct.enum_tags
                else {expr.name}
            )
        if dataclasses.is_dataclass(expr):
            out = set()
            for f in dataclasses.fields(expr):
                v = getattr(expr, f.name)
                if dataclasses.is_dataclass(v):
                    out.update(self._free_vars(v))
                elif isinstance(v, list):
                    for item in v:
                        if dataclasses.is_dataclass(item):
                            out.update(self._free_vars(item))
            return out
        return set()

    def _projection_bounds(self, expr, transition, env):
        if not isinstance(expr, A.Binary):
            return
        if expr.op == "and":
            self._projection_bounds(expr.left, transition, env)
            self._projection_bounds(expr.right, transition, env)
            return
        if expr.op not in ("ge", "gt", "le", "lt", "eq"):
            return
        ti = self.prog.transitions[transition]
        pname = None
        value_expr = None
        key = None
        offset = 0
        if (
            isinstance(expr.left, A.Var)
            and expr.left.name in ti.param_types
            and expr.left.name not in self._free_vars(expr.right)
        ):
            pname, value_expr = expr.left.name, expr.right
            key = {"ge": "min", "gt": "min", "le": "max", "lt": "max", "eq": "exact"}[
                expr.op
            ]
            offset = {"gt": 1, "lt": -1}.get(expr.op, 0)
        elif (
            isinstance(expr.right, A.Var)
            and expr.right.name in ti.param_types
            and expr.right.name not in self._free_vars(expr.left)
        ):
            pname, value_expr = expr.right.name, expr.left
            key = {"ge": "max", "gt": "max", "le": "min", "lt": "min", "eq": "exact"}[
                expr.op
            ]
            offset = {"gt": -1, "lt": 1}.get(expr.op, 0)
        if pname is None or not self._free_vars(value_expr) <= set(env):
            return
        value = self._projection_value(value_expr, env, ti.param_types[pname])
        self.line(
            f"if let Ok(value)=(||->Result<ProjectionValue,ErrorCode>{{Ok({value})}})(){{if let Some(value)=value.offset({offset}){{projection_bound(&mut bounds,{json.dumps(pname)},{json.dumps(key)},value);}}}}"
        )

    def _emit_projections(self):
        self.line(r"""
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionValue { Bool(bool), Integer(I256), Bytes32([u8;32]) }
impl ProjectionValue {fn offset(self,n:i128)->Option<Self>{match self{Self::Integer(v)=>v.checked_add(I256::from_i128(n)).map(Self::Integer),v if n==0=>Some(v),_=>None}}}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionCell {pub table:&'static str,pub column:&'static str,pub requested:Option<u64>,pub id:u64,pub live:bool,pub value:Option<ProjectionValue>}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterBound {pub parameter:&'static str,pub min:Option<ProjectionValue>,pub max:Option<ProjectionValue>,pub exact:Option<ProjectionValue>}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Affordance {pub transition:&'static str,pub enabled:bool,pub bounds:Vec<ParameterBound>}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionFrame {pub projection:&'static str,pub allowed:bool,pub reason:&'static str,pub cells:Vec<ProjectionCell>,pub affordances:Vec<Affordance>}
fn projection_bound(bounds:&mut[ParameterBound],parameter:&str,kind:&str,value:ProjectionValue){if let Some(b)=bounds.iter_mut().find(|b|b.parameter==parameter){match kind{"min"=>if b.min.as_ref().map_or(true,|v|*v<value){b.min=Some(value)},"max"=>if b.max.as_ref().map_or(true,|v|*v>value){b.max=Some(value)},"exact"=>b.exact=Some(value),_=>{}}}}
""")
        for name, pi in self.prog.projections.items():
            self.type_env = dict(pi.param_types)
            env = {p.name: "p_" + p.name for p in pi.decl.params}
            params = ",".join(
                f"p_{p.name}:{_rust_type(pi.param_types[p.name])}"
                for p in pi.decl.params
            )
            self.line(
                f'pub fn project_{name}(state:&State{"," + params if params else ""})->Result<ProjectionFrame,ErrorCode>{{let pre=state;let mut frame=ProjectionFrame{{projection:{json.dumps(name)},allowed:false,reason:"",cells:Vec::new(),affordances:Vec::new()}};'
            )
            if pi.decl.allow_if is not None:
                cond = self._expr(pi.decl.allow_if, env, "post", BOOL)
                self.line(
                    f'if !(||->Result<bool,ErrorCode>{{Ok({cond})}})().unwrap_or(false){{frame.reason="allow_if evaluated to false";return Ok(frame);}}'
                )
            self.line("frame.allowed=true;")
            for sl in pi.decl.expose:
                ti = self.prog.tables[sl.table]
                ci = ti.columns[sl.column]
                obj = "state." + self._field(sl.table)
                if sl.row is None:
                    self.line(
                        f"for slot in 0..{ti.layout.capacity}{{if ({obj}.occupied_bitmap[slot/64]>>(slot%64))&1==1{{let id=(({obj}.generation[slot] as u64)<<{ti.layout.key_bits})|slot as u64;"
                    )
                    access = A.TableAccess(sl.table, A.Var("_projection_id"), sl.column)
                    inner = dict(env)
                    inner["_projection_id"] = "id"
                    value = self._projection_value(access, inner, ci.type)
                    self.line(
                        f"frame.cells.push(ProjectionCell{{table:{json.dumps(sl.table)},column:{json.dumps(sl.column)},requested:None,id,live:true,value:Some({value})}});}}}}"
                    )
                else:
                    key = self._expr(sl.row, env, "post", ti.key_type)
                    self.line(
                        f"{{let requested={key};let slot=(requested&{ti.layout.capacity - 1}) as usize;let id=(({obj}.generation[slot] as u64)<<{ti.layout.key_bits})|slot as u64;let live=table_exists_{sl.table}(&{obj},requested);"
                    )
                    access = A.TableAccess(sl.table, A.Var("_projection_id"), sl.column)
                    inner = dict(env)
                    inner["_projection_id"] = "id"
                    value = self._projection_value(access, inner, ci.type)
                    self.line(
                        f"frame.cells.push(ProjectionCell{{table:{json.dumps(sl.table)},column:{json.dumps(sl.column)},requested:Some(requested),id,live,value:if live{{Some({value})}}else{{None}}}});}}"
                    )
            for intent in pi.decl.enable_intents:
                condition = self._expr(intent.condition, env, "post", BOOL)
                self.line(
                    f"{{let enabled=(||->Result<bool,ErrorCode>{{Ok({condition})}})().unwrap_or(false);let mut bounds=Vec::new();if enabled{{"
                )
                ti = self.prog.transitions[intent.transition]
                for p in ti.decl.params:
                    self.line(
                        f"bounds.push(ParameterBound{{parameter:{json.dumps(p.name)},min:None,max:None,exact:None}});"
                    )
                for _, req in ti.decl.requires:
                    self._projection_bounds(req, intent.transition, env)
                self.line(
                    f"}}frame.affordances.push(Affordance{{transition:{json.dumps(intent.transition)},enabled,bounds}});}}"
                )
            self.line("Ok(frame)}")

    def _emit_runtime(self, use_rt):
        self.line("#[derive(Clone, Debug, PartialEq, Eq)] pub enum Params {")
        for name, ti in self.prog.transitions.items():
            fields = ",".join(
                f"{self._param_field(p.name, ti.decl.params)}:{_rust_type(ti.param_types[p.name])}"
                for p in ti.decl.params
            )
            self.line(f"    {self._variant(name)}{{{fields}}},")
        self.line("}")
        target = "self" if self.prog.transitions else "*self"
        self.line(
            "impl Params {pub fn execute(&self,state:&mut State)->Result<(),ErrorCode>{match "
            + target
            + "{"
        )
        for name, ti in self.prog.transitions.items():
            binds = ",".join(
                self._param_field(p.name, ti.decl.params) for p in ti.decl.params
            )
            args = ",".join(
                "*" + self._param_field(p.name, ti.decl.params) for p in ti.decl.params
            )
            self.line(
                f"    Params::{self._variant(name)}{{{binds}}}=>{self._function(name)}(state{',' + args if args else ''}),"
            )
        self.line("}}")
        self.line(
            "pub fn encode(&self)->(u32,Vec<u8>){let mut out=Vec::new();out.extend_from_slice(&PROGRAM_FINGERPRINT);let transition=match "
            + target
            + "{"
        )
        for ident, (name, ti) in enumerate(self.prog.transitions.items()):
            binds = ",".join(
                self._param_field(p.name, ti.decl.params) for p in ti.decl.params
            )
            self.line(f"    Params::{self._variant(name)}{{{binds}}}=>{{")
            for p in ti.decl.params:
                self.line(
                    self._encode_scalar(
                        "*" + self._param_field(p.name, ti.decl.params),
                        ti.param_types[p.name],
                    )
                )
            self.line(f"    {ident}u32}},")
        self.line("};(transition,out)}")
        self.line(
            "pub fn decode(transition:u32,args:impl AsRef<[u8]>)->Result<Self,ErrorCode>{let mut r=Reader::new(args.as_ref());if r.take::<32>()?!=PROGRAM_FINGERPRINT{return Err(ErrorCode::InvalidEncoding);}let value=match transition{"
        )
        for ident, (name, ti) in enumerate(self.prog.transitions.items()):
            self.line(f"    {ident}=>Self::{self._variant(name)}{{")
            for p in ti.decl.params:
                self.line(
                    f"        {self._param_field(p.name, ti.decl.params)}:{self._decode_scalar(ti.param_types[p.name])},"
                )
            self.line("    },")
        self.line(
            "    _=>return Err(ErrorCode::InvalidEncoding),};r.done()?;Ok(value)}\n}"
        )
        self.line(r"""
/// Ordered serial execution, also the fallback for speculative conflicts.
pub fn run_batch_serial(state:&mut State,intents:&[Params])->Vec<Result<(),ErrorCode>>{intents.iter().map(|p|p.execute(state)).collect()}
/// Speculate on immutable snapshots; validate the full pre-state at ordered commit.
/// Conservative validation covers data-dependent reads, indexes, occupancy,
/// aggregates and effects. A stale speculation is re-executed against the
/// committed state, exactly preserving sequential intent-log semantics.
pub fn run_batch(state:&mut State,intents:&[Params])->Vec<Result<(),ErrorCode>>{
    let mut results=Vec::with_capacity(intents.len());
    let workers=std::thread::available_parallelism().map(|n|n.get()).unwrap_or(1).min(32);
    for chunk in intents.chunks(workers.max(1)){
        let base=state.clone();
        let candidates=std::thread::scope(|scope|{
            let handles:Vec<_>=chunk.iter().map(|intent|{let pre=&base;scope.spawn(move||{let mut staged=pre.clone();let result=intent.execute(&mut staged);(staged,result)})}).collect();
            handles.into_iter().map(|h|h.join().expect("VERA speculative worker failed")).collect::<Vec<_>>()
        });
        for (intent,(staged,result)) in chunk.iter().zip(candidates){
            if *state==base {if result.is_ok(){*state=staged;}results.push(result);}else{results.push(intent.execute(state));}
        }
    }results
}
""")
        if use_rt:
            self.line(
                "pub use ::vera_rt::{IoUringWAL as WAL,IntentRecord,replay_file,atomic_write,checksum};"
            )
        else:
            source = (
                Path(__file__).with_name("templates").joinpath("wal.rs").read_text()
            )
            self.line(
                "mod wal {\n"
                + source
                + "\n}\npub use wal::{WAL,IntentRecord,replay_file,atomic_write,checksum};"
            )

    def _emit_snapshot(self):
        self.line(
            'impl State {pub fn encode(&self)->Vec<u8>{let mut out=Vec::new();out.extend_from_slice(b"VERASNP2");out.extend_from_slice(&STATE_SCHEMA);'
        )
        for tn, ti in self.prog.tables.items():
            obj = "self." + self._field(tn)
            self.line(
                f"for value in &{obj}.occupied_bitmap{{out.extend_from_slice(&value.to_le_bytes());}}for value in &{obj}.generation{{out.extend_from_slice(&value.to_le_bytes());}}"
            )
            for cn, ci in ti.columns.items():
                self.line(
                    f"for value in &{obj}.col_{cn}{{{self._encode_scalar('*value', ci.type)}}}"
                )
                if ci.tracked_sum:
                    self.line(
                        f"out.extend_from_slice(&{obj}.agg_sum_{cn}.to_le_bytes());"
                    )
                if ci.unique:
                    self.line(
                        f"for value in &{obj}.hash_{cn}_key{{out.extend_from_slice(&value.to_le_bytes());}}for value in &{obj}.hash_{cn}_used{{out.extend_from_slice(&value.to_le_bytes());}}"
                    )
        self.line(
            "out.extend_from_slice(&(self.outbox.len() as u64).to_le_bytes());for effect in &self.outbox{match effect{"
        )
        for idx, (name, eff) in enumerate(self.prog.effects.items()):
            binds = ",".join(self._param_field(p.name, eff.params) for p in eff.params)
            self.line(
                f"Effect::{self.effect_variants[name]}{{{binds}}}=>{{out.extend_from_slice(&{idx}u32.to_le_bytes());"
            )
            for p in eff.params:
                self.line(
                    self._encode_scalar(
                        "*" + self._param_field(p.name, eff.params),
                        self._resolved(p.type),
                    )
                )
            self.line("},")
        if not self.prog.effects:
            self.line('_=>unreachable!("uninhabited effect"),')
        self.line(
            "}}let crc=checksum(&out);out.extend_from_slice(&crc.to_le_bytes());out}"
        )
        self.line(
            'pub fn decode(data:impl AsRef<[u8]>)->Result<Self,ErrorCode>{let data=data.as_ref();let end=data.len().checked_sub(4).ok_or(ErrorCode::InvalidEncoding)?;if checksum(&data[..end])!=u32::from_le_bytes(data[end..].try_into().unwrap()){return Err(ErrorCode::InvalidEncoding);}let mut r=Reader::new(&data[..end]);if r.take::<8>()?!=*b"VERASNP2"||r.take::<32>()?!=STATE_SCHEMA{return Err(ErrorCode::InvalidEncoding);}let mut state=Self::new();'
        )
        for tn, ti in self.prog.tables.items():
            obj = "state." + self._field(tn)
            self.line(
                f"for value in &mut {obj}.occupied_bitmap{{*value=u64::from_le_bytes(r.take::<8>()?);}}for value in &mut {obj}.generation{{*value=u32::from_le_bytes(r.take::<4>()?);}}"
            )
            if ti.layout.capacity % 64:
                self.line(
                    f"if {obj}.occupied_bitmap[{(ti.layout.capacity + 63) // 64 - 1}]>>{ti.layout.capacity % 64}!=0{{return Err(ErrorCode::InvalidEncoding);}}"
                )
            for cn, ci in ti.columns.items():
                self.line(
                    f"for value in &mut {obj}.col_{cn}{{*value={self._decode_scalar(ci.type)};}}"
                )
                if ci.tracked_sum:
                    rt = self._agg_rust(ci.type)
                    size = 32 if rt == "I256" else 16
                    self.line(
                        f"{obj}.agg_sum_{cn}={rt}::from_le_bytes(r.take::<{size}>()?);"
                    )
                if ci.unique:
                    self.line(
                        f"for value in &mut {obj}.hash_{cn}_key{{*value=u64::from_le_bytes(r.take::<8>()?);}}for value in &mut {obj}.hash_{cn}_used{{*value=u64::from_le_bytes(r.take::<8>()?);}}"
                    )
        self.line(
            "let count=u64::from_le_bytes(r.take::<8>()?);if count>r.data.len() as u64{return Err(ErrorCode::InvalidEncoding);}for _ in 0..count{let tag=u32::from_le_bytes(r.take::<4>()?);let effect=match tag{"
        )
        for idx, (name, eff) in enumerate(self.prog.effects.items()):
            self.line(f"{idx}=>Effect::{self.effect_variants[name]}{{")
            for p in eff.params:
                self.line(
                    f"{self._param_field(p.name, eff.params)}:{self._decode_scalar(self._resolved(p.type))},"
                )
            self.line("},")
        self.line(
            "_=>return Err(ErrorCode::InvalidEncoding),};state.outbox.push(effect);}r.done()?;state.validate()?;Ok(state)}"
        )
        self.line(
            "pub fn validate(&self)->Result<(),ErrorCode>{let state=self;let pre=self;"
        )
        for tn, ti in self.prog.tables.items():
            obj = "self." + self._field(tn)
            if ti.layout.capacity % 64:
                self.line(
                    f"if {obj}.occupied_bitmap[{(ti.layout.capacity + 63) // 64 - 1}]>>{ti.layout.capacity % 64}!=0{{return Err(ErrorCode::InvalidEncoding);}}"
                )
            for cn, ci in ti.columns.items():
                self.line(
                    f"for i in 0..{ti.layout.capacity}{{if ({obj}.occupied_bitmap[i/64]>>(i%64))&1==0 && {obj}.col_{cn}[i]!={_zero_rust(ci.type)}{{return Err(ErrorCode::InvalidEncoding);}}}}"
                )
                if ci.type.kind == "enum" and len(ci.type.enum_tags) < 256:
                    self.line(
                        f"if {obj}.col_{cn}.iter().any(|&value|value>={len(ci.type.enum_tags)}u8){{return Err(ErrorCode::InvalidEncoding);}}"
                    )
                if ci.tracked_sum:
                    rt = self._agg_rust(ci.type)
                    self.line(
                        f"let mut total:{rt}={self._agg_zero(ci.type)};for i in 0..{ti.layout.capacity}{{if ({obj}.occupied_bitmap[i/64]>>(i%64))&1==1{{total=total.checked_add({self._agg_cast(f'{obj}.col_{cn}[i]', ci.type)}).ok_or(ErrorCode::InvalidEncoding)?;}}}}if total!={obj}.agg_sum_{cn}{{return Err(ErrorCode::InvalidEncoding);}}"
                    )
                if ci.unique:
                    self.line(
                        f"let mut check={obj}.clone();check.hash_{cn}_key.fill(0);check.hash_{cn}_used.fill(0);for i in 0..{ti.layout.capacity}{{if (check.occupied_bitmap[i/64]>>(i%64))&1==1{{let value=check.col_{cn}[i];hash_insert_{tn}_{cn}(&mut check,&value,i).map_err(|_|ErrorCode::InvalidEncoding)?;}}}}if check.hash_{cn}_key!={obj}.hash_{cn}_key||check.hash_{cn}_used!={obj}.hash_{cn}_used{{return Err(ErrorCode::InvalidEncoding);}}"
                    )
        for inv in self.prog.invariants.values():
            if inv.quant_table is None:
                self.type_env = {}
                self.line(
                    f"if !({self._expr(inv.expr, {}, 'post', BOOL)}){{return Err(ErrorCode::InvalidEncoding);}}"
                )
            else:
                tn = inv.quant_table
                ti = self.prog.tables[tn]
                self.type_env = {inv.quant_var: ti.key_type}
                key = "_vera_quant_key"
                self.line(
                    f"for i in 0..{ti.layout.capacity}{{if (self.{self._field(tn)}.occupied_bitmap[i/64]>>(i%64))&1==1{{let {key}=((self.{self._field(tn)}.generation[i] as u64)<<{ti.layout.key_bits})|i as u64;"
                )
                self.line(
                    f"if !({self._expr(inv.expr, {inv.quant_var: key}, 'post', BOOL)}){{return Err(ErrorCode::InvalidEncoding);}}}}}}"
                )
        self.line("for effect in &self.outbox {match effect {")
        for name, eff in self.prog.effects.items():
            binds = ",".join(self._param_field(p.name, eff.params) for p in eff.params)
            self.line(f"Effect::{self.effect_variants[name]}{{{binds}}}=>{{")
            for p in eff.params:
                ct = self._resolved(p.type)
                if ct.kind == "enum" and len(ct.enum_tags) < 256:
                    field = self._param_field(p.name, eff.params)
                    self.line(
                        f"if *{field}>={len(ct.enum_tags)}u8{{return Err(ErrorCode::InvalidEncoding);}}"
                    )
            self.line("},")
        if not self.prog.effects:
            self.line('_=>unreachable!("uninhabited effect"),')
        self.line("}}Ok(())}}")
        self.line(r"""
pub fn snapshot(state:&State,path:&str)->std::io::Result<()>{state.validate().map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidData,"invalid VERA state"))?;atomic_write(path,&state.encode())}

pub fn restore(path:&str)->std::io::Result<State>{State::decode(std::fs::read(path)?).map_err(|_|std::io::Error::new(std::io::ErrorKind::InvalidData,"invalid VERA snapshot"))}
""")

    def _emit_demo_main(self):
        self.line("fn main(){let mut initial=State::new();")
        seeded = False
        for tn, ti in self.prog.tables.items():
            numeric = [
                ci
                for ci in ti.columns.values()
                if ci.type.kind in ("i64", "dec64", "u64")
            ]
            if seeded or ti.layout.capacity < 2 or not numeric:
                continue
            seeded = True
            ci = numeric[0]
            obj = "initial." + self._field(tn)
            self.line(
                f"{obj}.occupied_bitmap[0]=3;{obj}.generation[0]=1;{obj}.generation[1]=1;{obj}.col_{ci.name}[0]=500000000;{obj}.col_{ci.name}[1]=300000000;"
            )
            if ci.tracked_sum:
                self.line(f"{obj}.agg_sum_{ci.name}=800000000;")
        self.line(
            'if initial.validate().is_err(){initial=State::new();}initial.validate().expect("program requires an application-supplied valid initial state");'
        )
        self.line("let mut state=initial.clone();let intents=vec![")
        for name, ti in list(self.prog.transitions.items())[:6]:
            fields = []
            for p in ti.decl.params:
                ct = ti.param_types[p.name]
                if ct.kind == "id64":
                    value = f"1u64<<KEY_BITS_{self._constant(ct.key_table)}"
                elif ct.is_bool:
                    value = "false"
                elif ct.kind == "bytes32":
                    value = "[0u8;32]"
                elif ct.kind == "enum":
                    value = "0"
                elif ct.kind in ("u8", "u16"):
                    value = "1"
                else:
                    value = "100000000"
                fields.append(f"{self._param_field(p.name, ti.decl.params)}:{value}")
            self.line(f"Params::{self._variant(name)}{{{','.join(fields)}}},")
        self.line(
            '];let mut wal=WAL::open("vera.wal").unwrap();for rec in replay_file("vera.wal").unwrap(){let intent=Params::decode(rec.transition,rec.args).unwrap();let _=intent.execute(&mut state);}for intent in &intents{let(id,args)=intent.encode();wal.append(id,args).unwrap();}wal.sync().unwrap();println!("wal tx_seq = {}",wal.tx_seq);'
        )
        self.line(
            'let results=run_batch(&mut state,&intents);println!("batch committed {}/{}",results.iter().filter(|r|r.is_ok()).count(),intents.len());snapshot(&state,"vera.snap").unwrap();'
        )
        self.line(
            'let records=replay_file("vera.wal").unwrap();let mut replay=initial;for rec in records{let intent=Params::decode(rec.transition,rec.args).unwrap();let _=intent.execute(&mut replay);}assert_eq!(replay,state);println!("replay matches live state: deterministic");}'
        )


def _walk_stmts(node):
    """Visit statements nested anywhere in the expression/declaration tree.

    Folds can occur inside old keys, allocation fields, selects and pure
    specification expressions, so statement-only recursion misses guards.
    """
    if dataclasses.is_dataclass(node):
        yield node
        for field in dataclasses.fields(node):
            yield from _walk_stmts(getattr(node, field.name))
    elif isinstance(node, (list, tuple)):
        for item in node:
            yield from _walk_stmts(item)


def generate_rust(
    prog: Program, use_rt: bool = False, include_main: bool = True
) -> str:
    return RustGenerator(prog).emit(use_rt, include_main)
