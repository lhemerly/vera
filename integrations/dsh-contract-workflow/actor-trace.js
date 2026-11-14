import {SessionSeq} from '@deepseek-ai/dsh-session';
import {lastAssistantStreamChunk} from '@deepseek-ai/dsh-llm/assistant-stream';
const streamUsage=stream=>Array.isArray(stream)?lastAssistantStreamChunk(stream,'usage')?.usage:undefined;
const counters=['inputTokens','outputTokens','totalTokens','cacheReadTokens','cacheWriteTokens','reasoningTokens'];
/** Only provider-reported safe counters. Absent fields remain unknown, never zero. */
function addUsage(state,next){
 if(!next||!['inputTokens','outputTokens'].every(k=>Number.isSafeInteger(next[k])&&next[k]>=0))return {...state,complete:false};
 const value={};
 for(const k of counters){
  if(!Number.isSafeInteger(next[k])||next[k]<0)continue;
  if(state.value===null)value[k]=next[k];
  else if(Number.isSafeInteger(state.value[k])&&Number.isSafeInteger(state.value[k]+next[k]))value[k]=state.value[k]+next[k];
 }
 return {complete:state.complete,value};
}
/** Actor-only durable event projection. Not an admission audit or semantic evidence validator. */
export function projectActorTrace(ctx,agent,sink){
 let disposed=false,usage={complete:true,value:null};
 const write=row=>sink.write(JSON.stringify(row)+'\n');
 const consume=(session,event)=>{
  if(disposed||session!==agent.session)return;
  write({type:'session/event',session_id:agent.session.id,event});
  const d=event.data??{};
  switch(event.type){
   case 'turn/start':case 'step/start':case 'turn/end':
    write({type:'status',phase:event.type.replace('/','_'),turn:d.turn,step:d.step,...(d.reason?{reason:d.reason}:{})});break;
   case 'assistant/attempt':
    usage=addUsage(usage,streamUsage(d.stream));break;
   case 'assistant/message':
    usage=addUsage(usage,d.usage??streamUsage(d.stream));
    for(const block of d.message?.content??[])if(block.type==='text'||block.type==='reasoning')write({type:block.type==='reasoning'?'thinking':'text',text:block.text});break;
   case 'step/end':
    write({type:'status',phase:'step_end',turn:d.turn,step:d.step,...(usage.complete&&usage.value&&Number.isSafeInteger(usage.value.inputTokens)&&Number.isSafeInteger(usage.value.outputTokens)?{usage:usage.value}:{usageUnknown:true})});usage={complete:true,value:null};break;
   case 'tool/call':{
    let input=d.arguments;try{if(typeof input==='string')input=JSON.parse(input);}catch{/* preserve malformed original */}
    write({type:'tool_call',callId:d.callId,tool:d.name,input});break;
   }
   case 'tool/result':
    if(event.surfaceOp==='append')write({type:'tool_result',callId:d.message?.toolCallId,status:d.message?.isError?'error':'completed',result:d.message?.content??[]});break;
  }
 };
 write({type:'session',sessionId:agent.session.id,header:agent.session.header,cwd:process.cwd(),trace_scope:'actor_session_not_trusted_authority'});
 const stop=ctx.on('session/event',consume);
 // Initial durable setup events precede subscription. Replay before first followup.
 for(let i=0;i<agent.session.seq;i++){const event=agent.session.eventAt(SessionSeq(i));if(event)consume(agent.session,event);}
 return {dispose(){if(!disposed){disposed=true;stop();}},finish(text,result){write({type:'final',text,decision:result.decision,completion_scope:'accepted_stop_not_task_correctness'});}};
}
