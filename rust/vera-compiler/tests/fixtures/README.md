`codec-intent.hex`, `codec-snapshot.hex`, and `codec-wal.hex` are fixed
compatibility vectors for `codec.vera`. Tests compare emitted bytes against
these independent expected values.

The vectors encode and execute `Params::Create` with these values:

| Parameter | Raw value |
| --- | --- |
| flag | `true` |
| tiny | `255` |
| small | `0x1234` |
| word | `0x89abcdef` |
| big | `0xfedcba9876543210` |
| signed | `i64::MIN` |
| decimal | `-123456789` scaled units |
| wide | `-((1i128 << 120) + 123)` scaled units |
| symbol | `0xaabbccdd` |
| blob | bytes `0x00` through `0x1f` in order |
| tag | `1` (`B`) |
| peer | `u64::MAX` |

The snapshot contains one allocated row, its unique index and widened aggregates,
one `Event`, and a second empty row. The WAL frame contains this same typed intent
as transition `0`, intent ID `1`, sequence `1`. Hex files contain a final text
newline, which is not part of the encoded bytes.
