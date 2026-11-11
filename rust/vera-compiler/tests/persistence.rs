//! Durable-format compatibility against independently frozen wire vectors.
//! Tests consume only native Rust and checked-in bytes.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use num_bigint::{BigInt, Sign};
use vera_compiler::{
    ir::{CheckedProgram, TransitionId},
    parse_source,
    persistence::{
        append_intent, checksum, decode_state, decode_wal, encode_state, execute_logged,
        program_fingerprint, replay, restore, schema_fingerprint, snapshot, Intent, IntentRecord,
        PersistenceError, Wal,
    },
    runtime::{Machine, State},
    semantic::analyze,
    value::Value,
};

const WIRE: &str = include_str!("fixtures/codec.vera");

fn checked(source: &str) -> CheckedProgram {
    analyze(parse_source(source).unwrap()).unwrap()
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unhex(text: &str) -> Vec<u8> {
    let text = text.trim();
    assert_eq!(text.len() % 2, 0);
    (0..text.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&text[offset..offset + 2], 16).unwrap())
        .collect()
}

fn int(value: impl Into<BigInt>) -> Value {
    Value::Int(value.into())
}

fn wire_intent() -> Intent {
    Intent {
        transition: TransitionId(0),
        args: vec![
            Value::Bool(true),
            int(255u8),
            int(0x1234u16),
            int(0x89abcdefu32),
            int(0xfedcba9876543210u64),
            int(i64::MIN),
            int(-123456789i64),
            int(-((1i128 << 120) + 123)),
            int(0xaabbccddu32),
            int(BigInt::from_bytes_le(
                Sign::Plus,
                &(0u8..32).collect::<Vec<_>>(),
            )),
            int(1u8),
            int(u64::MAX),
        ],
    }
}

fn wire_state(program: &CheckedProgram) -> State {
    let mut machine = Machine::new(program).unwrap();
    let intent = wire_intent();
    machine
        .call("create", &intent.parameters(program).unwrap())
        .unwrap();
    machine.state
}

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vera-native-persistence-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn shipped_source_identities_match_canonical_wire_vectors() {
    for (source, program_hash, schema_hash) in [
        (
            include_str!("../../../examples/counter.vera"),
            "08f760a9efb7b16f0d837ba7ce01540436bfaeab5c48cec34676b7763ef92708",
            "958e021db8fea339bf95b141e72b5bb387d43de614ba8e027e669e69cb578e8c",
        ),
        (
            include_str!("../../../examples/bank.vera"),
            "ae6bddb468a80f5aefad59ba424420ef43f637d4c987b2758a0cc8de5afaea13",
            "cc4d7943dd8b0d27e226e7b608ac95270d0151d63475360079576ecbbb0254d2",
        ),
        (
            "module Types;table T capacity 1 {key id:id64<T>;col status:enum{A,B};col peer:id64<T>;}effect Event(tag:enum{A,B},peer:id64<T>);transition noop() reads [] writes [] body {commit;}",
            "95ee5a38002038a82a8e64f3436e6d04914f4c74670a3930db3116227fe1086b",
            "46a7563289c9459816c72eb8aaad6eafb2e6175668f6c889a9ae89f9aedad9b9",
        ),
        (
            "",
            "290c2a375a345a06e8d55352127af8089d2e9ea778b36d156403465c625d9736",
            "7896ac2ecd7014c6934e3152ab915e7aa64e58bd0b4b0e31e3d803c5b90a6ce1",
        ),
    ] {
        let program = checked(source);
        assert_eq!(hex(program_fingerprint(&program)), program_hash);
        assert_eq!(hex(schema_fingerprint(&program)), schema_hash);
    }
}

#[test]
fn every_scalar_and_twelve_arguments_match_historical_typed_intent() {
    let program = checked(WIRE);
    let intent = wire_intent();
    let (tag, payload) = intent.encode(&program).unwrap();
    let historical = unhex(include_str!("fixtures/codec-intent.hex"));
    assert_eq!(tag, 0);
    assert_eq!(payload, historical);
    assert_eq!(Intent::decode(&program, tag, &historical).unwrap(), intent);
    assert_eq!(
        Intent::from_parameters(&program, "create", &intent.parameters(&program).unwrap()).unwrap(),
        intent
    );
}

#[test]
fn native_snapshot_bytes_match_historical_engine_including_effects_and_indexes() {
    let program = checked(WIRE);
    let state = wire_state(&program);
    let historical = unhex(include_str!("fixtures/codec-snapshot.hex"));
    assert_eq!(encode_state(&program, &state).unwrap(), historical);
    let restored = decode_state(&program, &historical).unwrap();
    assert_eq!(encode_state(&program, &restored).unwrap(), historical);
    assert_eq!(restored.outbox.len(), 1);
    assert_eq!(restored.outbox[0].args[2], wire_intent().args[9]);
}

