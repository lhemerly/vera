"""Fixed-width scalar types, binary memory layout, and id64 key semantics."""

from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

from .errors import D_CAPACITY, D_UNKNOWN_TABLE, VeraError


@dataclass(frozen=True)
class ScalarType:
    """A VERA primitive scalar type (Section 2.1)."""

    kind: str
    width: int                      # bit width
    align: int                      # byte alignment
    key_table: Optional[str] = None # for id64<T>
    enum_tags: Tuple[str, ...] = ()
    scale: int = 0                  # for dec64/dec128: 10**scale units per integer

    # --- derived facts -----------------------------------------------------
    @property
    def size(self) -> int:
        return self.width // 8

    @property
    def is_bool(self) -> bool:
        return self.kind == "bool"

    @property
    def is_numeric(self) -> bool:
        return self.kind in ("u8", "u16", "u32", "u64", "i64", "dec64", "dec128")

    @property
    def is_signed(self) -> bool:
        return self.kind in ("i64", "dec64", "dec128")

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
        if k in ("u8", "u16", "u32", "u64", "sym32", "bytes32", "id64"):
            return 0
        if k == "i64":
            return -(2 ** 63)
        if k == "dec64":
            return -(2 ** 63)
        if k == "dec128":
            return -(2 ** 127)
        if k == "enum":
            return 0
        raise KeyError(k)

    @property
    def max_value(self) -> int:
        k = self.kind
        if k == "bool":
            return 1
        if k == "u8":
            return 2 ** 8 - 1
        if k == "u16":
            return 2 ** 16 - 1
        if k in ("u32", "sym32"):
            return 2 ** 32 - 1
        if k in ("u64", "id64"):
            return 2 ** 64 - 1
        if k == "i64":
            return 2 ** 63 - 1
        if k == "dec64":
            return 2 ** 63 - 1
        if k == "dec128":
            return 2 ** 127 - 1
        if k == "bytes32":
            return 2 ** 256 - 1
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
            "u8": "u8", "u16": "u16", "u32": "u32", "u64": "u64",
            "i64": "i64", "dec64": "i64", "dec128": "i128",
            "sym32": "u32", "bytes32": "[u8; 32]", "id64": "u64",
            "enum": "u8",
        }[self.kind]

    @property
    def rust_ctype(self) -> str:
        return {
            "bool": "bool",
            "u8": "uint8_t", "u16": "uint16_t", "u32": "uint32_t",
            "u64": "uint64_t", "i64": "int64_t", "dec64": "int64_t",
            "dec128": "__int128", "sym32": "uint32_t",
            "bytes32": "uint8_t[32]", "id64": "uint64_t", "enum": "uint8_t",
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
    "bool": BOOL, "u8": U8, "u16": U16, "u32": U32, "u64": U64,
    "i64": I64, "dec64": DEC64, "dec128": DEC128, "sym32": SYM32,
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


def compute_layout(name: str, capacity: int, columns: List[Tuple[str, ScalarType]],
                   tracked: List[str]) -> TableLayout:
    """Compute the 64-byte-aligned SoA layout for a table.

    Every column vector (and the occupancy bitmap / generation array) starts on a
    64-byte boundary, matching the Section 2.3 memory formula.
    """
    if capacity <= 0 or (capacity & (capacity - 1)) != 0:
        raise VeraError(D_CAPACITY,
                        f"table {name!r} capacity {capacity} is not a positive power of two")
    key_bits = capacity.bit_length() - 1
    layout = TableLayout(name=name, capacity=capacity, key_bits=key_bits)
    offset = 0

    def add_field(fname: str, fsize: int, falign: int, ftype: ScalarType) -> None:
        nonlocal offset
        start = _align_up(offset, 64)
        pad = start - offset
        layout.fields.append(FieldLayout(fname, start, fsize, falign, ftype, pad))
        offset = start + fsize

    # Occupancy bitmap: C bits stored as C/64 u64 words.
    add_field("occupied_bitmap", max(8, ((capacity + 63) // 64) * 8), 8, U64)
    # Generation array: one u32 per slot.
    add_field("generation", 4 * capacity, 4, U32)
    for cname, ctype in columns:
        add_field(f"col_{cname}", ctype.size * capacity, ctype.align, ctype)
        layout.column_offsets[cname] = layout.fields[-1].offset
        layout.column_types[cname] = ctype
    for cname in tracked:
        if cname not in layout.column_types:
            raise VeraError(D_UNKNOWN_COLUMN,
                            f"tracked_sum column {cname!r} is not declared in table {name!r}")
        ctype = layout.column_types[cname]
        add_field(f"agg_sum_{cname}", ctype.size, ctype.align, ctype)
        layout.agg_offsets[cname] = layout.fields[-1].offset
        layout.agg_types[cname] = ctype
    layout.size = _align_up(offset, 64)
    return layout


def slot_bits_for(capacity: int) -> int:
    """SMT bitvector width used to index a table slot."""
    if capacity <= 0 or (capacity & (capacity - 1)) != 0:
        raise VeraError(D_CAPACITY,
                        f"capacity {capacity} is not a positive power of two")
    return max(1, capacity.bit_length() - 1)
