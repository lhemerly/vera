const text={type:'string'},list={type:'array',items:text};
export const CORE_SCOPE_SCHEMA={type:'object',additionalProperties:false,properties:{
 guarantee:text,domain:text,assumptions:list,
 input_mapping:{type:'array',items:{type:'object',additionalProperties:false,properties:{parameter:text,origin:text,role:{type:'string',enum:['data','precomputed_decision']}},required:['parameter','origin','role']}},
 host_obligations:list,unguaranteed_behavior:list,out_of_domain_behavior:text,
 equivalence_claim:{type:'string',enum:['local_guarantee_only']}
},required:['guarantee','domain','assumptions','input_mapping','host_obligations','unguaranteed_behavior','out_of_domain_behavior','equivalence_claim']};
export const BLOCKER_SCHEMA={type:'object',additionalProperties:false,properties:{kind:{type:'string',enum:['none','unavailable_dependency','contradictory_observations','unsupported_observation']},detail:text,local_scope_impact:text,evidence_ids:list},required:['kind','detail','local_scope_impact','evidence_ids']};
export function authorityCompletionGuard(result){
 if(result?.outcome!=='needs_evidence')return;
 const b=result.discovery?.blocker;
 if(!b||b.kind==='none'||!['unavailable_dependency','contradictory_observations','unsupported_observation'].includes(b.kind)||typeof b.detail!=='string'||!b.detail.trim()||typeof b.local_scope_impact!=='string'||!b.local_scope_impact.trim())
  return 'Premature evidence deferral: identify a concrete unavailable dependency, contradiction, or unsupported observation and explain its impact on the LOCAL slice. If your next discriminating probe is permitted and executable, continue discovery here before concluding; you choose the probe, nothing is auto-executed. Missing whole-command or remote-system equivalence is not itself a blocker for a useful local guarantee. Legitimate blockers and not_suitable remain allowed; never invent a core or exceed permissions.';
}
export function validateCoreScope(scope){
 const nonempty=(v,n)=>{if(typeof v!=='string'||!v.trim())throw Error('Core scope needs '+n);};
 if(!scope||scope.equivalence_claim!=='local_guarantee_only')throw Error('Core scope must claim local guarantee only, not whole equivalence');
 for(const k of ['guarantee','domain','out_of_domain_behavior'])nonempty(scope[k],k);
 for(const k of ['assumptions','host_obligations','unguaranteed_behavior']){if(!Array.isArray(scope[k]))throw Error('Core scope needs '+k);for(const v of scope[k])nonempty(v,k);}
 if(!scope.host_obligations.length||!scope.unguaranteed_behavior.length)throw Error('Core scope must retain explicit host obligations and unguaranteed behavior');
 if(!Array.isArray(scope.input_mapping)||!scope.input_mapping.length)throw Error('Core scope needs input mapping');
 for(const p of scope.input_mapping){nonempty(p.parameter,'parameter');nonempty(p.origin,'input origin');if(!['data','precomputed_decision'].includes(p.role))throw Error('Invalid input role');}
 if(!scope.input_mapping.some(p=>p.role==='data'))throw Error('Core cannot merely re-prove a host-precomputed decision');
 return structuredClone({...scope,assurance:'declared_compositional_scope_not_host_equivalence'});
}
export const SCOPE_GUIDANCE='Use compositional verification: a useful local primitive need not prove its entire command, parser, protocol, remote service or application. State exactly its guarantee, domain and assumptions, genuine input mapping, host obligations, unguaranteed behavior and faithful handling outside its domain. Keep original obligations intact; local proof does not discharge unspecified host behavior. Do not reject a meaningful local decision merely because other options, formatting, transport or remote execution remain unproved. Conversely, a few observed flags do not establish an entire parser. Do not add arbitrary input-length restrictions, truncate accepted inputs, or reject valid out-of-domain inputs just to fit verification: use incremental/streaming composition or faithful host handling and state remaining obligations. Original semantic requirements are unchanged. Before needs_evidence, continue permitted executable observations that address your remaining local uncertainty. A real unavailable dependency, contradiction or unsupported observation can justify deferral; explain why it blocks this local slice rather than merely whole-application equivalence. Never manufacture a guarantee or cross permissions to avoid deferral.';
