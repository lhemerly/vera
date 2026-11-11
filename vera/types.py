"""Fixed-width scalar types, binary memory layout, and id64 key semantics."""

from dataclasses import dataclass, field
from decimal import Decimal
import hashlib
from typing import Dict, List, Optional, Tuple

from .errors import D_CAPACITY, D_UNKNOWN_COLUMN, D_UNKNOWN_TABLE, VeraError

# Maximum probe sequence length for secondary unique-key indexes (Section 2.3).
MAX_PROBES = 16


@dataclass(frozen=True)
class ScalarType:
    """A VERA primitive scalar type (Section 2.1)."""

    kind: str
    width: int  # bit width
    align: int  # byte alignment
    key_table: Optional[str] = None  # for id64<T>
    enum_tags: Tuple[str, ...] = ()
    scale: int = 0  # for dec64/dec128: 10**scale units per integer

    # --- derived facts -----------------------------------------------------
    @property
    def size(self) -> int:
        return self.width // 8

    @property
    def is_bool(self) -> bool:
        return self.kind == "bool"

    @property
    def is_numeric(self) -> bool:
        return self.kind in (
            "u8",
            "u16",
            "u32",
            "u64",
            "u128",
            "u256",
            "i64",
            "i128",
            "i256",
            "dec64",
            "dec128",
        )

    @property
    def is_signed(self) -> bool:
        return self.kind in ("i64", "i128", "i256", "dec64", "dec128")

    @property
    def is_decimal(self) -> bool:
        return self.kind in ("dec64", "dec128")

    @property
    def is_unsigned_int(self) -> bool:
        return self.kind in ("u8", "u16", "u32", "u64")

    @property
    def is_integer(self) -> bool:
        return self.kind in ("u8", "u16", "u32", "u64", "i64")

    @property
    def is_ordered(self) -> bool:
        return self.is_numeric or self.kind == "enum"

    @property
    def min_value(self) -> int:
        k = self.kind
        if k == "bool":
            return 0
        if k in ("u8", "u16", "u32", "u64", "u128", "u256", "sym32", "bytes32", "id64"):
            return 0
        if k in ("i64", "i128", "i256"):
            return -(2 ** (self.width - 1))
        if k == "dec64":
            return -(2 ** (self.width - 1))
        if k == "dec128":
            return -(2 ** (self.width - 1))
        if k == "enum":
            return 0
        raise KeyError(k)

    @property
    def max_value(self) -> int:
        k = self.kind
        if k == "bool":
            return 1
        if k == "u8":
            return 2**self.width - 1
        if k == "u16":
            return 2**self.width - 1
        if k in ("u32", "sym32"):
            return 2**self.width - 1
        if k in ("u64", "u128", "u256", "id64"):
            return 2**self.width - 1
        if k in ("i64", "i128", "i256"):
            return 2 ** (self.width - 1) - 1
        if k == "dec64":
            return 2 ** (self.width - 1) - 1
        if k == "dec128":
            return 2 ** (self.width - 1) - 1
        if k == "bytes32":
            return 2**256 - 1
        if k == "enum":
            return max(0, len(self.enum_tags) - 1)
        raise KeyError(k)

    # --- target mappings ---------------------------------------------------
    @property
    def smt_sort(self) -> str:
        if self.kind == "bool":
            return "Bool"
        return f"(_ BitVec {self.width})"

    @property
    def rust_type(self) -> str:
        return {
            "bool": "bool",
            "u8": "u8",
            "u16": "u16",
            "u32": "u32",
            "u64": "u64",
            "i64": "i64",
            "dec64": "i64",
            "dec128": "i128",
            "sym32": "u32",
            "bytes32": "[u8; 32]",
            "id64": "u64",
            "enum": "u8",
            "i128": "i128",
            "u128": "u128",
            "i256": "I256",
            "u256": "U256",
        }[self.kind]

    @property
    def rust_ctype(self) -> str:
        return {
            "bool": "bool",
            "u8": "uint8_t",
            "u16": "uint16_t",
            "u32": "uint32_t",
            "u64": "uint64_t",
            "i64": "int64_t",
            "dec64": "int64_t",
            "dec128": "__int128",
            "sym32": "uint32_t",
            "bytes32": "uint8_t[32]",
            "id64": "uint64_t",
            "enum": "uint8_t",
            "i128": "__int128",
            "u128": "unsigned __int128",
            "i256": "vera_i256",
            "u256": "vera_u256",
        }[self.kind]

    def __str__(self) -> str:
        if self.kind == "id64":
            return f"id64<{self.key_table}>"
        if self.kind == "enum":
            return "enum{" + ", ".join(self.enum_tags) + "}"
        return self.kind


# Predefined scalars (Section 2.1 table).
BOOL = ScalarType("bool", 8, 1)
U8 = ScalarType("u8", 8, 1)
U16 = ScalarType("u16", 16, 2)
U32 = ScalarType("u32", 32, 4)
U64 = ScalarType("u64", 64, 8)
I64 = ScalarType("i64", 64, 8)
DEC64 = ScalarType("dec64", 64, 8, scale=8)
DEC128 = ScalarType("dec128", 128, 16, scale=18)
SYM32 = ScalarType("sym32", 32, 4)
BYTES32 = ScalarType("bytes32", 256, 32)

