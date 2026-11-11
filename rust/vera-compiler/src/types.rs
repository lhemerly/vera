//! Scalar identity, evaluation domains and canonical physical table layouts.
use crate::ir::TableId;
use num_bigint::BigInt;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_PROBES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum ScalarType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I64,
    Dec64,
    Dec128,
    Sym32,
    Bytes32,
    Id64(TableId),
    Enum(Vec<String>),
}
impl ScalarType {
    pub fn bits(&self) -> u16 {
        match self {
            Self::Bool | Self::U8 | Self::Enum(_) => 8,
            Self::U16 => 16,
            Self::U32 | Self::Sym32 => 32,
            Self::U64 | Self::I64 | Self::Dec64 | Self::Id64(_) => 64,
            Self::Dec128 => 128,
            Self::Bytes32 => 256,
        }
    }
    pub fn signed(&self) -> bool {
        matches!(self, Self::I64 | Self::Dec64 | Self::Dec128)
    }
    pub fn scale(&self) -> u32 {
        match self {
            Self::Dec64 => 8,
            Self::Dec128 => 18,
            _ => 0,
        }
    }
    pub fn is_decimal(&self) -> bool {
        self.scale() != 0
    }
    pub fn is_bool(&self) -> bool {
        matches!(self, Self::Bool)
    }
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::I64 | Self::Dec64 | Self::Dec128
        )
    }
    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::I64
        )
    }
    pub fn is_ordered(&self) -> bool {
        self.is_numeric() || matches!(self, Self::Enum(_))
    }
    pub fn byte_width(&self) -> usize {
        usize::from(self.bits() / 8)
    }
    pub fn align(&self) -> u64 {
        self.byte_width() as u64
    }
    pub fn min_value(&self) -> BigInt {
        if self.signed() {
            -(BigInt::from(1u8) << (self.bits() - 1) as usize)
        } else {
            BigInt::from(0u8)
        }
    }
    pub fn max_value(&self) -> BigInt {
        match self {
            Self::Bool => BigInt::from(1u8),
            Self::Enum(tags) => BigInt::from(tags.len().saturating_sub(1)),
            _ => (BigInt::from(1u8) << (self.bits() - u16::from(self.signed())) as usize) - 1u8,
        }
    }
    pub fn aggregate_width(&self) -> u16 {
        if self.bits() > 64 {
            256
        } else {
            128
        }
    }
    pub fn rust_type(&self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 | Self::Enum(_) => "u8",
            Self::U16 => "u16",
            Self::U32 | Self::Sym32 => "u32",
            Self::U64 | Self::Id64(_) => "u64",
            Self::I64 | Self::Dec64 => "i64",
            Self::Dec128 => "i128",
            Self::Bytes32 => "[u8;32]",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::I64 => "i64",
            Self::Dec64 => "dec64",
            Self::Dec128 => "dec128",
            Self::Sym32 => "sym32",
            Self::Bytes32 => "bytes32",
            Self::Id64(_) => "id64",
            Self::Enum(_) => "enum",
        }
    }
}
impl fmt::Display for ScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id64(t) => write!(f, "id64<table#{}>", t.0),
            Self::Enum(tags) => write!(f, "enum{{{}}}", tags.join(", ")),
            _ => f.write_str(self.name()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Domain {
    pub scalar: ScalarType,
    pub width: u16,
}
impl Domain {
    pub fn scalar(scalar: ScalarType) -> Self {
        let width = scalar.bits();
        Self { scalar, width }
    }
    pub fn aggregate(scalar: ScalarType) -> Self {
        let width = scalar.aggregate_width();
        Self { scalar, width }
    }
    pub fn is_aggregate(&self) -> bool {
        self.width > self.scalar.bits()
    }
    pub fn signed(&self) -> bool {
        self.scalar.signed()
    }
    pub fn scale(&self) -> u32 {
        self.scalar.scale()
    }
    pub fn min_value(&self) -> BigInt {
        if !self.is_aggregate() {
            self.scalar.min_value()
        } else if self.signed() {
            -(BigInt::from(1u8) << (self.width - 1) as usize)
        } else {
            BigInt::from(0u8)
        }
    }
    pub fn max_value(&self) -> BigInt {
        if !self.is_aggregate() {
            self.scalar.max_value()
        } else {
            (BigInt::from(1u8) << (self.width - u16::from(self.signed())) as usize) - 1u8
        }
    }
}

#[derive(Clone, Debug)]
pub struct LayoutField {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub align: u64,
    pub pad_before: u64,
}
#[derive(Clone, Debug, Default)]
pub struct TableLayout {
    pub size: u64,
    pub fields: Vec<LayoutField>,
    pub column_offsets: Vec<u64>,
    pub aggregate_offsets: Vec<Option<u64>>,
    pub hash_key_offsets: Vec<Option<u64>>,
    pub hash_used_offsets: Vec<Option<u64>>,
    pub unique_buckets: Vec<Option<u64>>,
}

pub fn compute_layout(
    capacity: u64,
    columns: &[(String, ScalarType, bool, bool)],
) -> Result<TableLayout, String> {
    if capacity == 0 || capacity > (1u64 << 32) || !capacity.is_power_of_two() {
        return Err("capacity must be a power of two between 1 and 2^32".into());
    }
    let mut layout = TableLayout {
        column_offsets: vec![0; columns.len()],
        aggregate_offsets: vec![None; columns.len()],
        hash_key_offsets: vec![None; columns.len()],
        hash_used_offsets: vec![None; columns.len()],
        unique_buckets: vec![None; columns.len()],
        ..Default::default()
    };
    let mut end = 0u64;
    let mut add = |layout: &mut TableLayout, name: String, size: u64, align: u64| {
        let offset = end.div_ceil(64) * 64;
        layout.fields.push(LayoutField {
            name,
            offset,
            size,
            align,
            pad_before: offset - end,
        });
        end = offset + size;
        offset
    };
    add(
        &mut layout,
        "occupied_bitmap".into(),
        capacity.div_ceil(64).max(1) * 8,
        8,
    );
    add(&mut layout, "generation".into(), capacity * 4, 4);
    for (i, (name, ty, _, _)) in columns.iter().enumerate() {
        layout.column_offsets[i] = add(
            &mut layout,
            format!("col_{name}"),
            ty.byte_width() as u64 * capacity,
            ty.align(),
        );
    }
    for (i, (name, ty, tracked, _)) in columns.iter().enumerate() {
        if *tracked {
            let size = u64::from(ty.aggregate_width() / 8);
            layout.aggregate_offsets[i] =
                Some(add(&mut layout, format!("agg_sum_{name}"), size, size));
        }
    }
    for (i, (name, _, _, unique)) in columns.iter().enumerate() {
        if *unique {
            let buckets = (capacity * 2).max(16).next_power_of_two();
            layout.unique_buckets[i] = Some(buckets);
            layout.hash_key_offsets[i] =
                Some(add(&mut layout, format!("hash_{name}_key"), buckets * 8, 8));
            layout.hash_used_offsets[i] = Some(add(
                &mut layout,
                format!("hash_{name}_used"),
                buckets.div_ceil(64) * 8,
                8,
            ));
        }
    }
    layout.size = end.div_ceil(64) * 64;
    Ok(layout)
}

pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}
