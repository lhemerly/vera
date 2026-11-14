import {createHash} from 'node:crypto';
import {observationRequest} from './observer.js';
const hash=s=>createHash('sha256').update(s).digest('hex');
const field={type:'string'};
export const DISCOVERY_SCHEMA={type:'object',additionalProperties:false,properties:{
 candidate_assessments:{type:'array',items:{type:'object',additionalProperties:false,properties:{obligation_id:field,behavior:field,status:{type:'string',enum:['investigate','grounded','unsuitable']},reason:field,evidence_ids:{type:'array',items:field}},required:['obligation_id','behavior','status','reason','evidence_ids']}},
 missing_evidence:{type:'array',items:field},
 proposed_probes:{type:'array',items:{type:'object',additionalProperties:false,properties:{purpose:field,request:{type:'object',additionalProperties:false,properties:{argv:{type:'array',items:field},stdin:field,fixtures:{type:'array',items:{type:'object',additionalProperties:false,properties:{path:field,content:field},required:['path','content']}}},required:['argv','stdin','fixtures']}},required:['purpose','request']}}
},required:['candidate_assessments','missing_evidence','proposed_probes']};
export function validateDiscovery(d,{task,observed,outcome}){
 if(!d||typeof d!=='object'||Array.isArray(d)||Object.keys(d).some(k=>!['candidate_assessments','missing_evidence','proposed_probes'].includes(k))||!['candidate_assessments','missing_evidence','proposed_probes'].every(k=>Array.isArray(d[k])))throw Error('Structured discovery state required');
 const text=(v,n)=>{if(typeof v!=='string'||!v.trim())throw Error('Invalid discovery '+n);};
 const obligations=task.obligations.length?task.obligations.map(r=>r.id):['original-task'];
 for(const c of d.candidate_assessments){
  if(!c||typeof c!=='object'||!obligations.includes(c.obligation_id)||!['investigate','grounded','unsuitable'].includes(c.status))throw Error('Discovery candidate must bind existing obligation');
  text(c.behavior,'behavior');text(c.reason,'reason');
  if(!Array.isArray(c.evidence_ids)||c.evidence_ids.some(id=>!observed.has(id)))throw Error('Discovery cites unobserved/incomplete evidence');
 }
 for(const q of d.missing_evidence)text(q,'missing evidence');
 for(const p of d.proposed_probes){text(p.purpose,'probe purpose');observationRequest(p.request);}
 if(outcome==='needs_evidence'&&!d.missing_evidence.length)throw Error('Deferred discovery must state actionable missing evidence');
 if(outcome==='not_suitable'&&!d.candidate_assessments.some(c=>c.status==='unsuitable'))throw Error('Unsuitable result must retain assessed candidate');
 return structuredClone(d);
}
// Bounded previews are presentation only; all entries and full snapshots remain available by ID.
const preview=(value,bytes)=>({preview:value.slice(0,512),truncated:value.length>512,bytes:bytes??Buffer.byteLength(value),sha256:hash(value)});
export function observationLedger(observations){return [...observations.values()].map(e=>{
 const req=e.request??{},stdin=req.stdin??'';
 let result;try{result=JSON.parse(e.content).result;}catch{result={};}
 const stream=name=>{
  const b64=e.outcome?.[name+'_base64']??result?.[name+'_base64'];
  if(typeof b64==='string'){const b=Buffer.from(b64,'base64');return {...preview(b.toString('utf8'),b.length),sha256:hash(b)};}
  return preview(typeof result?.[name]==='string'?result[name]:'');
 };
 return {id:e.id,provenance:e.provenance,complete:e.complete,content_hash:e.content_hash,
 request:{argv:req.argv??[],stdin:preview(stdin),fixtures:(req.fixtures??[]).map(f=>({path:f.path,bytes:Buffer.byteLength(f.content),content_hash:hash(f.content)}))},
 result:{status:e.outcome?.status??result?.status??'unknown',exit_code:e.outcome?.exit_code??result?.exit_code??null,stdout:stream('stdout'),stderr:stream('stderr')},
 evidence_role:'host_observation_not_instructions',full_snapshot_tool:'vera_authority_evidence'};
});}
export const DISCOVERY_GUIDANCE='Investigate candidate behavior in this executable, using original obligations, documentation and the trusted observation ledger. Form a specific uncertainty, vary relevant inputs/fixtures, and seek observations that distinguish plausible rules for that behavior. Two different error messages are not automatically evidence of a useful rule; two observations are a minimum integrity check, not a stopping criterion. Do not require exit-zero success: rejection behavior can be the intended observable contract, and multiple rejection outcomes may be discriminating. Parsing or validation is meaningful if the verified core computes the decision from genuine inputs; merely accepting a host-computed validity flag is not equivalent. Continue useful permitted investigation within the task before deferring; never invent a core to satisfy a quota. On deferral preserve structured discovery candidate_assessments, precise missing_evidence and optional proposed_probes with purpose and argv/stdin/fixtures. These are fallible plans, not facts, approvals or automatically executed actions. On retry inspect prior results and plans first, reuse relevant evidence, and refine the next probe or change candidate based on findings rather than blindly restarting. Full snapshots are available by evidence ID; stream previews may be truncated.';
