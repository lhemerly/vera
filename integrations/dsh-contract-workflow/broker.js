import http from 'node:http';
import {chmod,lstat} from 'node:fs/promises';
import {dirname,isAbsolute} from 'node:path';
export const operations=Object.freeze({vera_task_get:[],vera_workflow_start:['requirements'],vera_workflow_get:['id'],vera_workflow_step:['id'],vera_workflow_retry:['id'],vera_workflow_petition:['id','explanation']});
export function validateRequest(v){
 if(!v||typeof v!=='object'||Array.isArray(v)||Object.keys(v).some(k=>!['method','args'].includes(k))||!Object.hasOwn(operations,v.method))throw Error('Invalid lifecycle envelope');
 const f=operations[v.method],a=v.args;
 if(!a||typeof a!=='object'||Array.isArray(a)||Object.keys(a).length!==f.length||f.some(k=>typeof a[k]!=='string'||!a[k].trim())||Object.keys(a).some(k=>!f.includes(k)))throw Error('Invalid lifecycle arguments');
 return v;
}
export async function serveBroker({socketPath,actorUid,dispatch}){
 if(!isAbsolute(socketPath)||!Number.isInteger(actorUid)||actorUid<0||process.getuid()===actorUid)throw Error('Absolute socket and different actor UID required');
 const parent=await lstat(dirname(socketPath));
 if(!parent.isDirectory()||parent.isSymbolicLink()||parent.uid!==process.getuid()||(parent.mode&0o027))throw Error('Socket parent must be broker-owned and not group/world writable or world accessible');
 try{await lstat(socketPath);throw Error('Existing socket refused');}catch(e){if(e.code!=='ENOENT')throw e;}
 let busy=false;const active=new Set();
 const server=http.createServer(async(req,res)=>{
  if(req.method!=='POST'||req.url!=='/lifecycle'){res.writeHead(404);res.end();return;}
  if(busy){res.writeHead(409);res.end(JSON.stringify({error:'Lifecycle busy'}));return;}
  busy=true;const controller=new AbortController();active.add(controller);res.on('close',()=>{if(!res.writableEnded)controller.abort();});
  try{let raw='';for await(const b of req){raw+=b;if(Buffer.byteLength(raw)>1048576)throw Error('Envelope too large');}
   const {method,args}=validateRequest(JSON.parse(raw));const result=await dispatch(method,args,controller.signal);
   res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({result}));
  }catch(e){if(!res.headersSent)res.writeHead(400,{'content-type':'application/json'});res.end(JSON.stringify({error:String(e.message)}));}
  finally{active.delete(controller);busy=false;}
 });
 // Default HTTP upload/header deadlines remain; no proof or model execution timeout added.
 await new Promise((r,j)=>{server.once('error',j);server.listen(socketPath,r);});await chmod(socketPath,0o660);
 return async()=>{for(const c of active)c.abort();server.closeAllConnections();await new Promise(r=>server.close(r));};
}
export function callBroker(socketPath,method,args,signal){
 validateRequest({method,args});return new Promise((resolve,reject)=>{
  const req=http.request({socketPath,path:'/lifecycle',method:'POST',signal,headers:{'content-type':'application/json'}},res=>{
   let raw='';res.setEncoding('utf8');res.on('data',s=>raw+=s);res.on('error',reject);res.on('end',()=>{try{const v=JSON.parse(raw);if(res.statusCode!==200||v.error)throw Error(v.error??'Broker failed');resolve(v.result);}catch(e){reject(e);}});
  });req.on('error',reject);req.end(JSON.stringify({method,args}));
 });
}
