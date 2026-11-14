import test from 'node:test';import assert from 'node:assert/strict';import {projectActorTrace} from '../actor-trace.js';
function fixture(seed=[]){let listener;const rows=[];const session={id:'actor',header:{id:'actor'},seq:seed.length,eventAt:i=>seed[i]};const agent={session};const ctx={on:(name,fn)=>{assert.equal(name,'session/event');listener=fn;return()=>{listener=null;};}};const trace=projectActorTrace(ctx,agent,{write:x=>rows.push(JSON.parse(x))});return {rows,trace,emit:(type,data={},surfaceOp='append')=>listener?.(session,{type,data,surfaceOp}),foreign:()=>listener?.({id:'authority'},{type:'tool/call',data:{name:'private'}})};}
const stream=usage=>[{type:'chunk',chunk:{type:'usage',usage}}];
test('raw actor events preserve full payload and initial events; foreign sessions excluded',()=>{
 const seed={type:'setup',data:{original:'retained'}};const f=fixture([seed]);assert.deepEqual(f.rows[1].event,seed);f.foreign();assert.equal(f.rows.length,2);
 const large='x'.repeat(40000);f.emit('tool/call',{callId:'c',name:'vera_verify',arguments:JSON.stringify({source:large})});f.emit('tool/result',{message:{toolCallId:'c',content:[{type:'text',text:large}]}});
 assert.equal(f.rows.find(x=>x.type==='tool_call').input.source.length,40000);assert.equal(f.rows.find(x=>x.type==='tool_result').result[0].text.length,40000);assert.equal(f.rows.filter(x=>x.type==='session/event').length,3);
 f.trace.dispose();const count=f.rows.length;f.emit('turn/start');assert.equal(f.rows.length,count);
});
test('known usage across attempts aggregates without inventing absent optional counters',()=>{
 const f=fixture();f.emit('assistant/attempt',{stream:stream({inputTokens:2,outputTokens:3,totalTokens:5,cacheReadTokens:1})});f.emit('assistant/message',{usage:{inputTokens:5,outputTokens:7,totalTokens:12},message:{content:[]}});f.emit('step/end');
 assert.deepEqual(f.rows.find(x=>x.phase==='step_end').usage,{inputTokens:7,outputTokens:10,totalTokens:17});
});
test('unknown or overflowing usage never becomes a plausible complete sum',()=>{
 for(const first of [undefined,{inputTokens:-1,outputTokens:3},{inputTokens:Number.MAX_SAFE_INTEGER,outputTokens:1}]){
  const f=fixture();f.emit('assistant/attempt',{stream:first?stream(first):[]});f.emit('assistant/message',{usage:{inputTokens:2,outputTokens:3},message:{content:[]}});f.emit('step/end');const row=f.rows.find(x=>x.phase==='step_end');assert.equal(row.usage,undefined);assert.equal(row.usageUnknown,true);
 }
 const f=fixture();f.emit('step/end');assert.equal(f.rows.at(-1).usageUnknown,true);
});
test('continuation statuses and final stop scope preserve no whole-task-success claim',()=>{
 const f=fixture();for(let turn=0;turn<2;turn++){f.emit('turn/start',{turn});f.emit('turn/end',{turn,reason:{kind:'completed'}});}f.trace.finish('candidate',{decision:'ready_for_audit'});assert.equal(f.rows.filter(x=>x.phase==='turn_start').length,2);assert.deepEqual(f.rows.at(-1),{type:'final',text:'candidate',decision:'ready_for_audit',completion_scope:'accepted_stop_not_task_correctness'});
});
