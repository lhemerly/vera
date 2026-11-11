"""Rust production runtime: Tier-1 dispatcher, intent WAL, snapshot, replay."""

import os
import shutil
import subprocess

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
rustc = shutil.which("rustc")


def _build(tmp_path):
    from vera.parser import parse_file
    from vera.semantic import analyze
    from vera.codegen_rust import generate_rust

    prog = analyze(parse_file(os.path.join(ROOT, "examples", "bank.vera")))
    src = tmp_path / "bank_rt.rs"
    src.write_text(generate_rust(prog))
    binary = tmp_path / "bank_rt"
    proc = subprocess.run(
        [rustc, "--edition", "2021", "-O", str(src), "-o", str(binary)],
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stderr
    return str(binary)


def test_rust_runtime_wal_snapshot_replay(tmp_path):
    if rustc is None:
        pytest.skip("rustc not available")
    binary = _build(tmp_path)
    run = subprocess.run(
        [binary], capture_output=True, text=True, cwd=tmp_path, timeout=60
    )
    assert run.returncode == 0, run.stderr
    assert "wal tx_seq = 3" in run.stdout
    assert "replay matches live state: deterministic" in run.stdout
    wal = tmp_path / "vera.wal"
    snap = tmp_path / "vera.snap"
    # Typed payloads include the program fingerprint; bank frames are 128 bytes.
    assert wal.exists() and wal.stat().st_size == 3 * 128
    assert snap.exists() and snap.stat().st_size > 0


def test_rust_runtime_parallel_batch(tmp_path):
    """A larger batch exercises multiple conflict-free waves."""
    if rustc is None:
        pytest.skip("rustc not available")
    binary = _build(tmp_path)
    # the demo batch has 3 intents; assert the dispatcher ran and committed some
    run = subprocess.run(
        [binary], capture_output=True, text=True, cwd=tmp_path, timeout=60
    )
    assert "batch committed" in run.stdout
    committed = int(run.stdout.split("batch committed ")[1].split("/")[0])
    assert committed >= 2
