# Byte values at the host boundary

`bytes<N>` is a logical sequence of zero through `N` bytes. Capacity is a type
property, not the current length. Zero capacity is valid. This type does not
assert UTF-8 or provide string normalization semantics.

Native CLI arguments use logical hexadecimal text, for example
`--param data=0x00ab`. Inside record/union JSON, the same value is a JSON string:
`{"data":"0x00ab"}`. Output uses lowercase hexadecimal with a `0x` prefix.
Malformed hex, odd digit counts and logical lengths beyond the declared capacity
are rejected. JSON arrays are not an alternate representation.

Canonical persistent and source-codec bytes contain:

1. An unsigned 64-bit little-endian logical length.
2. The logical bytes in order.
3. Zero padding to the declared capacity.

The total wire width is `8 + N`. Decoders require the exact total width, a length
at most `N`, and zero unused padding. Record fields retain declaration order;
unions retain their unsigned 32-bit tag and zero inactive payload padding. Existing
scalar encodings, including `bytes32`, are unchanged. `bytes32` is an exact-width
scalar, distinct from the variable logical length of `bytes<32>`.

`encode<T>(value)` produces the canonical representation as a logical byte value.
`can_decode<T>(bytes)` returns false for malformed representations. `decode<T>`
traps with `ERR_DECODE` for them; use lazy `select` to guard a partial decode.
Byte indexing/slicing trap with `ERR_BYTES_BOUNDS`; concatenation, resize or a
slice exceeding its result capacity trap with `ERR_BYTES_CAPACITY`. Resize never
silently truncates data. Arguments are evaluated once, left to right, and failed
transactions roll back both state and effects.

Native unique indexes hash non-scalar values using FNV-1a64 over their canonical
bytes (offset 14695981039346656037, prime 1099511628211), followed by VERA's existing
`mix64`. Equality still compares the full typed value; a hash match is not an
equality proof. Scalar hashing and historical persisted scalar schemas remain
unchanged. Snapshot schema identities include byte capacities.

No default validation, execution or codec capacity ceiling is added. Declared
capacity, actual host integer/address-space representation and available memory
remain relevant. A failed allocation is not evidence that a smaller model is
valid, and verification never silently shrinks a byte capacity.

## Native fuzz sampling

The native fuzzer samples empty values, random single bytes, common two-byte
patterns, and every fitting byte literal already in the program. Full-capacity
literals are eligible regardless of size. This is a coverage strategy, not an
admission or execution limit; it does not enumerate all lengths or contents.
Nested records/unions use the same sampling recursively. Fuzzing searches for
bugs and never substitutes for proof. Structured and byte payloads currently
retain their full values during trace minimization rather than being minimized
byte by byte.
