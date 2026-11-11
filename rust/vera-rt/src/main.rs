//! VERA furnace / runtime demo using the io_uring intent WAL.
//!
//! The generated specification (state, transitions, batch dispatcher) is
//! included from `spec.rs`, which `tools/build_cargo.py` writes.

mod spec;
use spec::*;

fn main() {
    let mut state = State::new();
    // Seed two accounts (generation 1).
    state.account.occupied_bitmap[0] |= 0b11u64;
    state.account.generation[0] = 1;
    state.account.generation[1] = 1;
    state.account.col_balance[0] = 500_000000;
    state.account.col_balance[1] = 300_000000;
    state.account.agg_sum_balance = 800_000000;

    let key0 = (1u64 << KEY_BITS_ACCOUNT) | 0u64;
    let key1 = (1u64 << KEY_BITS_ACCOUNT) | 1u64;

    let intents = vec![
        Params::Transfer { from: key0, to: key1, amount: 100_000000 },
        Params::Transfer { from: key1, to: key0, amount: 50_000000 },
        Params::Freeze { acct: key0 },
        Params::PlaceOrder { buyer: key1, amount: 10_000000 },
    ];

    // Intent WAL via io_uring (falls back to pwrite if unavailable).
    let mut wal = ::vera_rt::IoUringWAL::open("vera.wal").unwrap();
    println!("wal backend: {}", if wal.uses_io_uring() { "io_uring" } else { "pwrite" });
    for intent in &intents {
        let (tid, args) = intent.encode();
        wal.append(tid, args).unwrap();
    }
    wal.sync().unwrap();
    println!("wal tx_seq = {}", wal.tx_seq);

    // Tier-1 dispatcher: conflict analysis + lock-free waves.
    let results = run_batch(&mut state, &intents);
    let committed = results.iter().filter(|r| r.is_ok()).count();
    println!("batch committed {}/{}", committed, intents.len());
    println!("balance[0] = {}", state.account.col_balance[0]);
    println!("balance[1] = {}", state.account.col_balance[1]);
    println!("agg_sum_balance = {}", state.account.agg_sum_balance);

    snapshot(&state, "vera.snap").unwrap();
    println!("snapshot bytes = {}", std::fs::metadata("vera.snap").unwrap().len());

    // Replay the WAL deterministically and compare against the live state.
    let records = ::vera_rt::replay_file("vera.wal").unwrap();
    let mut replay = State::new();
    replay.account.occupied_bitmap[0] |= 0b11u64;
    replay.account.generation[0] = 1;
    replay.account.generation[1] = 1;
    replay.account.col_balance[0] = 500_000000;
    replay.account.col_balance[1] = 300_000000;
    replay.account.agg_sum_balance = 800_000000;
    for rec in &records {
        let intent = Params::decode(rec.transition, rec.args);
        let _ = intent.execute(&mut replay);
    }
    println!("replay records = {}", records.len());
    println!("replay balance[0] = {}", replay.account.col_balance[0]);
    println!("replay balance[1] = {}", replay.account.col_balance[1]);
    assert_eq!(replay.account.col_balance[0], state.account.col_balance[0]);
    assert_eq!(replay.account.col_balance[1], state.account.col_balance[1]);
    println!("replay matches live state: deterministic");
}
