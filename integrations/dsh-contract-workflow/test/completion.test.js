import test from 'node:test';import assert from 'node:assert/strict';
import {completionReview,completionProposal,governCompletion} from '../completion.js';
const binding={run_id:'r',task_hash:'t',deadline_ms:100};
const response=(decision='continue')=>({...binding,decision,summary:'reviewed',completion_scope:'accepted_stop_not_task_correctness',artifact_digest:'abc',missing:decision==='continue'?[{id:'adapter',requirement:'Actual adapter use',reason:'Missing',next_action:'Integrate adapter and test boundary'}]:[],evidence:[{id:'archive',provenance:'trusted frozen source'}]});
test('bind review, reject false completion/malformed review, require actionable continuation',()=>{
 assert.equal(completionReview(response(),binding).decision,'continue');
 for(const patch of [{deadline_ms:200},{task_hash:'other'},{decision:'complete'},{decision:'ready_for_audit'},{missing:[]},{completion_scope:'task_proved'},{artifact_digest:''}])assert.throws(()=>completionReview({...response(),...patch},binding));
 assert.equal(completionReview(response('ready_for_audit'),binding).decision,'ready_for_audit');
 assert.throws(()=>completionReview(response('blocked'),binding));
 assert.equal(completionReview({...response('blocked'),blocker:{kind:'inaccessible',detail:'Provider unavailable'}},binding).decision,'blocked');
});
test('same session continuation sees gap; no new clock; later readiness and proposal reset',async()=>{
 const inputs=[],claims=[];let pending={status:'partial'},reset=0;
 const agent={session:{id:'one'},whenIdle:async()=>{}};
 const result=await governCompletion({agent,task:'original',deadlineMs:100,now:()=>1,followup:x=>inputs.push(x),summary:()=>({text:'done',reason:{kind:'completed'}}),proposal:()=>pending,resetProposal:()=>{pending=null;reset++;},review:async c=>{claims.push(c);return response(claims.length===1?'continue':'ready_for_audit');}});
 assert.equal(result.decision,'ready_for_audit');assert.equal(inputs.length,2);assert.match(inputs[1],/Integrate adapter/);assert.equal(reset,1);assert.equal(claims[1].proposal,null);assert.equal(new Set(claims.map(x=>x.session_id)).size,1);
});
test('expired clock and cancellation do not start work; blocked stops without restart',async()=>{
 for(const opts of [{now:()=>100},{now:()=>1,signal:AbortSignal.abort()}]){
  let calls=0;const r=await governCompletion({agent:{},task:'x',deadlineMs:100,followup:()=>calls++,...opts});assert.equal(calls,0);assert.ok(['time_expired','cancelled'].includes(r.decision));
 }
 let calls=0;const r=await governCompletion({agent:{session:{id:'x'},whenIdle:async()=>{}},task:'x',deadlineMs:100,now:()=>1,followup:()=>calls++,summary:()=>({text:'blocked'}),review:async()=>({...response('blocked'),blocker:{kind:'unsafe',detail:'Requires unapproved access'}})});assert.equal(r.decision,'blocked');assert.equal(calls,1);
});
test('review outages never become completion and invalid blocker proposals rejected',async()=>{
 await assert.rejects(governCompletion({agent:{session:{id:'x'},whenIdle:async()=>{}},task:'x',deadlineMs:100,now:()=>1,followup:()=>{},summary:()=>({text:'done'}),review:async()=>{throw Error('offline');}}),/offline/);
 assert.throws(()=>completionProposal({status:'blocked',remaining_obligation_ids:[],core_artifact_paths:[],adapter_artifact_paths:[],test_evidence:[],blocker:{kind:'none',detail:''}}));
});
