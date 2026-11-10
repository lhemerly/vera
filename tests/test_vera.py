"""VERA v1.0 test-suite.

Run with:  .venv/bin/python -m pytest tests -q
"""

import json
import os
import subprocess
import sys

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

from vera.parser import parse_file, parse_source
from vera.semantic import analyze
from vera.smt_emit import (SmtConfig, SmtEncoder, verify_transition,
                           verify_transition_dual)
from vera.verifier import attach_counterexample
from vera.runtime import Machine, Revert
from vera.fuzz import fuzz_module
from vera.projection import build_affordance_frame
from vera.types import DEC64, id64


def load(path):
    return analyze(parse_file(os.path.join(ROOT, path)))


# ---------------------------------------------------------------------------
# Front-end / semantics
# ---------------------------------------------------------------------------

def test_parse_bank():
    prog = load("examples/bank.vera")
    assert not prog.diags.has_errors()
    assert set(prog.tables) == {"Account", "Order"}
    assert set(prog.transitions) == {"transfer", "freeze", "place_order"}
    assert set(prog.invariants) == {"non_negative_balance", "balance_bounded",
                                    "order_owner_live"}
    assert len(prog.projections) == 1


def test_type_errors_are_reported():
    src = """
    module Bad;
    table T capacity 8 { key id : id64<T>; col v : u64; }
    transition t(k: id64<T>)
      reads [T[k].v]
      writes [T[k].v]
      body {
        let %x: bool = add(1, 2);
        table_set(T, k, v, %x);
        commit;
      }
    """
    prog = analyze(parse_source(src))
    assert prog.diags.has_errors()


def test_access_footprint_extraction():
    prog = load("examples/bank.vera")
    ti = prog.transitions["transfer"]
    assert ti.tier == 1
    assert any(r.table == "Account" and r.column == "balance" for r in ti.reads)
    assert any(w.table == "Account" and w.column == "balance" for w in ti.writes)
    assert ("Account", "balance") in ti.agg_writes
    assert prog.transitions["place_order"].tier == 2


def test_wcet_bound():
    prog = load("examples/bank.vera")
    assert prog.transitions["transfer"].wcet_bound == 10
    assert prog.transitions["freeze"].wcet_bound == 3


def test_capacity_must_be_power_of_two():
    src = "module M; table T capacity 7 { key id : id64<T>; col v : u64; }"
    prog = analyze(parse_source(src))
    assert prog.diags.has_errors()


def test_fold_depth_and_bound():
    src = """
    module M;
    table T capacity 8 { key id : id64<T>; col v : u64; }
    transition t()
      reads [] writes []
      body {
        let %x: u64 = fold_bounded<2000>(acc: u64 = 0, i in [0..1]) {
          yield add(acc, i);
        };
        commit;
      }
    """
    prog = analyze(parse_source(src))
    assert any("N_max" in d.message or "exceeds" in d.message
               for d in prog.diags.errors)


# ---------------------------------------------------------------------------
# Verification
# ---------------------------------------------------------------------------

def test_correct_bank_verifies():
    prog = load("examples/bank.vera")
    for name in prog.transitions:
        result = verify_transition_dual(prog, name, SmtConfig(timeout_ms=20000))
        assert result.ok, result.summary()


def test_buggy_transfer_violates_conservation():
    prog = load("examples/bank_buggy.vera")
    result = verify_transition_dual(prog, "transfer", SmtConfig(timeout_ms=20000))
    assert result.safety.status == "sat"
    assert result.panic.status == "unsat"
    assert result.liveness.status == "sat"
    result = attach_counterexample(prog, result.encoders, result)
    assert result.counterexample is not None
    text = result.counterexample.render()
    assert "conservation_of_funds" in text
    assert "from = " in text and "to = " in text
    # the counterexample must exhibit the self-transfer aliasing
    assert result.counterexample.params.get("from") == result.counterexample.params.get("to")


def test_vacuous_transition_rejected():
    src = """
    module V;
    table T capacity 8 { key id : id64<T>; col v : u64; }
    transition dead(k: id64<T>)
      reads [T[k].v] writes [T[k].v]
      body {
        guard(eq(1, 0), ERR_REJECT);
        table_set(T, k, v, 1);
        commit;
      }
    """
    prog = analyze(parse_source(src))
    result = verify_transition_dual(prog, "dead", SmtConfig(timeout_ms=5000))
    assert result.liveness.status == "unsat"


def test_missing_row_trap_detected():
    src = """
    module M;
    table T capacity 8 { key id : id64<T>; col v : u64; }
    transition t(k: id64<T>)
      reads [T[k].v] writes [T[k].v]
      body {
        let %v: u64 = table_get(T, k, v);
        table_set(T, k, v, %v);
        commit;
      }
    """
    prog = analyze(parse_source(src))
    result = verify_transition_dual(prog, "t", SmtConfig(timeout_ms=5000))
    assert result.panic.status == "sat"


def test_smt_script_emission():
    prog = load("examples/bank.vera")
    enc = SmtEncoder(prog, "freeze", SmtConfig())
    enc.build()
    script = enc.script_full()
    assert "(set-logic QF_ABV)" in script
    assert "(declare-fun Account_occ" in script
    assert "check-sat" in script


# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------

