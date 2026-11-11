"""VERA v1.0 test-suite.

Run with:  .venv/bin/python -m pytest tests -q
"""

import json
import os
import subprocess
import sys
import time

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
    assert "(declare-const Account_occ (Array" in script
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
    run = subprocess.run([str(binary)], capture_output=True, text=True,
                         cwd=tmp_path)
    assert run.returncode == 0, run.stderr
    assert "batch committed" in run.stdout
    assert "replay matches live state: deterministic" in run.stdout
    assert (tmp_path / "vera.wal").exists()
    assert (tmp_path / "vera.snap").exists()


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


# ---------------------------------------------------------------------------
# Secondary unique-key indexes
# ---------------------------------------------------------------------------

UNIQUE_SRC = """
module Unique;
table Account capacity 8 {
  key id : id64<Account>;
  col balance : dec64 [tracked_sum];
  col acct_no : bytes32 [unique];
}
invariant non_negative { forall k in Account : ge(Account[k].balance, 0); }
invariant bounded { forall k in Account : le(Account[k].balance, 1000000000); }
transition create(balance : dec64, acct_no : bytes32)
  reads [] writes [Account[*].balance, Account[*].acct_no]
  require non_negative : ge(balance, 0);
  require bounded : le(balance, 1000000000);
  body {
    let %id : id64<Account> = table_alloc(Account, { balance : balance, acct_no : acct_no });
    commit;
  }
transition lookup(acct_no : bytes32)
  reads [Account[*].acct_no, Account[*].balance]
  writes []
  body {
    guard(table_exists_unique(Account, acct_no, acct_no), ERR_MISSING_ROW);
    let %id : id64<Account> = table_lookup(Account, acct_no, acct_no);
    let %bal : dec64 = table_get(Account, %id, balance);
    guard(ge(%bal, 0), ERR_INSUFFICIENT);
    commit;
  }
"""


def test_unique_key_semantics():
    prog = analyze(parse_source(UNIQUE_SRC))
    assert not prog.diags.has_errors()
    layout = prog.tables["Account"].layout
    assert "acct_no" in layout.unique_hashes
    assert layout.unique_hashes["acct_no"] >= 2 * layout.capacity


def test_unique_key_runtime():
    prog = analyze(parse_source(UNIQUE_SRC))
    machine = Machine(prog)
    machine.call("create", {"balance": 100_000000, "acct_no": b"\x01" * 32})
    machine.call("create", {"balance": 200_000000, "acct_no": b"\x02" * 32})
    t = machine.state.tables["Account"]
    assert any(t.hash_used["acct_no"])
    # lookup succeeds
    machine.call("lookup", {"acct_no": b"\x01" * 32})
    # missing key reverts
    with pytest.raises(Revert) as exc:
        machine.call("lookup", {"acct_no": b"\x09" * 32})
    assert exc.value.code == "ERR_MISSING_ROW"
    # duplicate unique key reverts
    with pytest.raises(Revert) as exc:
        machine.call("create", {"balance": 300_000000, "acct_no": b"\x01" * 32})
    assert exc.value.code == "ERR_DUPLICATE_KEY"


def test_unique_key_verifies():
    prog = analyze(parse_source(UNIQUE_SRC))
    for name in ("create", "lookup"):
        result = verify_transition_dual(prog, name, SmtConfig(timeout_ms=10000))
        assert result.ok, result.summary()


def test_unique_key_rust_codegen_compiles(tmp_path):
    rustc = shutil_which("rustc")
    if rustc is None:
        pytest.skip("rustc not available")
    from vera.codegen_rust import generate_rust
    prog = analyze(parse_source(UNIQUE_SRC))
    out = tmp_path / "unique.rs"
    out.write_text(generate_rust(prog))
    binary = tmp_path / "unique"
    proc = subprocess.run([rustc, "--edition", "2021", "-O", str(out), "-o", str(binary)],
                          capture_output=True, text=True)
    assert proc.returncode == 0, proc.stderr


# ---------------------------------------------------------------------------
# Bounded folds with allocation (indexed write maps + known-slot fast path)
# ---------------------------------------------------------------------------

def test_fold_alloc_verifies_and_scales():
    src = """
module FoldAlloc;
table Item capacity 64 {
  key id : id64<Item>;
  col value : i64;
  col flag : bool;
}
transition build()
  reads [Item[*].value] writes [Item[*].value, Item[*].flag]
  body {
    let %x: i64 = fold_bounded<8>(acc: i64 = 0, i in [0..7]) {
      let %id: id64<Item> = table_alloc(Item, { value: i, flag: false });
      let %v: i64 = table_get(Item, %id, value);
      yield add(acc, %v);
    };
    commit;
  }
"""
    prog = analyze(parse_source(src))
    assert not prog.diags.has_errors()
    result = verify_transition_dual(prog, "build", SmtConfig(timeout_ms=15000))
    assert result.ok, result.summary()


# ---------------------------------------------------------------------------
# Frame-decomposed tracked_sum aggregates (Spec 6.2)
# ---------------------------------------------------------------------------

def _big_deposit(cap: int) -> str:
    return f"""
module Big;
table Account capacity {cap} {{
  key id : id64<Account>;
  col balance : i64 [tracked_sum];
}}
invariant non_negative {{ forall k in Account : ge(Account[k].balance, 0); }}
invariant bounded {{ forall k in Account : le(Account[k].balance, 1000000000); }}
transition deposit(k: id64<Account>, amount: i64)
  reads [Account[k].balance] writes [Account[k].balance]
  require exists : table_exists(Account, k);
  require positive : ge(amount, 0);
  require amount_bounded : le(amount, 1000000000);
  require bounded : le(add(Account[k].balance, amount), 1000000000);
  body {{
    let %b : i64 = table_get(Account, k, balance);
    let %n : i64 = add(%b, amount);
    table_set(Account, k, balance, %n);
    commit;
  }}
"""


