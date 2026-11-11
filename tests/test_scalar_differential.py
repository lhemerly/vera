"""Cross-backend arithmetic against independently calculated integer results."""

import random
import shutil
import subprocess

import pytest

from vera.codegen_rust import generate_rust
from vera.codegen_gpu import generate_cuda
from vera.parser import parse_source
from vera.runtime import Machine, Revert
from vera.semantic import analyze
from vera.types import primitive

OPS = ("add", "sub", "mul", "div", "rem", "min", "max")
TYPES = ("u8", "u16", "u32", "u64", "i64", "dec64", "dec128")


def mathematical(op, a, b, ty):
    scale = 10**ty.scale if ty.is_decimal else 1
    if op == "add":
        value = a + b
    elif op == "sub":
        value = a - b
    elif op == "mul":
        if a * b % scale:
            return None
        value = a * b // scale
    elif op in ("div", "rem"):
        if b == 0 or (ty.is_signed and a == ty.min_value and b == -1):
            return None
        numerator = a * scale if op == "div" else a
        quotient = abs(numerator) // abs(b)
        if (numerator < 0) != (b < 0):
            quotient = -quotient
        if op == "div":
            if ty.is_decimal and numerator % b:
                return None
            value = quotient
        else:
            value = a - quotient * b
    elif op == "min":
        value = min(a, b)
    else:
        value = max(a, b)
    return value if ty.min_value <= value <= ty.max_value else None


def cases(ty):
    scale = 10**ty.scale if ty.is_decimal else 1
    values = [0, 1, 2, ty.max_value, ty.max_value - 1, scale, 2 * scale]
    if ty.is_signed:
        values += [-1, -2, ty.min_value, ty.min_value + 1, -scale]
    values = sorted(set(v for v in values if ty.min_value <= v <= ty.max_value))
    pairs = [(a, b) for a in values for b in values]
    rng = random.Random(1729)
    pairs += [
        (
            rng.randint(ty.min_value, ty.max_value),
            rng.randint(ty.min_value, ty.max_value),
        )
        for _ in range(8)
    ]
    return pairs


def limb(value):
    value %= 1 << 256
    return (
        "v_make("
        + ",".join(f"{(value >> (32 * i)) & 0xFFFFFFFF}u" for i in range(8))
        + ")"
    )


@pytest.mark.parametrize("type_name", TYPES)
def test_scalar_values_and_failures_agree_across_backends(tmp_path, type_name):
    ty = primitive(type_name)
    declarations = "\n".join(
        f"""transition calc_{op}(a:{type_name},b:{type_name})
    reads [] writes [T[*].v] body {{
    let %v:{type_name}={op}(a,b);let %k:id64<T>=table_alloc(T,{{v:%v}});commit;}}"""
        for op in OPS
    )
    source = (
        f"module Scalars;table T capacity 2 {{key id:id64<T>;col v:{type_name};}}\n"
        + declarations
    )
    prog = analyze(parse_source(source))
    assert not prog.diags.has_errors(), prog.diags.render()
    rust_lines = []
    cpp_lines = []
    for op in OPS:
        for a, b in cases(ty):
            expected = mathematical(op, a, b, ty)
            machine = Machine(prog)
            try:
                machine.call("calc_" + op, {"a": a, "b": b})
                observed = machine.state.tables["T"].columns["v"][0]
            except Revert as exc:
                assert exc.code == "ERR_ARITH_OVERFLOW"
                observed = None
                assert machine.state == Machine(prog).state
            assert observed == expected, (type_name, op, a, b, expected, observed)
            rt = ty.rust_type
            rust_lines.append(
                "{let mut s=State::new();let result=calc_"
                + op
                + f"(&mut s,{a}{rt},{b}{rt});"
            )
            cpp_lines.append(
                "{Ctx ctx;vera_init(&ctx);uint32_t result=transition_v_calc_"
                + op
                + "(&ctx,"
                + limb(a)
                + ","
                + limb(b)
                + ");"
            )
            if expected is None:
                rust_lines.append(
                    "assert_eq!(result,Err(ErrorCode::ArithOverflow));assert_eq!(s,State::new());}"
                )
                cpp_lines.append("assert(result==3u);assert(ctx.s.occ[0]==0u);}")
            else:
                rust_lines.append(
                    f"assert_eq!(result,Ok(()));assert_eq!(s.t.col_v[0],{expected}{rt});}}"
                )
                cpp_lines.append(
                    "assert(result==1u);assert(v_eq(ctx.s.cols[0],"
                    + limb(expected)
                    + "));}"
                )
    if shutil.which("rustc"):
        path = tmp_path / "scalars.rs"
        path.write_text(
            generate_rust(prog, include_main=False)
            + "\nfn main(){\n"
            + "\n".join(rust_lines)
            + "\n}\n"
        )
        compiled = subprocess.run(
            [
                "rustc",
                "--edition",
                "2021",
                "-O",
                str(path),
                "-o",
                str(tmp_path / "rust-case"),
            ],
            capture_output=True,
            text=True,
            timeout=120,
        )
        assert compiled.returncode == 0, compiled.stderr
        run = subprocess.run(
            [str(tmp_path / "rust-case")], capture_output=True, text=True, timeout=30
        )
        assert run.returncode == 0, run.stderr
    if shutil.which("g++"):
        path = tmp_path / "scalars.cpp"
        path.write_text(
            "#define VERA_GPU_HOST\n#include <cassert>\n"
            + generate_cuda(prog)
            + "\nint main(){\n"
            + "\n".join(cpp_lines)
            + "\n}\n"
        )
        compiled = subprocess.run(
            ["g++", "-std=c++17", "-O1", str(path), "-o", str(tmp_path / "gpu-case")],
            capture_output=True,
            text=True,
            timeout=120,
        )
        assert compiled.returncode == 0, compiled.stderr
        run = subprocess.run(
            [str(tmp_path / "gpu-case")], capture_output=True, text=True, timeout=30
        )
        assert run.returncode == 0, run.stderr
