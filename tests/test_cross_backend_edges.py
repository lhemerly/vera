"""Cross-target state observations for evaluation order and wide arithmetic."""

import shutil
import subprocess

import pytest

from vera.codegen_gpu import generate_cuda
from vera.codegen_rust import generate_rust
from vera.parser import parse_source
from vera.runtime import Machine
from vera.semantic import analyze
from vera.smt_emit import SmtConfig, verify_transition_dual


def program(source):
    p = analyze(parse_source(source))
    assert not p.diags.has_errors(), p.diags.render()
    return p


def rust_case(tmp_path, p, body):
    if not shutil.which("rustc"):
        pytest.skip("rustc unavailable")
    source = tmp_path / "edge.rs"
    binary = tmp_path / "rust_edge"
    source.write_text(
        generate_rust(p, include_main=False) + "\nfn main(){" + body + "}\n"
    )
    compiled = subprocess.run(
        ["rustc", "--edition", "2021", "-O", str(source), "-o", str(binary)],
        capture_output=True,
        text=True,
    )
    assert compiled.returncode == 0, compiled.stderr
    result = subprocess.run([str(binary)], capture_output=True, text=True, cwd=tmp_path)
    assert result.returncode == 0, result.stderr


def gpu_case(tmp_path, p, body):
    if not shutil.which("g++"):
        pytest.skip("C++ compiler unavailable")
    source = tmp_path / "edge.cpp"
    binary = tmp_path / "gpu_edge"
    source.write_text(
        "#define VERA_GPU_HOST\n#include <cassert>\n"
        + generate_cuda(p)
        + "\nint main(){"
        + body
        + "}\n"
    )
    compiled = subprocess.run(
        ["g++", "-std=c++17", "-O1", str(source), "-o", str(binary)],
        capture_output=True,
        text=True,
    )
    assert compiled.returncode == 0, compiled.stderr
    result = subprocess.run([str(binary)], capture_output=True, text=True, cwd=tmp_path)
    assert result.returncode == 0, result.stderr


FIELD_ORDER = """module FieldOrder;
table T capacity 2 {key id:id64<T>;col v:i64;}
table R capacity 2 {key id:id64<R>;col first:i64;col second:i64;}
invariant seven {forall r in R:eq(R[r].first,7);}
transition go(k:id64<T>) reads [T[k].v] writes [T[k].v,R[*].first,R[*].second]
require live:table_exists(T,k);
body {let %r:id64<R>=table_alloc(R,{
second:fold_bounded<1>(acc:i64=0,i in [0..0]) {table_set(T,k,v,7);yield 0;},
first:table_get(T,k,v)});commit;}
"""


@pytest.mark.parametrize("target", ["python", "smt", "rust", "cuda"])
def test_named_allocation_fields_evaluate_in_source_order(target, tmp_path):
    p = program(FIELD_ORDER)
    if target == "python":
        m = Machine(p)
        t = m.state.tables["T"]
        t.occ[0] = 1
        t.gen[0] = 1
        m.call("go", {"k": 2})
        assert m.state.tables["R"].columns["first"][0] == 7
    elif target == "smt":
        proof = verify_transition_dual(p, "go", SmtConfig(timeout_ms=15000))
        assert proof.ok, proof.summary()
    elif target == "rust":
        rust_case(
            tmp_path,
            p,
            "let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;go(&mut s,2).unwrap();assert_eq!(s.r.col_first[0],7);",
        )
    else:
        gpu_case(
            tmp_path,
            p,
            "Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;assert(transition_v_go(&ctx,v_small(2u))==1u);assert(v_eq(ctx.s.cols[2],v_small(7u)));",
        )


FOLD_ORDER = """module FoldOrder;
table T capacity 2 {key id:id64<T>;col v:i64;col result:i64;}
transition go(k:id64<T>) reads [T[k].v] writes [T[k].v,T[k].result]
require live:table_exists(T,k);
ensure empty:eq(T[k].result,0);
body {let %x:i64=fold_bounded<1>(a:i64=
fold_bounded<1>(b:i64=0,j in [0..0]) {table_set(T,k,v,7);yield 0;},
i in [table_get(T,k,v)..0]) {yield 1;};table_set(T,k,result,%x);commit;}
"""


@pytest.mark.parametrize("target", ["python", "smt", "rust", "cuda"])
def test_fold_initializer_runs_before_range_bounds(target, tmp_path):
    p = program(FOLD_ORDER)
    if target == "python":
        m = Machine(p)
        t = m.state.tables["T"]
        t.occ[0] = 1
        t.gen[0] = 1
        m.call("go", {"k": 2})
        assert t.columns["result"][0] == 0
    elif target == "smt":
        proof = verify_transition_dual(p, "go", SmtConfig(timeout_ms=15000))
        assert proof.ok, proof.summary()
    elif target == "rust":
        rust_case(
            tmp_path,
            p,
            "let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;go(&mut s,2).unwrap();assert_eq!(s.t.col_result[0],0);",
        )
    else:
        gpu_case(
            tmp_path,
            p,
            "Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;assert(transition_v_go(&ctx,v_small(2u))==1u);assert(v_eq(ctx.s.cols[2],v_zero()));",
        )


