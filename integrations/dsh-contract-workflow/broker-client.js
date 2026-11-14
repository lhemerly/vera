import z from '@deepseek-ai/schemastery';
import {defineTool} from '@deepseek-ai/dsh-tools';
import {operations,callBroker} from './broker.js';
export const name='vera-authority-client';
export const inject=['tools','systemPrompt'];
export const Config=z.object({socketPath:z.string().required()});
export function apply(ctx,config){
 for(const [name,fields] of Object.entries(operations))ctx.tools.register(defineTool({name,description:'Independent task authority. Start once; petition for revisions; get/task_get recover state. Start/step/retry/petition may invoke models.',parameters:Object.fromEntries(fields.map(k=>[k,{type:'string',required:true,description:k==='requirements'?'Proposed core focus, not replacement task':k}])),output:{schema:{type:'object',additionalProperties:true},render:(_a,v)=>[{type:'text',text:JSON.stringify(v)}]},execute:(args,exec)=>callBroker(config.socketPath,name,args,exec.signal)}));
 ctx.systemPrompt.section({name:'vera:independent-task-authority',order:3070,text:'VERA workflow tools call a protected authority. Start one task workflow, step to an admitted core, and petition the same workflow for changes. Caller focus cannot replace original requirements. Preserve and consume admitted outputs; admission proves declared core properties, not host behavior or completeness. Ordinary VERA tools do not substitute for task-bound admission.',interpolate:false});
}
