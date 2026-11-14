import test from 'node:test';
import assert from 'node:assert/strict';
import {ContractWorkflow,digest} from '../workflow.js';
const grounding=()=>({obligation_id:'R1',target_scope:'this_executable',input_origin:'CLI value argument parsed to u64',callsite:'emit command after parsing',observable_consumer:'emitted stdout payload',contrast:[]});
const spec=()=>({outcome:'ready',rationale:'Original task explicitly defines value identity',source:'fixture',requirements_map:'Identity core',coverage:[{id:'R1',requirement:'emit input value',disposition:'core_contract',justification:'Only value identity; host formatting remains external',evidence_ids:[]}],grounding:grounding(),assumptions:'u64 input',acceptance_tests:'Compare stdout for inputs 0 and 1'});
function fixture({runtime=true,answer=()=>spec(),observe}={}){
 let f,n=0;const events=[],freezes=[];
 const machine=new ContractWorkflow({task:{id:'task',original_requirements:'emit input value',provenance:'fixture',policy:{requireRuntimeGrounding:runtime},obligations:[{id:'R1',requirement:'emit input value'}]},
 backend:{async freeze(source){freezes.push(source);return {source};}},
 journal:async e=>events.push(e),
 observe:observe??(async req=>{const content=JSON.stringify({result:{status:'completed',stdout:req.argv[0],stderr:'',exit_code:0}});return {id:'obs-'+(++n),content,content_hash:digest(content),provenance:'isolated test reference',complete:true};}),
 runRole:async p=>answer(f,p),
 });f={machine,events,freezes};return f;
}
const signal=()=>new AbortController().signal;
const start=f=>f.machine.start({owner:'owner',requirements:'suggestion',parent:{id:'owner'},signal:signal()});
async function observedSpec(f,p){const s=spec();for(const v of ['0','1']){const e=await f.machine.observe({id:p.workflow,owner:'owner',request:{argv:[v]},signal:signal()});s.grounding.contrast.push({evidence_id:e.id,observed_output:v});s.coverage[0].evidence_ids.push(e.id);}return s;}
test('reference-backed ready contract binds original obligation and protected runtime snapshots',async()=>{const f=fixture({answer:observedSpec});const j=await start(f);assert.equal(j.status,'frozen');assert.equal(j.current.grounding.assessment,'declared_suitability_not_semantic_proof');assert.equal(f.events.filter(e=>e.type==='runtime_observation').length,2);assert.equal(f.freezes.length,1);});
test('fully specified pure task permits no-reference grounding, but still requires actual host mapping',async()=>{const f=fixture({runtime:false});assert.equal((await start(f)).status,'frozen');const bad=fixture({runtime:false,answer:()=>({...spec(),grounding:{...grounding(),callsite:''}})});await assert.rejects(start(bad),/callsite/);});
test('unknown suitability defers without freezing and retries same reserved lineage',async()=>{let first=true;const f=fixture({answer:(f,p)=>{if(first){first=false;return {outcome:'needs_evidence',rationale:'Need input/output observations'};}return observedSpec(f,p);}});const j=await start(f);assert.equal(j.status,'needs_evidence');assert.equal(f.freezes.length,0);await assert.rejects(start(f),/reserved/);const next=await f.machine.retry({id:j.id,owner:'owner',parent:{id:'owner'},signal:signal()});assert.equal(next.id,j.id);assert.equal(next.status,'frozen');});
test('not_suitable is distinct from missing evidence and emits no contract',async()=>{const f=fixture({answer:()=>({outcome:'not_suitable',rationale:'Proposed scheduler exists only in remote server'})});assert.equal((await start(f)).status,'not_suitable');assert.equal(f.freezes.length,0);});
test('reject remote-only and invented-scope core proposals before compiler',async()=>{for(const change of [{target_scope:'remote_daemon'},{obligation_id:'invented-scheduling'},{observable_consumer:''}]){const f=fixture({answer:async(f,p)=>{const s=await observedSpec(f,p);Object.assign(s.grounding,change);return s;}});await assert.rejects(start(f));assert.equal(f.freezes.length,0);}});
test('reject incomplete, repeated or noncontrasting observation claims',async()=>{for(const mode of ['incomplete','repeated','same-output']){const f=fixture({answer:async(f,p)=>{const s=await observedSpec(f,p);if(mode==='repeated')s.grounding.contrast[1]=s.grounding.contrast[0];if(mode==='same-output')s.grounding.contrast[1].evidence_id=s.grounding.contrast[0].evidence_id;return s;},...(mode==='incomplete'?{observe:async req=>{const content=JSON.stringify(req);return {id:'partial-'+req.argv[0],content,content_hash:digest(content),provenance:'timeout fixture',complete:false};}}:{})});await assert.rejects(start(f),/unobserved|contrasting|runtime evidence/);assert.equal(f.freezes.length,0);}});
test('observation integrity and IDs fail closed',async()=>{const f=fixture({answer:async(f,p)=>{await assert.rejects(f.machine.observe({id:p.workflow,owner:'owner',request:{argv:[]},signal:signal()}),/Invalid trusted/);return {outcome:'needs_evidence',rationale:'observer invalid'};},observe:async()=>({id:'bad',content:'text',content_hash:'forged',provenance:'fixture',complete:true})});await start(f);assert.equal(f.events.filter(e=>e.type==='runtime_observation').length,0);});

