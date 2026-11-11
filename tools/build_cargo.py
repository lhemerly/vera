#!/usr/bin/env python3
"""Regenerate the Rust runtime crate's spec module from a VERA source file.

Usage: python tools/build_cargo.py [spec.vera] [output.rs]
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from vera.codegen_rust import generate_rust
from vera.parser import parse_file
from vera.semantic import analyze


def main() -> int:
    src = sys.argv[1] if len(sys.argv) > 1 else "examples/bank.vera"
    out = sys.argv[2] if len(sys.argv) > 2 else "rust/vera-rt/src/spec.rs"
    prog = analyze(parse_file(src))
    if prog.diags.has_errors():
        for d in prog.diags.errors:
            print(d.render())
        return 1
    root = Path(__file__).resolve().parent.parent
    (root / "rust/vera-rt/src/wal.rs").write_bytes(
        (root / "vera/templates/wal.rs").read_bytes()
    )
    Path(out).write_text(generate_rust(prog, use_rt=True, include_main=False))
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
