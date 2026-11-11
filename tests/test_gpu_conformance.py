"""GPU targets execute the language rather than merely emitting target markers."""

import os
from pathlib import Path
import shutil
import subprocess

import pytest

from vera.parser import parse_source
from vera.semantic import analyze
from vera.codegen_gpu import generate_cuda, generate_wgsl
from vera.codegen_glsl import generate_glsl


def program(source):
    p = analyze(parse_source(source))
    assert not p.diags.has_errors(), p.diags.render()
    return p


WIDE = """module Wide;
table T capacity 2 { key id: id64<T>; col value: dec128; col code: bytes32 [unique]; }
invariant bounded { forall k in T : ge(T[k].value, 0); }
transition create(value:dec128, code:bytes32) reads [] writes [T[*].value,T[*].code]
 require positive: ge(value,0);
 body {let %k:id64<T> = table_alloc(T,{value:value,code:code});commit;}
"""


def test_cuda_wide_arithmetic_and_atomic_execution(tmp_path):
    compiler = shutil.which("g++")
    if compiler is None:
        pytest.skip("C++ compiler unavailable")
    p = program(WIDE)
    code = generate_cuda(p)
    src = tmp_path / "gpu.cpp"
    src.write_text("#define VERA_GPU_HOST\n" + code + "\nint main(){return 0;}\n")
    result = subprocess.run(
        [compiler, "-std=c++17", "-O0", str(src), "-o", str(tmp_path / "gpu")],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr


def test_all_width_glsl_compiles(tmp_path):
    compiler = shutil.which("glslangValidator")
    if compiler is None:
        pytest.skip("GLSL compiler unavailable")
    src = tmp_path / "wide.comp"
    src.write_text(generate_glsl(program(WIDE), depth=2))
    result = subprocess.run([compiler, str(src)], capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr


def compile_run_cpp(tmp_path, p, main):
    compiler = shutil.which("g++")
    if compiler is None:
        pytest.skip("C++ compiler unavailable")
    source = tmp_path / "check.cpp"
    source.write_text(
        "#define VERA_GPU_HOST\n#include <cassert>\n"
        + generate_cuda(p)
        + "\nint main(){"
        + main
        + "}\n"
    )
    compiled = subprocess.run(
        [compiler, "-std=c++17", "-O1", str(source), "-o", str(tmp_path / "check")],
        capture_output=True,
        text=True,
    )
    assert compiled.returncode == 0, compiled.stderr
    run = subprocess.run(
        [str(tmp_path / "check")], capture_output=True, text=True, timeout=30
    )
    assert run.returncode == 0, run.stderr


def const(value):
    value = value % (1 << 256)
    return (
        "v_make("
        + ",".join(str((value >> (i * 32)) & 0xFFFFFFFF) + "u" for i in range(8))
        + ")"
    )


def test_limb_arithmetic_matches_mathematical_results(tmp_path):
    p = program(WIDE)
    cases = [
        (255, 1, 0, 8, False, 1, None),
        ((1 << 64) - 1, (1 << 64) - 1, 2, 64, False, 1, None),
        (-7, 3, 3, 64, True, 1, -2),
        (-7, 3, 4, 64, True, 1, -1),
        (-(1 << 63), -1, 4, 64, True, 1, None),
        (3 * 10**18, 2 * 10**18, 2, 128, True, 10**18, 6 * 10**18),
        (3 * 10**18, 2 * 10**18, 3, 128, True, 10**18, 15 * 10**17),
        (10**18, 3 * 10**18, 3, 128, True, 10**18, None),
        (1 << 100, 10**18, 2, 128, True, 10**18, 1 << 100),
        (1 << 200, 1 << 40, 2, 256, False, 1, 1 << 240),
        (1 << 240, 1 << 20, 2, 256, False, 1, None),
        (1 << 240, 1 << 100, 3, 256, False, 1, 1 << 140),
    ]
    lines = []
    for a, b, op, width, signed, scale, expected in cases:
        lines.append(
            "{R r=v_arith("
            + ",".join(
                [
                    const(a),
                    const(b),
                    str(op) + "u",
                    str(width) + "u",
                    str(signed).lower(),
                    const(scale),
                ]
            )
            + ");"
        )
        lines.append(
            "assert(r.ok); assert(v_eq(r.value," + const(expected) + "));}"
            if expected is not None
            else "assert(!r.ok);}"
        )
    compile_run_cpp(tmp_path, p, "\n".join(lines))


def test_gpu_wide_values_and_unique_allocation_are_not_truncated(tmp_path):
    p = program(WIDE)
    a = (1 << 100) + 1
    b = (1 << 101) + 2
    k1 = 7 + (1 << 240)
    k2 = 7 + (2 << 240)
    main = "Ctx ctx;vera_init(&ctx);"
    main += "assert(transition_v_create(&ctx," + const(a) + "," + const(k1) + ")==1u);"
    main += "assert(transition_v_create(&ctx," + const(b) + "," + const(k2) + ")==1u);"
    main += (
        "assert(v_eq(ctx.s.cols[0],"
        + const(a)
        + "));assert(v_eq(ctx.s.cols[1],"
        + const(b)
        + "));"
    )
    main += (
        "assert(v_eq(ctx.s.cols[2],"
        + const(k1)
        + "));assert(v_eq(ctx.s.cols[3],"
        + const(k2)
        + "));"
    )
    main += "assert(transition_v_create(&ctx," + const(a) + "," + const(9) + ")==0u);"
    compile_run_cpp(tmp_path, p, main)


def test_gpu_reverts_writes_indexes_and_effects_atomically(tmp_path):
    p = program("""module Atomic;
 table T capacity 2 {key id:id64<T>;col value:i64;col code:u64 [unique];}
 effect Changed(value:i64);
 transition create(value:i64,code:u64,accept:bool) reads [] writes [T[*].value,T[*].code]
 body {let %k:id64<T> = table_alloc(T,{value:value,code:code});emit Changed(value);guard(accept,ERR_REJECT);commit;}
 """)
    main = "Ctx ctx;vera_init(&ctx);"
    main += (
        "assert(transition_v_create(&ctx,"
        + const(99)
        + ","
        + const(7)
        + ",v_bool(false))==0u);"
    )
    main += "assert(ctx.s.occ[0]==0u);assert(ctx.s.gen[0]==0u);assert(ctx.s.effect_count==0u);"
    main += (
        "assert(transition_v_create(&ctx,"
        + const(42)
        + ","
        + const(7)
        + ",v_bool(true))==1u);"
    )
    main += (
        "assert(ctx.s.effect_count==1u);assert(v_eq(ctx.s.cols[0]," + const(42) + "));"
    )
    compile_run_cpp(tmp_path, p, main)


@pytest.fixture(scope="module")
def gpu_device():
    wgpu = pytest.importorskip("wgpu")
    try:
        adapter = wgpu.gpu.request_adapter_sync(force_fallback_adapter=True)
        if adapter is None:
            pytest.skip("no WebGPU adapter")
        device = adapter.request_device_sync()
    except Exception as exc:
        pytest.skip("WebGPU adapter unavailable: " + str(exc))
    return wgpu, device


def dispatch_wgsl(gpu_device, source, entry="probe"):
    import struct

    wgpu, device = gpu_device
    module = device.create_shader_module(code=source)
    pipeline = device.create_compute_pipeline(
        layout="auto", compute={"module": module, "entry_point": entry}
    )
    buf = device.create_buffer(
        size=16,
        usage=wgpu.BufferUsage.STORAGE
        | wgpu.BufferUsage.COPY_SRC
        | wgpu.BufferUsage.COPY_DST,
    )
    group = device.create_bind_group(
        layout=pipeline.get_bind_group_layout(0),
        entries=[{"binding": 0, "resource": {"buffer": buf, "offset": 0, "size": 16}}],
    )
    enc = device.create_command_encoder()
    comp = enc.begin_compute_pass()
    comp.set_pipeline(pipeline)
    comp.set_bind_group(0, group)
    comp.dispatch_workgroups(1)
    comp.end()
    device.queue.submit([enc.finish()])
    return struct.unpack("<4I", device.queue.read_buffer(buf))


def test_wgsl_executes_full_dec128_bytes32_and_atomic_unique_index(gpu_device):
    p = program(WIDE)
    value = (1 << 100) + 42
    code = (123 << 224) + 7
    shader = (
        generate_wgsl(p, depth=1)
        + """
@compute @workgroup_size(1)
fn probe(){
 var ctx:Ctx;vera_init(&ctx);
 var status:u32=transition_v_create(&ctx,VALUE,CODE);
 var rejected:u32=transition_v_create(&ctx,VALUE,CODE);
 atomicStore(&output.violations,status);
 atomicStore(&output.committed,ctx.s.cols[0].w[3]);
 atomicStore(&output.reverted,ctx.s.cols[2].w[7]);
 atomicStore(&output.effects,ctx.s.occ[0]+ctx.s.occ[1]+rejected);
}
""".replace("VALUE", const(value)).replace("CODE", const(code))
    )
    assert dispatch_wgsl(gpu_device, shader) == (1, 16, 123, 1)


def test_wgsl_furnace_executes_and_checks_complex_invariants(gpu_device):
    p = program("""module Broken;
 table T capacity 1 {key id:id64<T>;col value:u64;}
 invariant zero {forall k in T: and(eq(T[k].value,0),eq(rem(T[k].value,2),0));}
 transition bad() reads [] writes [T[*].value]
 body {let %k:id64<T> = table_alloc(T,{value:1});commit;}
 """)
    violations, committed, reverted, effects = dispatch_wgsl(
        gpu_device, generate_wgsl(p, depth=2), "furnace"
    )
    assert (violations, committed, reverted, effects) == (32, 32, 0, 0)


def test_wgsl_furnace_reports_arithmetic_traps(gpu_device):
    p = program("""module Panic;
 table T capacity 1 {key id:id64<T>;col value:u64;}
 transition bad() reads [] writes []
 body {let %x:u64=div(1,0);commit;}
 """)
    assert dispatch_wgsl(gpu_device, generate_wgsl(p, depth=1), "furnace") == (
        32,
        0,
        32,
        0,
    )


def test_gpu_old_key_fold_locals_read_the_pre_state(tmp_path, gpu_device):
    p = program("""module OldFold;
 table T capacity 2 {key id:id64<T>;col owner:id64<T>;col value:i64;col result:i64;}
 transition go(k:id64<T>) reads [T[*].owner,T[*].value] writes [T[k].owner,T[k].result]
 require live:table_exists(T,k);
 body {
 table_set(T,k,owner,key_of<T>(3));
 let %v:i64=old(T[fold_bounded<1>(acc:id64<T>=k,i in [0..0]) {
   let %owner:id64<T>=table_get(T,k,owner);
   guard(table_exists(T,%owner),ERR_MISSING);
   yield %owner;
 }].value);
 table_set(T,k,result,%v);commit;}
 """)
    init = "Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(2u);ctx.s.cols[2]=v_small(7u);"
    compile_run_cpp(
        tmp_path,
        p,
        init
        + "assert(transition_v_go(&ctx,v_small(2u))==1u);assert(v_eq(ctx.s.cols[4],v_small(7u)));",
    )
    shader = (
        generate_wgsl(p, depth=1)
        + """
@compute @workgroup_size(1)
fn probe(){
 var ctx:Ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(2u);ctx.s.cols[2]=v_small(7u);
 var status:u32=transition_v_go(&ctx,v_small(2u));
 atomicStore(&output.violations,status);atomicStore(&output.committed,ctx.s.cols[4].w[0]);
 atomicStore(&output.reverted,ctx.s.cols[0].w[0]);atomicStore(&output.effects,ctx.trap);
}
"""
    )
    assert dispatch_wgsl(gpu_device, shader) == (1, 7, 3, 0)
