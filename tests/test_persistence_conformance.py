"""Behavioral checks for the durable intent format and recovery protocol."""

from pathlib import Path
import shutil
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[1]
CARGO = shutil.which("cargo")


def _run(tmp_path, source):
    if CARGO is None:
        pytest.skip("cargo not available")
    crate = tmp_path / "harness"
    (crate / "src").mkdir(parents=True)
    (crate / "Cargo.toml").write_text(
        '[package]\nname="persistence-conformance"\nversion="0.0.0"\nedition="2021"\n'
        '[dependencies]\nvera-rt={path="' + str(ROOT / "rust" / "vera-rt") + '"}\n'
    )
    (crate / "src" / "main.rs").write_text(source)
    run = subprocess.run(
        [CARGO, "run", "--offline", "--quiet"],
        cwd=crate,
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert run.returncode == 0, run.stdout + run.stderr
    return run.stdout


def test_wal_replay_uses_the_open_log_and_resumes_sequence(tmp_path):
    _run(
        tmp_path,
        r"""
use vera_rt::{IoUringWAL, replay_file};
fn main() {
    let path="local.wal";
    let mut wal=IoUringWAL::open(path).unwrap();
    wal.append(7,[1u64,2,3,4]).unwrap();
    wal.sync().unwrap();
    let records=wal.replay().unwrap();
    assert_eq!(records.len(),1);
    assert_eq!(records[0].transition,7);
    assert_eq!(records[0].tx_seq,1);
    drop(wal);
    let mut wal=IoUringWAL::open(path).unwrap();
    assert_eq!(wal.tx_seq,1);
    wal.append(8,[5u64,6,7,8]).unwrap();
    wal.sync().unwrap();
    let records=replay_file(path).unwrap();
    assert_eq!(records.len(),2);
    assert_eq!(records[1].tx_seq,2);
}
""",
    )


def test_wal_keeps_full_payloads_and_recovers_only_incomplete_tail(tmp_path):
    _run(
        tmp_path,
        r"""
use vera_rt::{IoUringWAL,replay_file};
use std::io::Write;
fn main() {
    let path="payload.wal";
    let payload:Vec<u8>=(0..255).collect();
    let mut wal=IoUringWAL::open(path).unwrap();
    wal.append(70000,&payload).unwrap();
    wal.sync().unwrap();
    assert_eq!(wal.replay().unwrap()[0].args,payload);
    let valid_len=std::fs::metadata(path).unwrap().len();
    drop(wal);
    std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(b"VERA\x02").unwrap();
    assert_eq!(replay_file(path).unwrap().len(),1);
    let mut wal=IoUringWAL::open(path).unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(),valid_len);
    wal.append(90000,b"next").unwrap();
    wal.sync().unwrap();
    let recs=replay_file(path).unwrap();
    assert_eq!(recs.len(),2);
    assert_eq!(recs[1].tx_seq,2);
    assert_eq!(recs[1].args,b"next");
}
""",
    )


def test_wal_rejects_checksum_corruption_and_reordered_records(tmp_path):
    _run(
        tmp_path,
        r"""
use vera_rt::{IoUringWAL,replay_file};
fn main() {
    let mut wal=IoUringWAL::open("valid.wal").unwrap();
    wal.append(5,b"content").unwrap();wal.sync().unwrap();drop(wal);
    let valid=std::fs::read("valid.wal").unwrap();
    let mut bad=valid.clone();bad[40]^=1;
    std::fs::write("corrupt.wal",bad).unwrap();
    assert_eq!(replay_file("corrupt.wal").unwrap_err().kind(),std::io::ErrorKind::InvalidData);
    assert!(IoUringWAL::open("corrupt.wal").is_err());
    let mut length_corrupt=valid.clone();length_corrupt[9]^=1;
    std::fs::write("bad-length.wal",length_corrupt).unwrap();
    assert_eq!(replay_file("bad-length.wal").unwrap_err().kind(),std::io::ErrorKind::InvalidData);
    assert!(IoUringWAL::open("bad-length.wal").is_err());
    let mut duplicate=valid.clone();duplicate.extend_from_slice(&valid);
    std::fs::write("duplicate.wal",duplicate).unwrap();
    assert_eq!(replay_file("duplicate.wal").unwrap_err().kind(),std::io::ErrorKind::InvalidData);
    assert!(IoUringWAL::open("duplicate.wal").is_err());
}
""",
    )


def test_wal_has_one_writer_and_does_not_advance_on_append_failure(tmp_path):
    _run(
        tmp_path,
        r"""
use vera_rt::IoUringWAL;
fn main() {
    let mut wal=IoUringWAL::open("locked.wal").unwrap();
    assert!(IoUringWAL::open("locked.wal").is_err());
    wal.append(4,b"ok").unwrap();wal.sync().unwrap();
    assert_eq!(wal.tx_seq,1);
    drop(wal);
    assert!(IoUringWAL::open("locked.wal").is_ok());
}
""",
    )


def test_standalone_and_crate_share_the_same_wal_implementation():
    canonical = (ROOT / "vera" / "templates" / "wal.rs").read_bytes()
    assert (ROOT / "rust" / "vera-rt" / "src" / "wal.rs").read_bytes() == canonical
