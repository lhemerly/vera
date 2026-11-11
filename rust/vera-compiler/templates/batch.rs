/// Ordered serial execution, also the fallback for speculative conflicts.
pub fn run_batch_serial(state:&mut State,intents:&[Params])->Vec<Result<(),ErrorCode>>{intents.iter().map(|p|p.execute(state)).collect()}
/// Speculate on immutable snapshots; validate the full pre-state at ordered commit.
/// Conservative validation covers data-dependent reads, indexes, occupancy,
/// aggregates and effects. A stale speculation is re-executed against the
/// committed state, exactly preserving sequential intent-log semantics.
pub fn run_batch(state:&mut State,intents:&[Params])->Vec<Result<(),ErrorCode>>{
    let mut results=Vec::with_capacity(intents.len());
    let workers=std::thread::available_parallelism().map(|n|n.get()).unwrap_or(1).min(32);
    for chunk in intents.chunks(workers.max(1)){
        let base=state.clone();
        let candidates=std::thread::scope(|scope|{
            let handles:Vec<_>=chunk.iter().map(|intent|{let pre=&base;scope.spawn(move||{let mut staged=pre.clone();let result=intent.execute(&mut staged);(staged,result)})}).collect();
            handles.into_iter().map(|h|h.join().expect("VERA speculative worker failed")).collect::<Vec<_>>()
        });
        for (intent,(staged,result)) in chunk.iter().zip(candidates){
            if *state==base {if result.is_ok(){*state=staged;}results.push(result);}else{results.push(intent.execute(state));}
        }
    }results
}