#[test]
fn typed_intent_rejects_bad_domains_identity_lengths_and_parameters() {
    let program = checked(WIRE);
    let (tag, bytes) = wire_intent().encode(&program).unwrap();
    for prefix in 0..bytes.len() {
        assert!(Intent::decode(&program, tag, &bytes[..prefix]).is_err());
    }
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(Intent::decode(&program, tag, &bad).is_err());
    bad = bytes.clone();
    bad[0] ^= 1;
    assert!(Intent::decode(&program, tag, &bad).is_err());
    bad = bytes.clone();
    bad[32] = 2; // bool
    assert!(Intent::decode(&program, tag, &bad).is_err());
    bad = bytes;
    bad[116] = 2; // enum has only A and B
    assert!(Intent::decode(&program, tag, &bad).is_err());
    assert!(Intent::decode(&program, u32::MAX, &bad).is_err());
    let mut intent = wire_intent();
    intent.args[1] = int(256u16);
    assert!(intent.encode(&program).is_err());
    intent.args.pop();
    assert!(intent.encode(&program).is_err());
    intent.transition = TransitionId(usize::MAX);
    assert!(intent.encode(&program).is_err());
    assert!(Intent::from_parameters(&program, "create", &BTreeMap::new()).is_err());
    let mut parameters = wire_intent().parameters(&program).unwrap();
    parameters.remove("tiny");
    parameters.insert("unrecognized".into(), int(1));
    assert!(Intent::from_parameters(&program, "create", &parameters).is_err());
    assert!(Intent::from_parameters(&program, "absent", &parameters).is_err());
}

fn repair_checksum(bytes: &mut [u8]) {
    let end = bytes.len() - 4;
    let crc = checksum(&bytes[..end]);
    bytes[end..].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn snapshots_reject_corrupt_and_noncanonical_state_with_valid_crcs() {
    let program = checked(WIRE);
    let historical = unhex(include_str!("fixtures/codec-snapshot.hex"));
    assert_eq!(historical.len(), 500);
    // Every missing byte and every single-bit corruption is rejected.
    for prefix in 0..historical.len() {
        assert!(decode_state(&program, &historical[..prefix]).is_err());
    }
    for index in 0..historical.len() {
        let mut bad = historical.clone();
        bad[index] ^= 1;
        assert!(decode_state(&program, &bad).is_err());
    }
    // Bypass the CRC to exercise admission rather than just the envelope check.
    for (offset, value) in [
        (8, 0),   // wrong schema
        (40, 5),  // nonzero unused occupied bit
        (57, 1),  // value in an unoccupied slot
        (216, 0), // lost unique index entry
        (240, 1), // wrong signed aggregate
        (304, 1), // wrong widened decimal aggregate
        (408, 2), // invalid enum column tag
        (434, 9), // unknown effect tag
        (438, 2), // invalid effect bool
        (487, 2), // invalid effect enum tag
    ] {
        let mut bad = historical.clone();
        bad[offset] = value;
        repair_checksum(&mut bad);
        assert!(
            decode_state(&program, &bad).is_err(),
            "accepted offset {offset}"
        );
    }
    let mut bad = historical[..historical.len() - 4].to_vec();
    bad.push(0);
    bad.extend_from_slice(&[0; 4]);
    repair_checksum(&mut bad);
    assert!(decode_state(&program, &bad).is_err());
}

#[test]
fn restoring_valid_state_does_not_require_empty_state_to_satisfy_invariants() {
    let program = checked("table T capacity 1 {key id:id64<T>;col v:u64 [tracked_sum];}invariant populated {eq(sum(T.v),1);}");
    assert!(Machine::new(&program).is_err());
    let mut state = State::empty(&program).unwrap();
    state.tables[0].occupied[0] = true;
    state.tables[0].generations[0] = 1;
    state.tables[0].columns[0][0] = int(1);
    state.tables[0].aggregates[0] = Some(BigInt::from(1));
    let bytes = encode_state(&program, &state).unwrap();
    let restored = decode_state(&program, &bytes).unwrap();
    assert!(Machine::with_state(&program, restored).is_ok());
}

#[test]
fn tiny_snapshot_for_huge_schema_is_rejected_before_table_allocation() {
    let program =
        checked("table Huge capacity 4294967296 {key id:id64<Huge>;col v:dec128 [tracked_sum];}");
    assert!(decode_state(&program, &[0; 52]).is_err());
}

#[test]
fn zero_argument_effects_roundtrip_and_forged_counts_are_bounded() {
    let program = checked(
        "effect Ping();transition send() reads [] writes [] body {emit Ping();emit Ping();commit;}",
    );
    let mut machine = Machine::new(&program).unwrap();
    machine.call("send", &BTreeMap::new()).unwrap();
    let mut bytes = encode_state(&program, &machine.state).unwrap();
    let restored = decode_state(&program, &bytes).unwrap();
    assert_eq!(restored.outbox.len(), 2);
    assert!(restored.outbox.iter().all(|effect| effect.args.is_empty()));
    for count in [3u64, u64::MAX] {
        bytes[40..48].copy_from_slice(&count.to_le_bytes());
        repair_checksum(&mut bytes);
        assert!(decode_state(&program, &bytes).is_err());
    }
}

#[test]
fn atomic_snapshot_roundtrip_and_invalid_save_preserve_previous_file() {
    let directory = TestDir::new();
    let path = directory.file("state.snapshot");
    let program = checked(WIRE);
    let mut state = wire_state(&program);
    snapshot(&program, &state, &path).unwrap();
    let previous = fs::read(&path).unwrap();
    let restored = restore(&program, &path).unwrap();
    assert_eq!(encode_state(&program, &restored).unwrap(), previous);
    state.tables[0].aggregates[5] = Some(BigInt::from(0));
    assert!(snapshot(&program, &state, &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), previous);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    assert!(restore(&program, &directory.file("missing")).is_err());
}

#[test]
fn wal_frame_matches_historical_engine_and_restart_replays_failed_intents() {
    let program = checked(WIRE);
    let intent = wire_intent();
    let (transition, args) = intent.encode(&program).unwrap();
    let record = IntentRecord {
        intent_id: 1,
        tx_seq: 1,
        transition,
        args,
    };
    assert_eq!(
        record.encode().unwrap(),
        unhex(include_str!("fixtures/codec-wal.hex"))
    );
    let directory = TestDir::new();
    let path = directory.file("intents.wal");
    let mut live = Machine::new(&program).unwrap();
    {
        let mut wal = Wal::open(&path).unwrap();
        execute_logged(&mut live, &mut wal, &intent, true)
            .unwrap()
            .unwrap();
        let before = encode_state(&program, &live.state).unwrap();
        assert!(execute_logged(
            &mut live,
            &mut wal,
            &Intent {
                transition: TransitionId(1),
                args: Vec::new(),
            },
            true,
        )
        .unwrap()
        .is_err());
        assert_eq!(encode_state(&program, &live.state).unwrap(), before);
    }
    let mut restarted = Machine::new(&program).unwrap();
    let outcomes = replay(&mut restarted, &path).unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes[0].is_ok());
    assert!(outcomes[1].is_err());
    assert_eq!(
        encode_state(&program, &live.state).unwrap(),
        encode_state(&program, &restarted.state).unwrap()
    );
    let mut reopened = Wal::open(&path).unwrap();
    append_intent(
        &mut reopened,
        &program,
        &Intent {
            transition: TransitionId(1),
            args: Vec::new(),
        },
    )
    .unwrap();
    reopened.sync().unwrap();
    assert_eq!(reopened.replay().unwrap()[2].tx_seq, 3);
}

