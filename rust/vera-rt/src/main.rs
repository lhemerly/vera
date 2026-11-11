//! VERA furnace / runtime demo using the io_uring intent WAL.
//!
//! The generated specification (state, transitions, batch dispatcher) is
//! included from `spec.rs`, emitted by native `vera codegen rust --runtime-crate`.

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

    let key0 = 1u64 << KEY_BITS_ACCOUNT;
    let key1 = (1u64 << KEY_BITS_ACCOUNT) | 1u64;

    let intents = vec![
        Params::Transfer {
            from: key0,
            to: key1,
            amount: 100_000000,
        },
        Params::Transfer {
            from: key1,
            to: key0,
            amount: 50_000000,
        },
        Params::Freeze { acct: key0 },
        Params::PlaceOrder {
            buyer: key1,
            amount: 10_000000,
        },
    ];

    // Intent WAL via io_uring (falls back to pwrite if unavailable).
    let mut wal = ::vera_rt::IoUringWAL::open("vera.wal").unwrap();
    // Reconstruct the committed state before accepting another batch. Typed
    // reverts are deterministic log outcomes and intentionally leave no writes.
    for record in wal.replay().expect("valid existing WAL") {
        let intent = Params::decode(record.transition, &record.args)
            .expect("existing WAL belongs to this program");
        let _ = intent.execute(&mut state);
    }
    println!(
        "wal backend: {}",
        if wal.uses_io_uring() {
            "io_uring"
        } else {
            "pwrite"
        }
    );
    for intent in &intents {
        let (tid, args) = intent.encode();
        wal.append(tid, args).unwrap();
    }
    wal.sync().unwrap();
    println!("wal tx_seq = {}", wal.tx_seq);

    // Ordered speculative dispatcher with validation and serial re-execution.
    let results = run_batch(&mut state, &intents);
    let committed = results.iter().filter(|r| r.is_ok()).count();
    println!("batch committed {}/{}", committed, intents.len());
    println!("balance[0] = {}", state.account.col_balance[0]);
    println!("balance[1] = {}", state.account.col_balance[1]);
    println!("agg_sum_balance = {}", state.account.agg_sum_balance);

    snapshot(&state, "vera.snap").unwrap();
    println!(
        "snapshot bytes = {}",
        std::fs::metadata("vera.snap").unwrap().len()
    );

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
        let intent = Params::decode(rec.transition, &rec.args).expect("valid persisted intent");
        let _ = intent.execute(&mut replay);
    }
    println!("replay records = {}", records.len());
    println!("replay balance[0] = {}", replay.account.col_balance[0]);
    println!("replay balance[1] = {}", replay.account.col_balance[1]);
    assert_eq!(
        replay.encode(),
        state.encode(),
        "all tables, generations, indexes, aggregates and effects must replay identically"
    );
    let restored =
        State::decode(std::fs::read("vera.snap").unwrap()).expect("valid portable snapshot");
    assert_eq!(restored.encode(), state.encode());
    println!("replay matches live state: deterministic");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_transfers_preserve_the_aggregate() {
        let k = |s: u64| (1u64 << KEY_BITS_ACCOUNT) | s;
        let mut bad = 0u32;
        for _ in 0..50000 {
            let mut state = State::new();
            state.account.occupied_bitmap[0] |= 0xFFFFu64;
            for s in 0..16 {
                state.account.generation[s] = 1;
                state.account.col_balance[s] = 1_000_000;
            }
            state.account.agg_sum_balance = 16_000_000;
            let mut intents = Vec::new();
            for s in 0..8u64 {
                intents.push(Params::Transfer {
                    from: k(s * 2),
                    to: k(s * 2 + 1),
                    amount: 100_000 + s as i64 * 10_000,
                });
            }
            let _ = run_batch(&mut state, &intents);
            if state.account.agg_sum_balance != 16_000_000 {
                bad += 1;
            }
        }
        assert_eq!(
            bad, 0,
            "lost update on the tracked_sum accumulator in {bad}/50000 runs"
        );
    }
}
