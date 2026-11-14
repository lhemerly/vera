import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp,chmod,stat,rm} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {serveBroker,callBroker,validateRequest} from '../broker.js';
test('lifecycle schema rejects role calls, owner injection and extra paths',()=>{
 for(const v of [{method:'vera_authority_evidence',args:{}},{method:'vera_workflow_start',args:{requirements:'x',owner:'fake'}},{method:'vera_task_get',args:{path:'/secret'}},{method:'__proto__',args:{}},{method:'vera_workflow_get',args:{id:1}}])assert.throws(()=>validateRequest(v));
 assert.deepEqual(validateRequest({method:'vera_task_get',args:{}}),{method:'vera_task_get',args:{}});
});
test('group-only broker roundtrip, same UID refusal, no existing socket replacement',async()=>{
 const dir=await mkdtemp(join(tmpdir(),'vera-broker-'));const path=join(dir,'s.sock');let close;
 try{
 await chmod(dir,0o750);
 await assert.rejects(serveBroker({socketPath:path,actorUid:process.getuid(),dispatch(){}}),/different actor/);
 const seen=[];close=await serveBroker({socketPath:path,actorUid:process.getuid()+1,dispatch:async(method,args)=>{seen.push([method,args]);return {id:'fixed'};}});
 assert.equal((await stat(path)).mode&0o777,0o660);
 assert.deepEqual(await callBroker(path,'vera_workflow_start',{requirements:'proposal'}),{id:'fixed'});
 assert.deepEqual(seen,[['vera_workflow_start',{requirements:'proposal'}]]);
 await assert.rejects(serveBroker({socketPath:path,actorUid:process.getuid()+1,dispatch(){}}),/Existing socket/);
 }finally{await close?.();await rm(dir,{recursive:true,force:true});}
});