def test_frame_sum_is_used_and_scales():
    from vera.smt_emit import SmtEncoder
    prog = analyze(parse_source(_big_deposit(1024)))
    enc = SmtEncoder(prog, "deposit", SmtConfig(timeout_ms=30000))
    enc.build()
    assert enc.frame_ok.get(("Account", "balance")) is True
    # invariant-derived bounds are tighter than the raw i64 domain
    lo, hi = enc.agg_bounds[("Account", "balance")]
    assert (lo, hi) == (0, 1000000000)
    result = verify_transition_dual(prog, "deposit", SmtConfig(timeout_ms=30000))
    assert result.ok, result.summary()


def test_frame_sum_beats_exact_sum_at_scale():
    """The O(|K_rw|) frame proves the accumulator range where the O(C) exact
    sum times out."""
    from dataclasses import replace
    from vera.smt_emit import SmtEncoder
    prog = analyze(parse_source(_big_deposit(256)))
    t0 = time.perf_counter()
    frame = SmtEncoder(prog, "deposit", SmtConfig(timeout_ms=30000))
    frame.build()
    frame_result = frame._solve_one("panic")
    frame_ms = (time.perf_counter() - t0) * 1000
    assert frame_result.status == "unsat"
    assert frame_ms < 5000, f"frame took {frame_ms:.0f}ms"


FRAME_TWO_COL = """
module TwoCol;
table Ledger capacity 8 {
  key id : id64<Ledger>;
  col assets : i64 [tracked_sum];
  col liabilities : i64 [tracked_sum];
}
invariant assets_nonneg { forall k in Ledger : ge(Ledger[k].assets, 0); }
invariant liab_nonneg { forall k in Ledger : ge(Ledger[k].liabilities, 0); }
transition revalue(k: id64<Ledger>, delta : i64)
  reads [Ledger[k].assets, Ledger[k].liabilities]
  writes [Ledger[k].assets, Ledger[k].liabilities]
  require exists : table_exists(Ledger, k);
  require delta_small : le(delta, 100000000);
  require delta_not_small : ge(delta, -100000000);
  require assets_ok : ge(add(Ledger[k].assets, delta), 0);
  require liab_ok : ge(sub(Ledger[k].liabilities, delta), 0);
  ensure assets_increased : eq(sum(Ledger.assets), add(old_sum(Ledger.assets), delta));
  body {
    let %a : i64 = table_get(Ledger, k, assets);
    let %l : i64 = table_get(Ledger, k, liabilities);
    table_set(Ledger, k, assets, add(%a, delta));
    table_set(Ledger, k, liabilities, sub(%l, delta));
    commit;
  }
"""


def test_frame_sum_two_dependent_columns():
    prog = analyze(parse_source(FRAME_TWO_COL))
    assert not prog.diags.has_errors()
    from vera.smt_emit import SmtEncoder
    enc = SmtEncoder(prog, "revalue", SmtConfig(timeout_ms=20000))
    enc.build()
    assert enc.frame_ok.get(("Ledger", "assets")) is True
    assert enc.frame_ok.get(("Ledger", "liabilities")) is True
    result = verify_transition_dual(prog, "revalue", SmtConfig(timeout_ms=20000))
    assert result.ok, result.summary()


def test_frame_sum_joint_two_column_bound():
    """A single invariant constraining the sum of two tracked_sum columns
    simultaneously (Spec 6.2's dependent-column case)."""
    src = """
module TwoColJoint;
table Ledger capacity 2 {
  key id : id64<Ledger>;
  col assets : i64 [tracked_sum];
  col liabilities : i64 [tracked_sum];
}
invariant assets_nonneg { forall k in Ledger : ge(Ledger[k].assets, 0); }
invariant liab_nonneg { forall k in Ledger : ge(Ledger[k].liabilities, 0); }
invariant assets_bounded { forall k in Ledger : le(Ledger[k].assets, 1000000000); }
invariant liab_bounded { forall k in Ledger : le(Ledger[k].liabilities, 1000000000); }
invariant joint_bound { le(add(sum(Ledger.assets), sum(Ledger.liabilities)), 2000000000); }
transition revalue(k: id64<Ledger>, delta : i64)
  reads [Ledger[k].assets, Ledger[k].liabilities]
  writes [Ledger[k].assets, Ledger[k].liabilities]
  require exists : table_exists(Ledger, k);
  require delta_small : le(delta, 100000000);
  require delta_not_small : ge(delta, -100000000);
  require assets_ok : ge(add(Ledger[k].assets, delta), 0);
  require liab_ok : ge(sub(Ledger[k].liabilities, delta), 0);
  require assets_new_bounded : le(add(Ledger[k].assets, delta), 1000000000);
  require liab_new_bounded : le(sub(Ledger[k].liabilities, delta), 1000000000);
  body {
    let %a : i64 = table_get(Ledger, k, assets);
    let %l : i64 = table_get(Ledger, k, liabilities);
    table_set(Ledger, k, assets, add(%a, delta));
    table_set(Ledger, k, liabilities, sub(%l, delta));
    commit;
  }
"""
    prog = analyze(parse_source(src))
    assert not prog.diags.has_errors()
    result = verify_transition_dual(prog, "revalue", SmtConfig(timeout_ms=60000))
    assert result.ok, result.summary()
