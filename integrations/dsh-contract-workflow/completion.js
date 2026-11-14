import http from 'node:http';
const text=x=>typeof x==='string'&&x.trim();
export function completionReview(value,binding){
 if(!value||typeof value!=='object'||Array.isArray(value))throw Error('Invalid completion review');
 for(const k of ['run_id','task_hash','deadline_ms'])if(value[k]!==binding[k])throw Error('Completion review binding mismatch: '+k);
 if(!['continue','ready_for_audit','blocked','time_expired','cancelled'].includes(value.decision)||!text(value.summary)||!Array.isArray(value.missing)||!Array.isArray(value.evidence))throw Error('Invalid completion review decision');
 if(value.completion_scope!=='accepted_stop_not_task_correctness')throw Error('Completion review must distinguish stop from correctness');
 if(!text(value.artifact_digest))throw Error('Completion review needs frozen artifact identity');
 if(value.missing.some(x=>!x||!['id','requirement','reason','next_action'].every(k=>text(x[k]))))throw Error('Invalid actionable missing obligation');
 if(value.evidence.some(x=>!x||!text(x.id)||!text(x.provenance)))throw Error('Invalid review evidence');
 if(value.decision==='continue'&&!value.missing.length)throw Error('Continuation needs actionable gaps');
 if(value.decision==='ready_for_audit'&&(value.missing.length||!value.evidence.length))throw Error('Accepted completion needs evidence and no unresolved required gaps');
 if(value.decision==='blocked'&&(!value.blocker||!['inaccessible','unsafe','stalled'].includes(value.blocker.kind)||!text(value.blocker.detail)))throw Error('Partial stop needs concrete blocker');
 return structuredClone(value);
}
export function completionClient(base,binding){
 const url=new URL(base);
 if(url.protocol!=='http:'||!['127.0.0.1','[::1]'].includes(url.hostname)||url.username||url.password||url.search||url.hash||!['','/'].includes(url.pathname))throw Error('Completion reviewer must be a fixed loopback HTTP origin');
 url.pathname='/completion/review';
 return async(claim,signal)=>{
  const value=await new Promise((resolve,reject)=>{
   const req=http.request(url,{method:'POST',signal,headers:{'content-type':'application/json'}},res=>{
    let raw='';res.setEncoding('utf8');res.on('data',x=>raw+=x);res.on('error',reject);res.on('end',()=>{try{if(res.statusCode!==200)throw Error('Completion review HTTP '+res.statusCode);resolve(JSON.parse(raw));}catch(e){reject(e);}});
   });req.on('error',reject);req.end(JSON.stringify({...claim,run_id:binding.run_id,task_hash:binding.task_hash}));
  });return completionReview(value,binding);
 };
}
export function continuationText(review){
 return 'Trusted completion review: this is the SAME authorized task and session, not a new run. A verified local slice is not completion of the original obligations. Continue addressing the concrete gaps below within the existing time and permissions. Do not invent evidence, fake adapter use, weaken requirements, or claim blanket correctness. If a step is genuinely inaccessible/unsafe, document its concrete blocker and preserved unknowns.\n'+JSON.stringify({summary:review.summary,missing:review.missing,evidence:review.evidence});
}
/** One existing agent, no fresh sessions, no new clock or budgets. Reviewer owns evidence checks. */
export async function governCompletion({agent,followup,task,review,summary,proposal=()=>null,resetProposal=()=>{},deadlineMs,signal,onReview=()=>{},now=Date.now}){
 if(!Number.isSafeInteger(deadlineMs))throw Error('Original absolute deadline is required');
 let turn=0;
 const stopped=()=>signal?.aborted?'cancelled':now()>=deadlineMs?'time_expired':null;
 if(stopped())return {decision:stopped(),summary:'Stopped before actor work; original clock/cancellation applies.'};
 followup(task);
 while(true){
  await agent.whenIdle();
  const reason=stopped();if(reason)return {decision:reason,summary:'Original clock/cancellation stopped this partial attempt.'};
  const outcome=summary();
  if(outcome.reason?.kind==='error')throw Error('Actor infrastructure failure: '+JSON.stringify(outcome.reason));
  const decision=await review({session_id:agent.session.id,turn_index:++turn,final_text:outcome.text,proposal:proposal()},signal);
  await onReview(decision);
  if(stopped())return {decision:stopped(),summary:'Original clock/cancellation stopped this partial attempt.'};
  if(decision.decision!=='continue')return decision;
  resetProposal();
  followup(continuationText(decision));
 }
}

export function completionProposal(value){
 if(!value||typeof value!=='object'||!['ready','partial','blocked'].includes(value.status))throw Error('Invalid completion proposal status');
 for(const key of ['remaining_obligation_ids','core_artifact_paths','adapter_artifact_paths'])if(!Array.isArray(value[key])||value[key].some(x=>!text(x)))throw Error('Invalid completion proposal '+key);
 if(!Array.isArray(value.test_evidence)||value.test_evidence.some(x=>!text(x?.command)||!text(x?.result)))throw Error('Invalid claimed test evidence');
 if(!value.blocker||!['none','inaccessible','unsafe','stalled'].includes(value.blocker.kind)||typeof value.blocker.detail!=='string')throw Error('Invalid completion blocker');
 if(value.status==='blocked'&&(value.blocker.kind==='none'||!text(value.blocker.detail)))throw Error('Blocked proposal needs concrete reason');
 return structuredClone(value);
}