test('different model labels cannot fake contrast when actual outcomes are identical',async()=>{let n=0;const f=fixture({answer:observedSpec,observe:async request=>{const content=JSON.stringify({request,timestamp:++n,result:{status:'completed',stdout:'same',stderr:'',exit_code:0}});return {id:'obs-'+n,content,content_hash:digest(content),provenance:'fixture',complete:true};}});await assert.rejects(start(f),/contrasting/);assert.equal(f.freezes.length,0);});
test('same request cannot fake contrast through nondeterministic outcomes',async()=>{const f=fixture({answer:async(f,p)=>{const s=spec();for(const v of ['a','b']){const e=await f.machine.observe({id:p.workflow,owner:'owner',request:{argv:['same']},signal:signal()});s.grounding.contrast.push({evidence_id:e.id,observed_output:v});}return s;},observe:(()=>{let n=0;return async()=>{const content=JSON.stringify({result:{status:'completed',stdout:String(++n),stderr:'',exit_code:0}});return {id:'obs-'+n,content,content_hash:digest(content),provenance:'fixture',complete:true};};})()});await assert.rejects(start(f),/contrasting/);});

test('signal exit cannot be promoted to complete observation by an erroneous host flag',async()=>{const f=fixture({answer:observedSpec,observe:async()=>{const content=JSON.stringify({result:{status:'completed',stdout:'',stderr:'',exit_code:-9}});return {id:'signal',content,content_hash:digest(content),provenance:'fixture',complete:true};}});await assert.rejects(start(f),/completed runtime outcome/);assert.equal(f.events.filter(e=>e.type==='runtime_observation').length,0);});

test('same-lineage retry carries prior deferral rationale without replacing requirements',async()=>{
 for(const outcome of ['needs_evidence','not_suitable']){
  let count=0;const reason='Candidate needs actual input origin and success/rejection observations';
  const f=fixture({answer:async(f,p)=>{if(count++===0)return {outcome,rationale:reason};assert.match(p.prompt,/PRIOR INITIAL ASSESSMENTS/);assert.ok(p.prompt.includes(reason));assert.ok(p.prompt.includes('emit input value'));return observedSpec(f,p);}});
  const j=await start(f);const retried=await f.machine.retry({id:j.id,owner:'owner',parent:{id:'owner'},signal:signal()});
  assert.equal(retried.id,j.id);assert.equal(retried.status,'frozen');assert.equal(retried.requirements,'emit input value');
 }
});
