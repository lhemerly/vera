import {randomUUID} from 'node:crypto';
import z from '@deepseek-ai/schemastery';
import {brandString} from '@deepseek-ai/dsh-brand';
import {installModelSelection} from '@deepseek-ai/dsh-agent';
import {createUserMessage} from '@deepseek-ai/dsh-llm';
import {SessionSeq} from '@deepseek-ai/dsh-session';
import {defineTool} from '@deepseek-ai/dsh-tools';
import {completionClient,governCompletion,completionProposal} from './completion.js';
export const name='vera-governed-headless';
export const inject=['agentDefaultModel','agents','sessions','tools','systemPrompt'];
export const Config=z.object({reviewURL:z.string().required(),runId:z.string().required(),taskHash:z.string().required(),deadlineMs:z.number().required()});
function latest(session){
 let text='',reason;
 for(let i=0;i<session.seq;i++){
  const e=session.eventAt(SessionSeq(i));
  if(e?.type==='assistant/message'){
   const s=e.data.message.content.filter(b=>b.type==='text').map(b=>b.text).join('');if(s)text=s;
  }
  if(e?.type==='turn/end')reason=e.data.reason;
 }
 return {text,reason};
}
export async function runGoverned(ctx,config,proposal=()=>null,resetProposal=()=>{}){
 const binding={run_id:config.runId,task_hash:config.taskHash,deadline_ms:config.deadlineMs};
 if(!Number.isSafeInteger(config.deadlineMs))throw Error('Original absolute deadline required');
 const review=completionClient(config.reviewURL,binding);
 let task='';for await(const p of process.stdin)task+=p;
 if(!task.trim())throw Error('Task required');
 const selection=ctx.agentDefaultModel.currentSelection();
 const {agent}=await ctx.agents.create({sessionId:brandString('session-'+randomUUID()),meta:{cwd:process.cwd()},agentOptions:{provider:selection.provider,model:selection.model},setup:agentCtx=>{installModelSelection(agentCtx,{current:selection,assembled:undefined});}});
 let actorFailure;
 const stopErrors=ctx.on('agent/error',({agent:subject,error})=>{if(subject===agent)actorFailure=error;});
 const controller=new AbortController();let timedOut=false;
 const cancel=()=>{controller.abort();agent.cancel({kind:'disposed'});};
 const remaining=config.deadlineMs-Date.now();
 let timer;const scheduleDeadline=()=>{const left=config.deadlineMs-Date.now();if(left<=0){timedOut=true;cancel();}else timer=setTimeout(scheduleDeadline,Math.min(left,2147483647));};
 if(remaining>0)scheduleDeadline();
 const stop=()=>cancel();process.once('SIGTERM',stop);process.once('SIGINT',stop);
 let result;
 try{
  result=await governCompletion({agent,task,proposal,resetProposal,deadlineMs:config.deadlineMs,signal:controller.signal,followup:text=>agent.followup(createUserMessage({content:[{type:'text',text}],source:{kind:'user'}})),summary:()=>{if(actorFailure)throw actorFailure;return latest(agent.session);},review,onReview:r=>process.stdout.write(JSON.stringify({type:'completion/review',session_id:agent.session.id,...r})+'\n')});
 }catch(error){
  if(controller.signal.aborted)result={decision:timedOut?'time_expired':'cancelled',summary:'Original clock/cancellation stopped this partial attempt.'};
  else result={decision:'infrastructure_failure',summary:String(error)};
 }finally{
  clearTimeout(timer);stopErrors();process.off('SIGTERM',stop);process.off('SIGINT',stop);await ctx.sessions.flush(agent.session);
 }
 if(timedOut)result={...result,decision:'time_expired'};
 process.stdout.write(JSON.stringify({type:'completion/final',session_id:agent.session.id,...result})+'\n');
 return result;
}
export function apply(ctx,config){
 const exit=ctx.get('appExit');if(!exit)throw Error('Launcher appExit required');
 let pending=null;
 const strings={type:'array',items:{type:'string'},required:true};
 ctx.tools.register(defineTool({name:'vera_completion_propose',description:'Before finishing, report candid task status and artifact/test evidence claims for independent host review. A proved local core is not completion. No claims here establish correctness or waive obligations.',parameters:{status:{type:'string',enum:['ready','partial','blocked'],required:true},remaining_obligation_ids:strings,core_artifact_paths:strings,adapter_artifact_paths:strings,test_evidence:{type:'array',required:true,items:{type:'object',additionalProperties:false,properties:{command:{type:'string',required:true},result:{type:'string',required:true}}}},blocker:{type:'object',required:true,additionalProperties:false,properties:{kind:{type:'string',required:true,enum:['none','inaccessible','unsafe','stalled']},detail:{type:'string',required:true}}}},output:{schema:{type:'object',additionalProperties:true},render:(_a,v)=>[{type:'text',text:JSON.stringify(v)}]},execute:async args=>{pending=completionProposal(args);return {recorded:true,assurance:'actor_claims_only_not_audit'};}}));
 ctx.systemPrompt.section({name:'vera:completion-review',order:3080,interpolate:false,text:'Work on the ORIGINAL complete task, not only the verified slice. Before your final answer call vera_completion_propose with honest remaining obligation IDs, actual core and adapter paths, and test command/result claims. These are claims checked by a trusted host, not proof. Final answers are reviewed; actionable gaps continue in this SAME session within the original clock and permissions. Do not fake artifact use or conceal unknowns to exit. A concrete inaccessible, unsafe, or stalled blocker permits an honest partial stop. Ready means ready for independent audit, not verified whole-task correctness.'});
 runGoverned(ctx,config,()=>pending,()=>{pending=null;}).then(r=>exit(r.decision==='ready_for_audit'?0:1),e=>{process.stderr.write(String(e)+'\n');exit(1);});
}
