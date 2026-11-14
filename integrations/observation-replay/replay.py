#!/usr/bin/env python3
"""Actor-owned empirical CLI regression aid. NOT a sandbox or trusted proof service."""
import argparse,base64,contextlib,fcntl,hashlib,json,os,shutil,signal,subprocess,time,uuid
from pathlib import Path
TRUST='actor_reported_reference_observation_not_protected_authority'
def digest(x):return hashlib.sha256(json.dumps(x,sort_keys=True,separators=(',',':')).encode()).hexdigest()
def b64(x):return base64.b64encode(x).decode()
def un64(x):return base64.b64decode(x,validate=True)
def relative(x):
 if not isinstance(x,str) or not x or Path(x).is_absolute() or any(p in ('..','.') for p in x.split('/')) or '\0' in x or '\\' in x:raise ValueError('fixture/output path must be nontraversing relative path')
 return x

def case_request(x):
 allowed={'argv','stdin_b64','files','output_paths','obligation_ids','interaction'}
 if not isinstance(x,dict) or set(x)-allowed:raise ValueError('Unknown case fields; expected outcomes and arbitrary commands are not accepted')
 y={**dict(argv=[],stdin_b64='',files=[],output_paths=[],obligation_ids=[],interaction='plain_cli'),**x}
 if not isinstance(y['argv'],list) or any(not isinstance(a,str) or '\0' in a for a in y['argv']):raise ValueError('argv must be strings')
 un64(y['stdin_b64'])
 names=set()
 for f in y['files']:
  if not isinstance(f,dict) or set(f)!={'path','data_b64'}:raise ValueError('file requires path/data_b64')
  p=relative(f['path']);un64(f['data_b64'])
  if p in names:raise ValueError('duplicate fixture')
  names.add(p)
 for p in y['output_paths']:relative(p)
 if not isinstance(y['obligation_ids'],list) or any(not isinstance(i,str) or not i for i in y['obligation_ids']):raise ValueError('obligation IDs must be strings')
 if not isinstance(y['interaction'],str):raise ValueError('interaction must be text')
 return y

def command_spec(spec):
 command=spec['command']
 if not isinstance(command,list) or not command or any(not isinstance(s,str) or '\0' in s for s in command) or not Path(command[0]).is_absolute():raise ValueError('configured command must be an absolute executable argv array')
 return command

def identity(spec):
 command=command_spec(spec)
 paths=set([command[0],*spec.get('artifact_paths',[])])
 # Include script/launcher file arguments, not just an interpreter executable.
 paths.update(a for a in command[1:] if Path(a).is_absolute())
 records=[]
 for p in sorted(paths):
  q=Path(p)
  if not q.is_absolute() or not q.is_file():raise ValueError('artifact_paths must be existing absolute files')
  provided=spec.get('artifact_metadata',{}).get(p)
  if provided is not None:
   h=provided.get('sha256','')
   if len(h)!=64 or any(c not in '0123456789abcdef' for c in h) or not provided.get('provenance'):raise ValueError('provided artifact metadata requires sha256 and provenance')
   records.append({'path':str(q),'resolved_path':str(q.resolve()),'sha256':h,'provenance':provided['provenance'],'verification':'provided_not_recomputed_by_actor'})
  else:records.append({'path':str(q),'resolved_path':str(q.resolve()),'sha256':hashlib.sha256(q.read_bytes()).hexdigest(),'verification':'locally_read_hash_not_protected_provenance'})
 result={'command':command,'files':records,'runtime_provenance':spec.get('runtime_provenance','unspecified'),'binding_scope':'configured executable and listed artifact files; not full environment'}
 return {**result,'available':True,'digest':digest(result)}

def available_identity(spec):
 try:return identity(spec)
 except Exception as e:return {'available':False,'digest':None,'error':str(e)}

def outcome_key(o):
 return {k:o[k] for k in ('status','exit_code','stdout_b64','stderr_b64','outputs')}

