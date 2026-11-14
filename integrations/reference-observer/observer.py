"""Host-only, fixed-reference observations in disposable disconnected containers.
No shell, model-selected image/executable/environment, host mounts or credentials.
Operational probe limits are NOT proof/verification limits or semantic evidence.
"""
import base64,hashlib,json,os,re,subprocess,time,uuid,threading
from pathlib import PurePosixPath,Path

# Runs inside the isolated reference image as its unprivileged actor user.
CHILD=r'''
import base64,json,os,selectors,signal,subprocess,sys,time
from pathlib import Path
r=json.load(sys.stdin);root=Path('/tmp/observation');root.mkdir();(root/'home').mkdir()
for f in r.get('fixtures',[]):
 p=root/f['path'];p.parent.mkdir(parents=True,exist_ok=True);p.write_text(f['content'])
env={'PATH':'/usr/local/bin:/usr/bin:/bin','HOME':str(root/'home'),'XDG_CONFIG_HOME':str(root/'home/.config'),'XDG_DATA_HOME':str(root/'home/.local/share'),'LANG':'C.UTF-8'}
p=subprocess.Popen([sys.argv[1],*r['argv']],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd=root,env=env,start_new_session=True)
# communicate handles stdin concurrently; output is limited by the container memory budget.
try:
 out,err=p.communicate(r.get('stdin','').encode(),timeout=30);reason='completed' if p.returncode>=0 else 'target_signal'
except subprocess.TimeoutExpired:
 os.killpg(p.pid,signal.SIGKILL);out,err=p.communicate();reason='probe_timeout'
limit=262144
if len(out)>limit or len(err)>limit:reason='output_truncated'
result={'status':reason,'exit_code':p.returncode,'stdout_base64':base64.b64encode(out[:limit]).decode(),'stderr_base64':base64.b64encode(err[:limit]).decode(),'stdout':out[:limit].decode('utf8','replace'),'stderr':err[:limit].decode('utf8','replace'),'stdout_bytes':len(out),'stderr_bytes':len(err),'environment':env,'cwd':str(root),'probe_limit_seconds':30,'output_limit_bytes_per_stream':limit}
print(json.dumps(result))
'''

def validate(request):
 if not isinstance(request,dict) or set(request)-{'argv','stdin','fixtures'}:raise ValueError('Invalid observation fields')
 if not isinstance(request.get('argv'),list) or any(not isinstance(s,str) or '\0' in s for s in request['argv']):raise ValueError('argv must be strings without NUL')
 if not isinstance(request.get('stdin',''),str):raise ValueError('stdin must be UTF-8 text')
 fixtures=request.get('fixtures',[])
 if not isinstance(fixtures,list):raise ValueError('fixtures must be an array')
 seen=set()
 for f in fixtures:
  if not isinstance(f,dict) or set(f)!={'path','content'} or not isinstance(f['path'],str) or not isinstance(f['content'],str):raise ValueError('Invalid fixture')
  path=PurePosixPath(f['path'])
  if not f['path'] or '\0' in f['path'] or path.is_absolute() or '..' in path.parts or path==PurePosixPath('.') or path.parts[0]=='home' or str(path) in seen:raise ValueError('Unsafe or duplicate fixture path')
  seen.add(str(path))
 for path in seen:
  if any(str(parent) in seen for parent in PurePosixPath(path).parents):raise ValueError('Conflicting fixture paths')
 if len(json.dumps(request).encode())>1048576:raise ValueError('Observation request too large')
 return request

class Observer:
 def __init__(self,*,image,executable,docker,directory,environment=None):
  if not re.fullmatch(r'[^\s]+@sha256:[0-9a-f]{64}',image):raise ValueError('Pinned image digest required')
  if not PurePosixPath(executable).is_absolute() or '\0' in executable:raise ValueError('Fixed absolute executable required')
  self.image=image;self.executable=executable;self.docker=docker;self.directory=Path(directory);self.env=environment
  self.directory.mkdir(parents=True,exist_ok=True,mode=0o700)
  self.lock=threading.Lock()
  self.blocked=False
 def command(self,name):
  return [self.docker,'run','--rm','--name',name,'--network','none','--read-only','--cap-drop','ALL','--security-opt','no-new-privileges','--user','agent','--memory','1g','--memory-swap','1g','--cpus','1','--pids-limit','64','--tmpfs','/tmp:rw,nosuid,nodev,size=64m','--entrypoint','python3','-i',self.image,'-c',CHILD,self.executable]
 def __call__(self,request):
  validate(request)
  if self.blocked:raise RuntimeError('Observer cleanup uncertain; trusted host reconciliation required')
  if not self.lock.acquire(blocking=False):raise RuntimeError('Reference observer busy; retry after current observation')
  try:return self._observe(request)
  finally:self.lock.release()
 def _cleanup(self,name):
  outcome={'container':name,'verified_stopped_or_absent':False}
  try:
   k=subprocess.run([self.docker,'kill',name],env=self.env,capture_output=True,text=True,timeout=15)
   outcome['kill_exit']=k.returncode
   i=subprocess.run([self.docker,'inspect','--format','{{.State.Running}}',name],env=self.env,capture_output=True,text=True,timeout=15)
   outcome['verified_stopped_or_absent']=(i.returncode==0 and i.stdout.strip()=='false') or (i.returncode!=0 and ('No such object' in i.stderr or 'No such container' in i.stderr))
   if not outcome['verified_stopped_or_absent']:outcome['diagnostic']='Container cessation unconfirmed; host inspection required'
  except (OSError,subprocess.TimeoutExpired) as e:outcome['diagnostic']='Cleanup uncertain: '+type(e).__name__
  return outcome
 def _observe(self,request):
  validate(request);name='vera-observe-'+uuid.uuid4().hex;started=time.time()
  try:
   p=subprocess.run(self.command(name),input=json.dumps(request),capture_output=True,text=True,env=self.env,timeout=45)
   if p.returncode!=0:result={'status':'observer_infrastructure_error','exit_code':None,'diagnostic':p.stderr[-2000:],'cleanup':self._cleanup(name)}
   else:
    try:
     result=json.loads(p.stdout)
     if not isinstance(result,dict) or result.get('status') not in {'completed','target_signal','probe_timeout','output_truncated'} or not isinstance(result.get('exit_code'),int):raise ValueError('Invalid target result')
     if result['exit_code']<0 and result['status']=='completed':result['status']='target_signal'
    except (ValueError,TypeError):result={'status':'observer_invalid_response','exit_code':None,'diagnostic':p.stderr[-2000:]}
  except (OSError,subprocess.TimeoutExpired) as e:
   result={'status':'observer_infrastructure_timeout' if isinstance(e,subprocess.TimeoutExpired) else 'observer_launch_error','exit_code':None,'cleanup':self._cleanup(name)}
  if result.get('cleanup') and not result['cleanup']['verified_stopped_or_absent']:self.blocked=True
  content=json.dumps({'request':request,'result':result,'reference':{'image':self.image,'executable':self.executable},'container':name,'started':started,'ended':time.time()},sort_keys=True,separators=(',',':'))
  digest=hashlib.sha256(content.encode()).hexdigest()
  record={'id':'runtime-'+digest,'provenance':'Fresh disconnected read-only container; fixed reference '+self.image+' '+self.executable,'content':content,'content_hash':digest,'complete':result.get('status')=='completed' and isinstance(result.get('exit_code'),int)}
  path=self.directory/(record['id']+'.json')
  with path.open('x') as f:json.dump(record,f,indent=2);f.write('\n');f.flush();os.fsync(f.fileno())
  return record