#[test]
fn wal_recovers_torn_tail_but_rejects_complete_corruption() {
    let program = checked(WIRE);
    let directory = TestDir::new();
    let path = directory.file("intents.wal");
    let first = unhex(include_str!("fixtures/codec-wal.hex"));
    let second = IntentRecord {
        intent_id: 2,
        tx_seq: 2,
        transition: 1,
        args: program_fingerprint(&program).to_vec(),
    }
    .encode()
    .unwrap();
    for missing in [1, second.len() / 2, second.len() - 1] {
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second[..second.len() - missing]);
        fs::write(&path, bytes).unwrap();
        {
            let wal = Wal::open(&path).unwrap();
            assert_eq!(wal.replay().unwrap().len(), 1);
        }
        assert_eq!(fs::read(&path).unwrap(), first);
    }
    let mut corrupt = first.clone();
    corrupt[32] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert!(Wal::open(&path).is_err());
    assert!(decode_wal(&program, &path).is_err());
}

#[test]
fn replay_rejects_later_semantic_identity_before_publishing_any_prefix() {
    let program = checked(WIRE);
    let directory = TestDir::new();
    let path = directory.file("intents.wal");
    {
        let mut wal = Wal::open(&path).unwrap();
        append_intent(&mut wal, &program, &wire_intent()).unwrap();
        let mut bad = program_fingerprint(&program).to_vec();
        bad[0] ^= 1;
        wal.append(1, bad).unwrap();
        wal.sync().unwrap();
    }
    let mut machine = Machine::new(&program).unwrap();
    let before = encode_state(&program, &machine.state).unwrap();
    assert!(replay(&mut machine, &path).is_err());
    assert_eq!(encode_state(&program, &machine.state).unwrap(), before);
}

#[cfg(target_os = "linux")]
#[test]
fn logged_append_failure_does_not_publish_state_or_outbox() {
    const CHILD: &str = "VERA_TEST_PERSISTENCE_FILE_LIMIT";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new("bash")
            .args([
                "-c",
                "ulimit -f 0; trap '' XFSZ; exec \"$1\" --exact logged_append_failure_does_not_publish_state_or_outbox --nocapture",
                "vera-persistence-fault-test",
            ])
            .arg(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let program = checked(WIRE);
    let directory = TestDir::new();
    let path = directory.file("cannot-grow.wal");
    let mut machine = Machine::new(&program).unwrap();
    let before = encode_state(&program, &machine.state).unwrap();
    let mut wal = Wal::open(&path).unwrap();
    assert!(matches!(
        execute_logged(&mut machine, &mut wal, &wire_intent(), true),
        Err(PersistenceError::Io(_))
    ));
    assert_eq!(encode_state(&program, &machine.state).unwrap(), before);
    assert!(machine.state.outbox.is_empty());
    assert_eq!(fs::metadata(Path::new(&path)).unwrap().len(), 0);
}