_PRIMITIVES = {
    "bool": BOOL,
    "u8": U8,
    "u16": U16,
    "u32": U32,
    "u64": U64,
    "i64": I64,
    "dec64": DEC64,
    "dec128": DEC128,
    "sym32": SYM32,
    "bytes32": BYTES32,
}


def primitive(name: str) -> Optional[ScalarType]:
    return _PRIMITIVES.get(name)


def is_primitive_name(name: str) -> bool:
    return name in _PRIMITIVES


def id64(table: str) -> ScalarType:
    return ScalarType("id64", 64, 8, key_table=table)


def enum_type(tags: Tuple[str, ...]) -> ScalarType:
    if len(tags) > 256:
        raise VeraError(D_CAPACITY, f"enum has {len(tags)} tags; maximum is 256")
    return ScalarType("enum", 8, 1, enum_tags=tags)


def scalar_from_name(name: str) -> Optional[ScalarType]:
    return _PRIMITIVES.get(name)


def symbol_id(text: str) -> int:
    """Stable symbol encoding shared by all compilation and ingress paths."""
    return int.from_bytes(hashlib.sha256(text.encode("utf-8")).digest()[:4], "little")


def literal_value(value, ctype: ScalarType):
    """Normalize a source literal without binary floating point or rounding.

    Integer decimal literals already denote scaled units. Fractional literals
    denote human decimals and must be exactly representable at the type scale.
    """
    if ctype.is_bool:
        if type(value) is not bool:
            raise ValueError("bool requires a boolean value")
        return value
    if isinstance(value, str):
        if ctype.kind == "sym32":
            return symbol_id(value)
        if ctype.kind == "enum" and value in ctype.enum_tags:
            return ctype.enum_tags.index(value)
        raise ValueError(f"string is not a value of {ctype}")
    if ctype.kind == "bytes32":
        if isinstance(value, (bytes, bytearray)) and len(value) == 32:
            return bytes(value)
        if type(value) is int and 0 <= value < (1 << 256):
            return value.to_bytes(32, "little")
        raise ValueError(
            "bytes32 requires exactly 32 bytes or an unsigned 256-bit literal"
        )
    if isinstance(value, (Decimal, float)):
        if not ctype.is_decimal:
            raise ValueError("fractional literals require a decimal type")
        d = value if isinstance(value, Decimal) else Decimal(str(value))
        if not d.is_finite():
            raise ValueError("decimal must be finite")
        sign, digits, exponent = d.as_tuple()
        coefficient = int("".join(map(str, digits)) or "0")
        shift = exponent + ctype.scale
        if shift >= 0:
            value = coefficient * (10**shift)
        else:
            divisor = 10 ** (-shift)
            if coefficient % divisor:
                raise ValueError(
                    f"fractional precision exceeds {ctype.scale} digits for {ctype}"
                )
            value = coefficient // divisor
        if sign:
            value = -value
    if type(value) is not int or not ctype.min_value <= value <= ctype.max_value:
        raise ValueError(f"value is outside the domain of {ctype}")
    return value


def accumulator_type(ctype: ScalarType, capacity: int) -> ScalarType:
    """Physical integer type wide enough for every live-row sum."""
    needed = ctype.width + max(0, capacity.bit_length() - 1)
    width = 128 if needed <= 128 else 256
    if needed > 256:
        raise VeraError(
            D_CAPACITY, "tracked sum exceeds the 256-bit accumulator domain"
        )
    signed = ctype.is_signed
    return ScalarType(
        ("i" if signed else "u") + str(width), width, width // 8, scale=ctype.scale
    )


@dataclass
class FieldLayout:
    name: str
    offset: int
    size: int
    align: int
    type: ScalarType
    pad_before: int = 0


@dataclass
class TableLayout:
    """Structure-of-Arrays physical layout for one table (Section 2.3)."""

    name: str
    capacity: int
    key_bits: int
    fields: List[FieldLayout] = field(default_factory=list)
    size: int = 0
    column_offsets: Dict[str, int] = field(default_factory=dict)
    agg_offsets: Dict[str, int] = field(default_factory=dict)
    agg_types: Dict[str, ScalarType] = field(default_factory=dict)
    column_types: Dict[str, ScalarType] = field(default_factory=dict)
    unique_hashes: Dict[str, int] = field(default_factory=dict)
    hash_key_offsets: Dict[str, int] = field(default_factory=dict)
    hash_used_offsets: Dict[str, int] = field(default_factory=dict)

    @property
    def slot_mask(self) -> int:
        return self.capacity - 1

    @property
    def generation_bits(self) -> int:
        # The physical generation array is [u32; CAPACITY] (Section 8.1), so the
        # counter is 32 bits wide; the remaining high bits of the id64 are zero.
        return min(64 - self.key_bits, 32)

    @property
    def generation_max(self) -> int:
        return (1 << self.generation_bits) - 1

    def column_offset(self, column: str) -> int:
        return self.column_offsets[column]

    def key_from_slot(self, slot: int, generation: int) -> int:
        return (generation << self.key_bits) | slot

    def slot_of(self, key: int) -> int:
        return key & self.slot_mask

    def generation_of(self, key: int) -> int:
        return key >> self.key_bits


