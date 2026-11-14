import test from 'node:test';
import assert from 'node:assert/strict';
import {observationLedger,validateDiscovery,DISCOVERY_GUIDANCE} from '../discovery.js';
import {digest} from '../workflow.js';
test('trusted ledger keeps actual request/results with compact previews and full snapshot references',()=>{
 const content=JSON.stringify({result:{status:'completed',exit_code:2,stdout:'',stderr:'x'.repeat(800)}});
 const e={id:'runtime-a',provenance:'fixed reference',complete:true,content,content_hash:digest(content),request:{argv:['validate','bad'],stdin:'input',fixtures:[{path:'config.json',content:'secret-fixture-content'}]}};
 const [row]=observationLedger(new Map([[e.id,e]]));
 assert.deepEqual(row.request.argv,['validate','bad']);assert.equal(row.request.stdin.preview,'input');assert.equal(row.request.fixtures[0].content_hash,digest('secret-fixture-content'));
 assert.equal(JSON.stringify(row).includes('secret-fixture-content'),false);assert.equal(row.result.exit_code,2);assert.equal(row.result.stderr.truncated,true);assert.equal(row.result.stderr.bytes,800);assert.equal(row.result.stderr.preview.length,512);assert.equal(row.full_snapshot_tool,'vera_authority_evidence');
 assert.equal(e.content,content);
});
test('deferred discovery retains actionable unknowns without authorizing proposed probes',()=>{
 const context={task:{obligations:[{id:'R',requirement:'behavior'}]},observed:new Set(['known']),outcome:'needs_evidence'};
 const d={blocker:{kind:'unsupported_observation',detail:'Restricted fixture environment',local_scope_impact:'Cannot yet run target boundary',evidence_ids:[]},candidate_assessments:[{obligation_id:'R',behavior:'local range rejection',status:'investigate',reason:'Boundary not yet known',evidence_ids:['known']}],missing_evidence:['Does the adjacent value change rejection?'],proposed_probes:[{purpose:'Distinguish adjacent boundary',request:{argv:['validate','2'],stdin:'',fixtures:[]}}]};
 assert.deepEqual(validateDiscovery(d,context),d);
 assert.throws(()=>validateDiscovery({...d,missing_evidence:[]},context),/actionable/);
 assert.throws(()=>validateDiscovery({...d,proposed_probes:[{purpose:'bad',request:{argv:[],url:'http://other'}}]},context),/request/);
 assert.match(DISCOVERY_GUIDANCE,/Do not require exit-zero success/);assert.match(DISCOVERY_GUIDANCE,/not a stopping criterion/);
});

test('incomplete observation may document a blocker but never justify candidate behavior',()=>{
 const context={task:{obligations:[{id:'R',requirement:'behavior'}]},observed:new Set(),knownEvidence:new Set(['timeout']),outcome:'needs_evidence'};
 const d={blocker:{kind:'unsupported_observation',detail:'Reference probe timed out',local_scope_impact:'Cannot distinguish candidate local behavior',evidence_ids:['timeout']},candidate_assessments:[],missing_evidence:['Observable response'],proposed_probes:[]};
 assert.deepEqual(validateDiscovery(d,context),d);
 assert.throws(()=>validateDiscovery({...d,candidate_assessments:[{obligation_id:'R',behavior:'rule',status:'grounded',reason:'timeout proves it',evidence_ids:['timeout']}]},context),/unobserved/);
});
