import test from 'node:test';import assert from 'node:assert/strict';import {createServer} from 'node:http';import {spawn} from 'node:child_process';import {mkdtemp,mkdir,writeFile,rm} from 'node:fs/promises';import {join} from 'node:path';import {tmpdir} from 'node:os';import {fileURLToPath} from 'node:url';
const dsh=process.env.DSH_CLI;
for(const mode of ['continue','blocked','expired','running-expiry','outage','provider-error'])test('real DSH governed actor '+mode+' with local fake provider only',{skip:!dsh},async()=>{
 const home=await mkdtemp(join(tmpdir(),'vera-governed-'));let server,child,failure;const requests=[],reviews=[];let out='',err='';
 const deadline=Date.now()+(mode==='expired'?-1000:mode==='running-expiry'?3000:60000);
 try{
 server=createServer(async(req,res)=>{try{
  let raw='';for await(const c of req)raw+=c;const r=JSON.parse(raw);
  if(req.url==='/completion/review'){
   reviews.push(r);if(mode==='outage'){res.writeHead(503);res.end('unavailable');return;}assert.equal(r.run_id,'fixture');assert.equal(r.task_hash,'task');assert.ok(r.proposal);assert.ok(r.session_id);
   const again=mode==='continue'&&reviews.length===1;
   res.end(JSON.stringify({run_id:'fixture',task_hash:'task',deadline_ms:deadline,decision:again?'continue':mode==='blocked'?'blocked':'ready_for_audit',summary:'Independent host review',artifact_digest:'frozen-'+reviews.length,completion_scope:'accepted_stop_not_task_correctness',missing:again?[{id:'adapter',requirement:'Actual adapter use',reason:'Missing boundary',next_action:'Integrate required adapter and report actual checks'}]:[],evidence:[{id:'snapshot',provenance:'trusted fixture archive'}],...(mode==='blocked'?{blocker:{kind:'inaccessible',detail:'Required dependency unavailable'}}:{})}));return;
  }
  requests.push(r);if(mode==='provider-error'){res.writeHead(500);res.end('local fixture failure');return;}if(mode==='running-expiry')return;const all=JSON.stringify(r.messages);
  const follow=all.includes('Trusted completion review:');
  const last=r.messages.at(-1);
  const submit=last.role!=='tool';
  if(follow)assert.match(all,/Integrate required adapter/);
  const proposal={status:mode==='blocked'?'blocked':follow?'ready':'partial',remaining_obligation_ids:follow?[]:['adapter'],core_artifact_paths:['src/core.rs'],adapter_artifact_paths:follow?['src/io.rs']:[],test_evidence:follow?[{command:'test boundary',result:'passed (actor claim)'}]:[],blocker:{kind:mode==='blocked'?'inaccessible':'none',detail:mode==='blocked'?'Dependency absent':''}};
  res.writeHead(200,{'content-type':'text/event-stream'});
  const chunk=(delta,finish_reason=null)=>res.write('data: '+JSON.stringify({id:'fake',object:'chat.completion.chunk',created:1,model:'fixture',choices:[{index:0,delta,finish_reason}]})+'\n\n');
  chunk({role:'assistant',...(submit?{tool_calls:[{index:0,id:'call-'+requests.length,type:'function',function:{name:'vera_completion_propose',arguments:JSON.stringify(proposal)}}]}:{content:'Actor final candidate.'})});chunk({},submit?'tool_calls':'stop');res.end('data: [DONE]\n\n');
 }catch(e){failure=e;res.writeHead(500);res.end(String(e));}});
 await new Promise(r=>server.listen(0,'127.0.0.1',r));
 await mkdir(join(home,'profiles/headless'),{recursive:true});await mkdir(join(home,'agents'));await mkdir(join(home,'workspace'));
 await writeFile(join(home,'profiles/headless/package.json'),JSON.stringify({name:'dsh-profile-headless',private:true,dependencies:{},dsh:{profile:{bundles:['@deepseek-ai/dsh-base','@deepseek-ai/dsh-headless']}}}));
 const patch=['headless-runner','session-title-llm','llm-retry','compaction-basic','command-compact','plugin-manager','tool-plugin-manager','tool-subagent','tool-subagent-fork','tool-subagent-control','tool-subagent-list-agents','tool-workflow','workflow-ptc','credentials','authorization','deepseek-account','tool-web'].map(id=>({id,disabled:true}));
 patch.push({id:'llm-pi-ai',config:{providers:{fixture:{api:'openai-completions',apiKeyEnv:'FIXTURE_KEY',baseURL:`http://127.0.0.1:${server.address().port}/v1`,models:[{id:'fixture',input:['text'],contextWindow:262144,maxTokens:4096}],retryPolicy:{mode:'normal',maxRetries:0}}}}},{id:'agent-default-model',config:{provider:'fixture',model:'fixture'}},{id:'tools',config:{mode:'native'}},{id:'skill-filesystem',config:{includeDefaultRoots:false,customSkillDirs:[],watch:false}},{insert:[{id:'vera-governed-headless',name:fileURLToPath(new URL('../governed-headless.js',import.meta.url)),config:{reviewURL:`http://127.0.0.1:${server.address().port}`,runId:'fixture',taskHash:'task',deadlineMs:deadline}}]});
 await writeFile(join(home,'patch.json'),JSON.stringify(patch));
 child=spawn(process.execPath,[dsh,'headless','--patch',join(home,'patch.json'),'--json','-'],{cwd:join(home,'workspace'),env:{PATH:process.env.PATH,HOME:home,DSH_HOME:home,DSH_AGENTS_HOME:join(home,'agents'),DSH_TELEMETRY_DISABLED:'1',FIXTURE_KEY:'fake-local'},stdio:['pipe','pipe','pipe']});
 child.stdout.on('data',x=>out+=x);child.stderr.on('data',x=>err+=x);child.stdin.end('Implement original task with required core and adapter.');
 const code=await new Promise((resolve,reject)=>{child.on('error',reject);child.on('close',resolve);});if(failure)throw failure;
 assert.equal(code,mode==='continue'?0:1,err+'\n'+out);
 const rows=out.split('\n').filter(x=>x.startsWith('{')).map(x=>JSON.parse(x));const final=rows.find(x=>x.type==='completion/final');assert.ok(final,err+'\n'+out);
 assert.equal(final.decision,mode==='continue'?'ready_for_audit':mode==='blocked'?'blocked':['outage','provider-error'].includes(mode)?'infrastructure_failure':'time_expired');
 if(mode==='provider-error'){assert.ok(requests.length>=1);assert.equal(reviews.length,0);}else if(mode==='running-expiry'){assert.equal(requests.length,1);assert.equal(reviews.length,0);}else if(mode==='expired'){assert.equal(requests.length,0);assert.equal(reviews.length,0);}else{assert.equal(reviews.length,mode==='continue'?2:1);assert.equal(new Set(reviews.map(x=>x.session_id)).size,1);assert.equal(requests.length,mode==='continue'?4:2);if(mode==='continue')assert.deepEqual(reviews[1].proposal.adapter_artifact_paths,['src/io.rs']);}
 }finally{child?.kill('SIGKILL');server?.closeAllConnections();await new Promise(r=>server?server.close(r):r());await rm(home,{recursive:true,force:true});}
});