def _align_up(value: int, alignment: int) -> int:
    if alignment <= 1:
        return value
    return (value + alignment - 1) // alignment * alignment


def _next_pow2(value: int) -> int:
    if value <= 1:
        return 1
    return 1 << (value - 1).bit_length()


def compute_layout(
    name: str,
    capacity: int,
    columns: List[Tuple[str, ScalarType]],
    tracked: List[str],
    unique: Optional[List[str]] = None,
) -> TableLayout:
    """Compute the 64-byte-aligned SoA layout for a table.

    Every column vector (and the occupancy bitmap / generation array) starts on a
    64-byte boundary, matching the Section 2.3 memory formula.  Unique-key
    secondary indexes get a fixed-capacity linear-probing hash table with a
    probe bound ``UERA_MAX_PROBES`` (Section 2.3).
    """
    if capacity <= 0 or capacity > (1 << 32) or (capacity & (capacity - 1)) != 0:
        raise VeraError(
            D_CAPACITY,
            f"table {name!r} capacity {capacity} must be a power of two between 1 and 2^32",
        )
    key_bits = capacity.bit_length() - 1
    layout = TableLayout(name=name, capacity=capacity, key_bits=key_bits)
    offset = 0

    def add_field(fname: str, fsize: int, falign: int, ftype: ScalarType) -> None:
        nonlocal offset
        start = _align_up(offset, 64)
        pad = start - offset
        layout.fields.append(FieldLayout(fname, start, fsize, falign, ftype, pad))
        offset = start + fsize

    # Occupancy bitmap: C bits stored as ceil(C/64) u64 words.
    add_field("occupied_bitmap", max(8, ((capacity + 63) // 64) * 8), 8, U64)
    # Generation array: one u32 per slot.
    add_field("generation", 4 * capacity, 4, U32)
    for cname, ctype in columns:
        add_field(f"col_{cname}", ctype.size * capacity, ctype.align, ctype)
        layout.column_offsets[cname] = layout.fields[-1].offset
        layout.column_types[cname] = ctype
    for cname in tracked:
        if cname not in layout.column_types:
            raise VeraError(
                D_UNKNOWN_COLUMN,
                f"tracked_sum column {cname!r} is not declared in table {name!r}",
            )
        ctype = layout.column_types[cname]
        atype = accumulator_type(ctype, capacity)
        add_field(f"agg_sum_{cname}", atype.size, atype.align, atype)
        layout.agg_offsets[cname] = layout.fields[-1].offset
        layout.agg_types[cname] = atype
    for cname in unique or []:
        if cname not in layout.column_types:
            raise VeraError(
                D_UNKNOWN_COLUMN,
                f"unique column {cname!r} is not declared in table {name!r}",
            )
        # Two buckets per slot keep the load factor <= 0.5 so the probe bound is
        # meaningful; the table never holds more than ``capacity`` rows.
        buckets = _next_pow2(max(2 * capacity, 16))
        add_field(f"hash_{cname}_key", 8 * buckets, 8, U64)
        layout.hash_key_offsets[cname] = layout.fields[-1].offset
        words = max(1, (buckets + 63) // 64)
        add_field(f"hash_{cname}_used", 8 * words, 8, U64)
        layout.hash_used_offsets[cname] = layout.fields[-1].offset
        layout.unique_hashes[cname] = buckets
    layout.size = _align_up(offset, 64)
    return layout


def slot_bits_for(capacity: int) -> int:
    """SMT bitvector width used to index a table slot."""
    if capacity <= 0 or capacity > (1 << 32) or (capacity & (capacity - 1)) != 0:
        raise VeraError(
            D_CAPACITY, f"capacity {capacity} must be a power of two between 1 and 2^32"
        )
    return max(1, capacity.bit_length() - 1)


MASK64 = (1 << 64) - 1

# splitmix64 finalizer: the deterministic mix used by the secondary index.
_MIX1 = 0xBF58476D1CE4E5B9
_MIX2 = 0x94D049BB133111EB


def hash_int(value) -> int:
    """Convert a scalar column value to a 64-bit integer for hashing."""
    if isinstance(value, bool):
        return 1 if value else 0
    if isinstance(value, int):
        return value & MASK64
    if isinstance(value, (bytes, bytearray)):
        return int.from_bytes(bytes(value), "little") & MASK64
    if isinstance(value, str):
        return symbol_id(value)
    raise ValueError(f"cannot hash scalar value of type {type(value).__name__}")


def mix64(z: int) -> int:
    z &= MASK64
    z = ((z ^ (z >> 30)) * _MIX1) & MASK64
    z = ((z ^ (z >> 27)) * _MIX2) & MASK64
    return (z ^ (z >> 31)) & MASK64


def bucket_of(value, buckets: int) -> int:
    return mix64(hash_int(value)) & (buckets - 1)