def _seeded_machine():
    prog = load("examples/bank.vera")
    machine = Machine(prog)
    acc = machine.state.tables["Account"]
    for slot, bal in [(0, 500_000000), (1, 300_000000)]:
        acc.occ[slot] = 1
        acc.gen[slot] = 1
        acc.columns["balance"][slot] = bal
        acc.columns["frozen"][slot] = False
    machine.recompute_aggregates()
    return prog, machine, (1 << 5) | 0, (1 << 5) | 1


def test_runtime_transfer_commits_and_conserves():
    prog, machine, k0, k1 = _seeded_machine()
    machine.call("transfer", {"from": k0, "to": k1, "amount": 100_000000})
    acc = machine.state.tables["Account"]
    assert acc.columns["balance"][0] == 400_000000
    assert acc.columns["balance"][1] == 400_000000
    assert acc.agg["balance"] == 800_000000


def test_runtime_revert_is_atomic():
    prog, machine, k0, k1 = _seeded_machine()
    with pytest.raises(Revert) as exc:
        machine.call("transfer", {"from": k0, "to": k1, "amount": 600_000000})
    assert exc.value.code == "ERR_INSUFFICIENT"
    acc = machine.state.tables["Account"]
    assert acc.columns["balance"][0] == 500_000000
    assert acc.columns["balance"][1] == 300_000000
    assert acc.agg["balance"] == 800_000000


def test_runtime_self_transfer_reverts():
    prog, machine, k0, _ = _seeded_machine()
    with pytest.raises(Revert) as exc:
        machine.call("transfer", {"from": k0, "to": k0, "amount": 1})
    assert exc.value.code == "ERR_SELF_TRANSFER"


def test_runtime_freeze_and_double_freeze():
    prog, machine, k0, _ = _seeded_machine()
    machine.call("freeze", {"acct": k0})
    assert machine.state.tables["Account"].columns["frozen"][0] is True
    with pytest.raises(Revert):
        machine.call("freeze", {"acct": k0})


def test_runtime_alloc_and_generation():
    prog, machine, k0, _ = _seeded_machine()
    machine.call("place_order", {"buyer": k0, "amount": 100_000000})
    order = machine.state.tables["Order"]
    assert order.occ[0] == 1
    assert order.gen[0] == 1
    assert order.columns["owner"][0] == k0
    key = (1 << 3) | 0
    assert order.valid(key)


# ---------------------------------------------------------------------------
# Projection
# ---------------------------------------------------------------------------

def test_affordance_frame():
    prog, machine, k0, _ = _seeded_machine()
    frame = build_affordance_frame(prog, machine.state, "AccountView", {"from": k0})
    assert frame.allowed
    data = frame.to_json()
    assert data["state_slice"]["Account.balance[32]"]["value"] == 500_000000
    transfer = next(a for a in data["affordances"] if a["transition"] == "transfer")
    assert transfer["enabled"] is True
    assert transfer["bounds"]["amount"]["min"] == 1


# ---------------------------------------------------------------------------
# Fuzzer
# ---------------------------------------------------------------------------

def test_fuzzer_clean_on_correct_spec():
    prog = load("examples/bank.vera")
    result = fuzz_module(prog, steps=16, threads=32, seed=7)
    assert result.ok, result.render()


def test_fuzzer_finds_buggy_spec():
    prog = load("examples/bank_buggy.vera")
    result = fuzz_module(prog, steps=16, threads=32, seed=7)
    assert not result.ok


# ---------------------------------------------------------------------------
# Code generation
# ---------------------------------------------------------------------------

def test_rust_codegen_compiles_and_runs(tmp_path):
    rustc = shutil_which("rustc")
    if rustc is None:
        pytest.skip("rustc not available")
    from vera.codegen_rust import generate_rust
    prog = load("examples/bank.vera")
    out = tmp_path / "bank.rs"
    out.write_text(generate_rust(prog))
    binary = tmp_path / "bank"
    proc = subprocess.run([rustc, "--edition", "2021", "-O", str(out),
                           "-o", str(binary)], capture_output=True, text=True)
    assert proc.returncode == 0, proc.stderr
    run = subprocess.run([str(binary)], capture_output=True, text=True)
    assert run.returncode == 0, run.stderr
    assert "transfer committed" in run.stdout
    assert "balance[0] = 400000000" in run.stdout
    assert "agg_sum_balance = 800000000" in run.stdout


def test_gpu_codegen_emits():
    from vera.codegen_gpu import generate_cuda, generate_wgsl
    prog = load("examples/bank.vera")
    cu = generate_cuda(prog)
    wgsl = generate_wgsl(prog)
    assert "xoshiro256" in cu or "xoshiro" in cu
    assert "vera_guard" in cu
    assert "workgroup_size(32)" in wgsl
    assert "fn furnace" in wgsl


def shutil_which(name):
    import shutil
    return shutil.which(name)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def test_cli_check_and_verify():
    env = dict(os.environ)
    env["PYTHONPATH"] = ROOT
    py = os.path.join(ROOT, ".venv", "bin", "python")
    if not os.path.exists(py):
        py = sys.executable
    check = subprocess.run([py, "-m", "vera", "check", "examples/bank.vera"],
                           cwd=ROOT, capture_output=True, text=True, env=env)
    assert check.returncode == 0, check.stderr
    verify = subprocess.run([py, "-m", "vera", "verify", "examples/bank.vera",
                             "--timeout", "20000"],
                            cwd=ROOT, capture_output=True, text=True, env=env)
    assert verify.returncode == 0, verify.stderr
    assert verify.stdout.count("VERDICT: VERIFIED") == 3
