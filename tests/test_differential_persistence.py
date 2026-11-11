"""Cross-process checks for program identity and restart/replay behavior."""

from pathlib import Path
import shutil
import subprocess

import pytest

from vera.codegen_rust import generate_rust
from vera.parser import parse_source
from vera.semantic import analyze

ROOT = Path(__file__).resolve().parents[1]
RUSTC = shutil.which("rustc")


def _compile(tmp_path, name, source, main=None):
    if RUSTC is None:
        pytest.skip("rustc unavailable")
    program = analyze(parse_source(source))
    assert not program.diags.has_errors(), program.diags.render()
    code = generate_rust(program, include_main=main is None)
    if main is not None:
        code += "\n" + main
    path = tmp_path / (name + ".rs")
    path.write_text(code)
    binary = tmp_path / name
    result = subprocess.run(
        [RUSTC, "--edition", "2021", "-O", str(path), "-o", str(binary)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert result.returncode == 0, result.stderr
    return binary


def _run(binary, cwd):
    result = subprocess.run(
        [str(binary)], cwd=cwd, capture_output=True, text=True, timeout=60
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


SOURCE = """
module Persist;
table T capacity 2 { key id: id64<T>; col value: u64; }
transition put(k: id64<T>, value: u64) reads [] writes [T[k].value]
  require live: table_exists(T,k);
  body { table_set(T,k,value,value); commit; }
"""


def test_wal_rejects_replay_under_different_transition_semantics(tmp_path):
    writer = _compile(
        tmp_path,
        "writer",
        SOURCE,
        """
fn main(){let mut wal=WAL::open("intents.wal").unwrap();let intent=Params::Put{k:2,value:7};let(tag,args)=intent.encode();wal.append(tag,args).unwrap();wal.sync().unwrap();}
""",
    )
    different = SOURCE.replace("table_set(T,k,value,value)", "table_set(T,k,value,0)")
    reader = _compile(
        tmp_path,
        "reader",
        different,
        """
fn main(){let records=replay_file("intents.wal").unwrap();assert_eq!(records.len(),1);let record=&records[0];assert!(Params::decode(record.transition,&record.args).is_err(),"a persisted intent must be bound to its program semantics");}
""",
    )
    _run(writer, tmp_path)
    _run(reader, tmp_path)


def test_intent_program_identity_ignores_comments_and_source_locations(tmp_path):
    writer = _compile(
        tmp_path,
        "writer",
        SOURCE,
        """
fn main(){let mut wal=WAL::open("intents.wal").unwrap();let(tag,args)=Params::Put{k:2,value:7}.encode();wal.append(tag,args).unwrap();wal.sync().unwrap();}
""",
    )
    equivalent = "// Same program, reformatted.\n\n" + SOURCE.replace(";", ";\n\n")
    reader = _compile(
        tmp_path,
        "reader",
        equivalent,
        """
fn main(){let records=replay_file("intents.wal").unwrap();let record=&records[0];let intent=Params::decode(record.transition,&record.args).unwrap();let mut state=State::new();state.t.occupied_bitmap[0]=1;state.t.generation[0]=1;intent.execute(&mut state).unwrap();assert_eq!(state.t.col_value[0],7);}
""",
    )
    _run(writer, tmp_path)
    _run(reader, tmp_path)


def test_generated_demo_recovers_previous_log_before_appending(tmp_path):
    source = (ROOT / "examples" / "bank.vera").read_text()
    binary = _compile(tmp_path, "bank", source)
    first = _run(binary, tmp_path)
    assert "wal tx_seq = 3" in first
    second = _run(binary, tmp_path)
    assert "wal tx_seq = 6" in second
    assert "replay matches live state: deterministic" in second


def test_snapshot_rejects_states_outside_the_verified_prestate_domain(tmp_path):
    source = SOURCE.replace("col value: u64", "col value: i64").replace(
        "transition put",
        "invariant nonnegative { forall k in T : ge(T[k].value,0); }\ntransition put",
    )
    source = source.replace("value: u64)", "value: i64)").replace(
        "require live:", "require nonnegative: ge(value,0);\n  require live:"
    )
    binary = _compile(
        tmp_path,
        "invalid-state",
        source,
        """
fn main(){
    let mut bad=State::new();bad.t.occupied_bitmap[0]=1;bad.t.generation[0]=1;bad.t.col_value[0]=-1;
    assert!(State::decode(bad.encode()).is_err(),"occupied rows must satisfy the program invariant");
    let mut bad=State::new();bad.t.col_value[1]=7;
    assert!(State::decode(bad.encode()).is_err(),"unoccupied columns must satisfy SMT well-formedness");
    let mut good=State::new();good.t.occupied_bitmap[0]=1;good.t.generation[0]=1;good.t.col_value[0]=7;
    assert_eq!(State::decode(good.encode()).unwrap(),good);
}
""",
    )
    _run(binary, tmp_path)


def test_snapshot_schema_migration_checks_new_invariants(tmp_path):
    writer = _compile(
        tmp_path,
        "writer",
        SOURCE,
        """
fn main(){let mut state=State::new();state.t.occupied_bitmap[0]=1;state.t.generation[0]=1;state.t.col_value[0]=7;snapshot(&state,"state.snap").unwrap();}
""",
    )
    stricter = SOURCE.replace(
        "transition put",
        "invariant zero { forall k in T : eq(T[k].value,0); }\ntransition put",
    )
    stricter = stricter.replace("table_set(T,k,value,value)", "table_set(T,k,value,0)")
    reader = _compile(
        tmp_path,
        "reader",
        stricter,
        """
fn main(){assert!(restore("state.snap").is_err(),"same-layout snapshots still must satisfy the new program invariants");}
""",
    )
    _run(writer, tmp_path)
    _run(reader, tmp_path)


def test_snapshot_detects_corruption_that_would_otherwise_be_valid_state(tmp_path):
    binary = _compile(
        tmp_path,
        "snapshot-crc",
        SOURCE,
        """
fn main(){
    let mut before=State::new();before.t.occupied_bitmap[0]=1;before.t.generation[0]=1;before.t.col_value[0]=7;
    let mut after=before.clone();after.t.col_value[0]=6;
    let mut bytes=before.encode();let different=after.encode();
    let changed=(0..bytes.len()-4).find(|&i|bytes[i]!=different[i]).unwrap();
    bytes[changed]=different[changed];
    assert!(State::decode(bytes).is_err(),"corruption must be detected even if scalar domains and invariants still hold");
    assert_eq!(State::decode(different).unwrap(),after);
}
""",
    )
    _run(binary, tmp_path)


def test_generated_demo_snapshot_satisfies_declared_initial_invariants(tmp_path):
    source = """module BoundedDemo;
    table T capacity 2 {key id:id64<T>;col value:u64;}
    invariant bounded {forall k in T:le(T[k].value,10);}
    transition create(value:u64) reads [] writes [T[*].value]
    require small:le(value,10);
    body {let %k:id64<T>=table_alloc(T,{value:value});commit;}"""
    demo = _compile(tmp_path, "demo", source)
    reader = _compile(
        tmp_path,
        "reader",
        source,
        """
    fn main(){let state=restore("vera.snap").unwrap();state.validate().unwrap();}
    """,
    )
    _run(demo, tmp_path)
    _run(reader, tmp_path)


def test_snapshot_rejects_invalid_bitmap_and_effect_without_replacing_file(tmp_path):
    source = """module SnapshotDomains;
    table T capacity 2 {key id:id64<T>;col value:u64;}
    effect Notice(state:enum{Only});
    transition noop() reads [] writes [] body {commit;}"""
    binary = _compile(
        tmp_path,
        "snapshot-domains",
        source,
        """
    fn main(){let mut state=State::new();snapshot(&state,"valid.snap").unwrap();
    let valid=std::fs::read("valid.snap").unwrap();
    state.t.occupied_bitmap[0]=1u64<<63;
    assert!(snapshot(&state,"valid.snap").is_err());
    assert_eq!(std::fs::read("valid.snap").unwrap(),valid);
    state=State::new();state.outbox.push(Effect::Notice{state:1});
    assert!(snapshot(&state,"valid.snap").is_err());
    assert_eq!(std::fs::read("valid.snap").unwrap(),valid);}
    """,
    )
    _run(binary, tmp_path)
