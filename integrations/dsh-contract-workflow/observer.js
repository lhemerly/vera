import http from 'node:http';
import {createHash} from 'node:crypto';
const digest=value=>createHash('sha256').update(value).digest('hex');
export function observationRequest(value){
 if(!value||typeof value!=='object'||Array.isArray(value)||Object.keys(value).some(k=>!['argv','stdin','fixtures'].includes(k)))throw Error('Invalid observation request');
 if(!Array.isArray(value.argv)||value.argv.some(x=>typeof x!=='string'||x.includes('\0')))throw Error('argv must be a string array');
 if(value.stdin!==undefined&&typeof value.stdin!=='string')throw Error('stdin must be text');
 if(value.fixtures!==undefined&&(!Array.isArray(value.fixtures)||value.fixtures.some(f=>!f||typeof f!=='object'||Array.isArray(f)||Object.keys(f).length!==2||typeof f.path!=='string'||typeof f.content!=='string'||!f.path||f.path.startsWith('/')||f.path.split('/').some(x=>!x||x==='.'||x==='..')||f.path.includes('\0'))))throw Error('fixtures must use relative nontraversing paths and text content');
 return structuredClone(value);
}
export function observerClient(base){
 const u=new URL(base);
 if(u.protocol!=='http:'||!['127.0.0.1','[::1]'].includes(u.hostname)||u.username||u.password||u.search||u.hash||!['','/'].includes(u.pathname))throw Error('Observer must be fixed loopback HTTP origin without credentials/path');
 u.pathname='/reference/observe';
 return async(value,signal)=>{
  const request=observationRequest(value);
  const result=await new Promise((resolve,reject)=>{
   const req=http.request(u,{method:'POST',signal,headers:{'content-type':'application/json'}},res=>{
    let raw='';res.setEncoding('utf8');res.on('data',s=>raw+=s);res.on('error',reject);res.on('end',()=>{try{if(res.statusCode!==200)throw Error('Observer HTTP '+res.statusCode);resolve(JSON.parse(raw));}catch(e){reject(e);}});
   });req.on('error',reject);req.end(JSON.stringify(request));
  });
  if(!result||typeof result!=='object'||Array.isArray(result)||typeof result.id!=='string'||!result.id.trim()||typeof result.provenance!=='string'||!result.provenance.trim()||typeof result.content!=='string'||typeof result.complete!=='boolean'||result.content_hash!==digest(result.content))throw Error('Invalid trusted observation response');
  return {id:result.id,provenance:result.provenance,content:result.content,content_hash:result.content_hash,complete:result.complete};
 };
}