def test_gpu_aggregate_arithmetic_uses_widened_domain(tmp_path):
    p = program("""module WideAggregate;
    table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
    transition fill() reads [] writes [T[*].v]
    body {let %k:id64<T>=table_alloc(T,{v:18446744073709551615});commit;}
    transition go() reads [T[*].v] writes []
    body {guard(gt(add(sum(T.v),1),sum(T.v)),ERR_BAD);commit;}
    """)
    gpu_case(
        tmp_path,
        p,
        "Ctx ctx;vera_init(&ctx);assert(transition_v_fill(&ctx)==1u);assert(transition_v_fill(&ctx)==1u);assert(transition_v_go(&ctx)==1u);",
    )


def test_old_key_reads_do_not_reuse_post_allocation_fastpaths():
    p = program("""module OldKey;table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:i64;}
    transition go() reads [T[*].owner,T[*].v] writes [T[*].owner,T[*].v]
    body {let %k:id64<T>=table_alloc(T,{owner:key_of<T>(0),v:7});
    let %v:i64=old(T[table_get(T,%k,owner)].v);commit;}""")
    proof = verify_transition_dual(p, "go", SmtConfig(timeout_ms=15000))
    assert not proof.ok
    assert proof.panic.status == "sat"
    assert proof.liveness.status == "unsat"


def test_gpu_aggregate_scalar_boundaries_reject_narrowing(tmp_path):
    p = program("""module NarrowAggregate;
    table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
    effect Seen(value:u64);
    transition fill() reads [] writes [T[*].v]
    body {let %k:id64<T>=table_alloc(T,{v:18446744073709551615});commit;}
    transition binding() reads [T[*].v] writes [] body {let %v:u64=sum(T.v);commit;}
    transition effect_arg() reads [T[*].v] writes [] body {emit Seen(sum(T.v));commit;}
    transition fold_acc() reads [T[*].v] writes []
    body {let %v:u64=fold_bounded<1>(acc:u64=sum(T.v),i in [0..0]) {yield acc;};commit;}
    transition fold_bound() reads [T[*].v] writes []
    body {let %v:u64=fold_bounded<1>(acc:u64=0,i in [sum(T.v)..0]) {yield acc;};commit;}
    transition write(k:id64<T>) reads [T[*].v] writes [T[k].v]
    require live:table_exists(T,k);body {table_set(T,k,v,sum(T.v));commit;}
    """)
    body = "Ctx ctx;vera_init(&ctx);assert(transition_v_fill(&ctx)==1u);assert(transition_v_fill(&ctx)==1u);"
    for name in ("binding", "effect_arg", "fold_acc", "fold_bound"):
        body += "assert(transition_v_" + name + "(&ctx)==3u);assert(ctx.trap==1u);"
    body += "assert(transition_v_write(&ctx,v_small(2u))==3u);assert(ctx.trap==1u);assert(ctx.s.effect_count==0u);"
    gpu_case(tmp_path, p, body)


OLD_FOLD = """module OldFold;
table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:i64;col result:i64;}
transition go(k:id64<T>) reads [T[k].owner,T[*].v] writes [T[k].owner,T[k].result]
require live:table_exists(T,k);
ensure snapshot:eq(T[k].result,old(T[table_get(T,k,owner)].v));
body {table_set(T,k,owner,key_of<T>(3));
let %v:i64=old(T[fold_bounded<1>(a:id64<T>=k,i in [0..0]) {
  let %owner:id64<T>=table_get(T,k,owner);guard(true,ERR_REJECT);yield %owner;
}].v);table_set(T,k,result,%v);commit;}
"""


@pytest.mark.parametrize("target", ["python", "smt", "rust", "cuda"])
def test_pure_fold_in_old_key_reads_pre_state_in_local_bindings(target, tmp_path):
    p = program(OLD_FOLD)
    if target == "python":
        m = Machine(p)
        t = m.state.tables["T"]
        t.occ[0] = 1
        t.gen[0] = 1
        t.columns["owner"][0] = 2
        t.columns["v"][0] = 7
        m.call("go", {"k": 2})
        assert t.columns["result"][0] == 7
    elif target == "smt":
        proof = verify_transition_dual(p, "go", SmtConfig(timeout_ms=15000))
        assert proof.ok, proof.summary()
    elif target == "rust":
        rust_case(
            tmp_path,
            p,
            "let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_owner[0]=2;s.t.col_v[0]=7;go(&mut s,2).unwrap();assert_eq!(s.t.col_result[0],7);",
        )
    else:
        gpu_case(
            tmp_path,
            p,
            "Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(2u);ctx.s.cols[2]=v_small(7u);assert(transition_v_go(&ctx,v_small(2u))==1u);assert(v_eq(ctx.s.cols[4],v_small(7u)));",
        )


def test_old_snapshot_key_rejects_mutating_folds():
    p = analyze(
        parse_source("""module M;table T capacity 2 {key id:id64<T>;col v:i64;}
    transition go(k:id64<T>) reads [T[*].v] writes [T[k].v]
    require live:table_exists(T,k);
    body {let %v:i64=old(T[fold_bounded<1>(a:id64<T>=k,i in [0..0]) {
    table_set(T,k,v,9);yield a;}].v);commit;}""")
    )
    assert p.diags.has_errors()