class Ledger:
 def __init__(self,config):
  self.config=config;self.root=Path(config['directory']).resolve();self.root.mkdir(parents=True,exist_ok=True)
  self.work=self.root/'active-fixture';self.timeout=config.get('process_timeout_seconds',30)
  if not isinstance(self.timeout,(int,float)) or self.timeout<=0:raise ValueError('positive host-config process timeout required')
  # This timeout is operational observation classification, never a VERA proof cap.
  for role in ('reference','candidate'):command_spec(config[role])
  if config['candidate'].get('artifact_metadata'):raise ValueError('candidate identities must be recomputed, not supplied')
 @contextlib.contextmanager
 def locked(self):
  with (self.root/'ledger.lock').open('a') as lock:fcntl.flock(lock,fcntl.LOCK_EX);yield
 def save(self,id,record):
  p=self.root/(id+'.json');tmp=p.with_suffix('.tmp');tmp.write_text(json.dumps(record,indent=2)+'\n');tmp.replace(p)
 def load(self,id):
  if not isinstance(id,str) or len(id)!=32 or any(c not in '0123456789abcdef' for c in id):raise ValueError('invalid case ID')
  return json.loads((self.root/(id+'.json')).read_text())
 def execute(self,spec,request):
  started=time.time();result={'status':'infrastructure_error','exit_code':None,'stdout_b64':'','stderr_b64':'','outputs':[]}
  try:
   if self.work.exists():shutil.rmtree(self.work)
   self.work.mkdir();(self.work/'home').mkdir()
   for f in request['files']:
    p=self.work/f['path'];p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(un64(f['data_b64']))
   # Same absolute fixture root every run; fresh contents prevent cross-case state leakage.
   env={'PATH':'/usr/bin:/bin','HOME':str(self.work/'home'),'LC_ALL':'C','TZ':'UTC'}
   process=subprocess.Popen(spec['command']+request['argv'],cwd=self.work,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
   try:
    out,err=process.communicate(un64(request['stdin_b64']),timeout=self.timeout);result.update(status='exited',exit_code=process.returncode)
   except subprocess.TimeoutExpired:
    os.killpg(process.pid,signal.SIGKILL);out,err=process.communicate();result.update(status='incomplete_timeout',exit_code=process.returncode)
   finally:
    try:os.killpg(process.pid,signal.SIGKILL)
    except ProcessLookupError:pass
   result.update(stdout_b64=b64(out),stderr_b64=b64(err))
   for name in request['output_paths']:
    p=self.work/name;entry={'path':name}
    if p.is_symlink() or not p.resolve().is_relative_to(self.work.resolve()):entry['status']='unsupported_symlink';result['status']='unsupported_output'
    elif not p.exists():entry['status']='missing'
    elif p.is_file():entry.update(status='file',data_b64=b64(p.read_bytes()))
    else:entry['status']='unsupported_nonfile';result['status']='unsupported_output'
    result['outputs'].append(entry)
  except Exception as e:result.update(status='infrastructure_error',error=str(e))
  return {**result,'duration_seconds':time.time()-started}
 def observe(self,raw):
  with self.locked():
   id=uuid.uuid4().hex;record={'id':id,'trust':TRUST,'created':time.time(),'replays':[],'completeness':'not_established','status':'pending'}
   self.save(id,record)
   try:
    request=case_request(raw);record['request']=request;before=identity(self.config['reference']);record['reference_identity']=before
    if request['interaction']!='plain_cli':record.update(status='unsupported',reason='Only isolated plain CLI cases supported; no socket, interactive, or cross-invocation state fidelity claimed')
    else:
     record['reference_runs']=[];self.save(id,record)
     for _ in range(2):
      record['reference_runs'].append(self.execute(self.config['reference'],request));self.save(id,record)
     runs=record['reference_runs'];after=identity(self.config['reference'])
     if before!=after:record.update(status='incomplete',reason='reference artifact changed during observation')
     elif any(r['status']!='exited' for r in runs):record.update(status='incomplete',reason='reference did not produce two complete observable outcomes')
     elif outcome_key(runs[0])!=outcome_key(runs[1]):record.update(status='unstable',reason='Repeated identical fixtures produced different outcomes; no normalization applied')
     else:record.update(status='recorded',observed_repeatability='two matching observations, not proof of determinism')
   except Exception as e:record.update(status='incomplete' if 'request' in record else 'rejected',reason=str(e))
   self.save(id,record);return record
 def check(self,raw):
  """Capture exact oracle case, then retain a baseline before caller repairs it."""
  record=self.observe(raw)
  self.replay([record['id']])
  return self.load(record['id'])
 def replay(self,ids):
  with self.locked():
   before=available_identity(self.config['candidate']);rows=[]
   if not ids:ids=[p.stem for p in sorted(self.root.glob('*.json'))]
   for id in ids:
    record=self.load(id);row={'time':time.time(),'candidate_identity':before,'trust':TRUST,'status':'pending','identity_verified_after':False}
    record['replays'].append(row);self.save(id,record)
    try:
     if not before['available']:row.update(status='incomplete',reason='candidate unavailable: '+before['error'])
     elif record['status']!='recorded':row.update(status='not_comparable',reason=record['status'])
     elif available_identity(self.config['reference'])!=record['reference_identity']:row.update(status='reference_changed',reason='Record a new case for changed reference; old bytes retained')
     elif available_identity(self.config['candidate'])!=before:row.update(status='incomplete',reason='candidate changed before case replay')
     else:
      actual=self.execute(self.config['candidate'],record['request']);row['actual']=actual;self.save(id,record)
      if actual['status']!='exited':row.update(status='incomplete',reason=actual['status'])
      else:
       expected=outcome_key(record['reference_runs'][0]);got=outcome_key(actual);diff={k:{'reference':expected[k],'candidate':got[k]} for k in expected if expected[k]!=got[k]}
       row.update(status='mismatch' if diff else 'match',differences=diff)
      after=available_identity(self.config['candidate']);row['candidate_identity_after']=after
      if before!=after:row.update(status='incomplete',reason='candidate artifact changed during replay')
      else:row['identity_verified_after']=True
    except Exception as e:row.update(status='incomplete',reason=str(e))
    self.save(id,record);rows.append({'id':id,**row})
   return {'trust':TRUST,'candidate_identity':before,'results':rows,'completeness':'not_established'}
 def summary(self):
  with self.locked():
   current=available_identity(self.config['candidate']);reference=available_identity(self.config['reference']);rows=[]
   for path in sorted(self.root.glob('*.json')):
    r=json.loads(path.read_text());last=r['replays'][-1] if r['replays'] else None
    rows.append({'id':r['id'],'obligation_ids':r.get('request',{}).get('obligation_ids',[]),'observation_status':r['status'],'last_replay_status':last['status'] if last else None,'current_candidate':bool(current['available'] and last and last['candidate_identity']==current),'current_reference':r.get('reference_identity')==reference,'current_match':bool(current['available'] and reference['available'] and last and last['status']=='match' and last.get('identity_verified_after') and last['candidate_identity']==current and r.get('reference_identity')==reference),'evidence_scope':'only this observed case; no obligation completeness inference'})
   return {'trust':TRUST,'candidate_identity':current,'cases':rows,'completeness':'not_established'}

def main():
 p=argparse.ArgumentParser();p.add_argument('--config',required=True);p.add_argument('operation',choices=['observe','check','replay','list']);p.add_argument('ids',nargs='*');a=p.parse_args();ledger=Ledger(json.loads(Path(a.config).read_text()))
 if a.operation in ('observe','check'):result=getattr(ledger,a.operation)(json.load(__import__('sys').stdin))
 elif a.operation=='replay':result=ledger.replay(a.ids)
 else:result=ledger.summary()
 print(json.dumps(result,indent=2))
if __name__=='__main__':main()
