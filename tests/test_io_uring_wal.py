"""io_uring intent WAL end-to-end (rust/vera-rt crate)."""

import os
import shutil
import subprocess

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
cargo = shutil.which("cargo")


@pytest.mark.skipif(cargo is None, reason="cargo not available")
def test_io_uring_wal_deterministic_replay(tmp_path):
    from vera.parser import parse_file
    from vera.semantic import analyze
    from vera.codegen_rust import generate_rust

    prog = analyze(parse_file(os.path.join(ROOT, "examples", "bank.vera")))
    crate = tmp_path / "vera-rt"
    shutil.copytree(
        os.path.join(ROOT, "rust", "vera-rt"),
        crate,
        ignore=shutil.ignore_patterns("target", ".cargo"),
    )
    (crate / "src" / "spec.rs").write_text(
        generate_rust(prog, use_rt=True, include_main=False)
    )
    build = subprocess.run(
        [cargo, "build", "--offline"], cwd=crate, capture_output=True, text=True
    )
    if build.returncode != 0:
        # Fresh checkouts may not have the libc crate cached; retry online.
        build = subprocess.run(
            [cargo, "build"], cwd=crate, capture_output=True, text=True
        )
    assert build.returncode == 0, build.stderr
    run_dir = tmp_path / "run"
    run_dir.mkdir()
    run = subprocess.run(
        [str(crate / "target" / "debug" / "vera-furnace")],
        cwd=run_dir,
        capture_output=True,
        text=True,
        timeout=90,
    )
    assert run.returncode == 0, run.stderr
    # Sandboxed and older kernels can legitimately deny io_uring. Both
    # backends must implement the identical durable WAL and replay contract.
    assert "wal backend: io_uring" in run.stdout or "wal backend: pwrite" in run.stdout
    assert "replay matches live state: deterministic" in run.stdout
    wal = run_dir / "vera.wal"
    # Program identity is part of every typed argument payload. Banking
    # intents therefore occupy 128-byte frames in the versioned WAL format.
    assert wal.exists() and wal.stat().st_size == 4 * 128
    again = subprocess.run(
        [str(crate / "target" / "debug" / "vera-furnace")],
        cwd=run_dir,
        capture_output=True,
        text=True,
        timeout=90,
    )
    assert again.returncode == 0, again.stdout + again.stderr
    assert "wal tx_seq = 8" in again.stdout
    assert "replay records = 8" in again.stdout
    assert "replay matches live state: deterministic" in again.stdout
    assert wal.stat().st_size == 8 * 128
