import hashlib,json,tempfile,unittest
from unittest.mock import patch
from types import SimpleNamespace
from observer import Observer,validate
IMAGE='test/image@sha256:'+'a'*64
class ObserverTests(unittest.TestCase):
 def test_rejects_unsafe_requests(self):
  for request in [{'argv':[],'image':'x'},{'argv':'x'},{'argv':['x\0']},{'argv':[],'fixtures':[{'path':'../escape','content':'x'}]},{'argv':[],'fixtures':[{'path':'/absolute','content':'x'}]},{'argv':[],'fixtures':[{'path':'home/token','content':'x'}]},{'argv':[],'fixtures':[{'path':'x','content':'x'},{'path':'x/y','content':'y'}]}]:
   with self.subTest(request=request),self.assertRaises(ValueError):validate(request)
 def test_no_shell_no_mount_no_network_and_fixed_identity(self):
  with tempfile.TemporaryDirectory() as d:
   o=Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d);cmd=o.command('unique')
   self.assertIn('--read-only',cmd);self.assertIn('none',cmd);self.assertNotIn('--privileged',cmd);self.assertNotIn('--mount',cmd);self.assertNotIn('-v',cmd);self.assertEqual(cmd[-1],'/workspace/executable');self.assertIn('no-new-privileges',cmd)
   with self.assertRaises(ValueError):Observer(image='floating:latest',executable='/workspace/executable',docker='/docker',directory=d)
 def test_complete_record_binds_inputs_outputs_and_identity(self):
  with tempfile.TemporaryDirectory() as d,patch('observer.subprocess.run',return_value=SimpleNamespace(returncode=0,stdout=json.dumps({'status':'completed','exit_code':2,'stdout':'','stderr':'invalid input'}),stderr='')) as run:
   o=Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d);r=o({'argv':['--bad']})
   self.assertTrue(r['complete']);self.assertEqual(hashlib.sha256(r['content'].encode()).hexdigest(),r['content_hash']);self.assertEqual(json.loads(r['content'])['request']['argv'],['--bad']);self.assertNotIn('shell',run.call_args.kwargs)
 def test_incomplete_never_credited_complete(self):
  for status in ['probe_timeout','output_truncated']:
   with tempfile.TemporaryDirectory() as d,patch('observer.subprocess.run',return_value=SimpleNamespace(returncode=0,stdout=json.dumps({'status':status,'exit_code':-9}),stderr='')):
    self.assertFalse(Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d)({'argv':[]})['complete'])
 def test_container_failure_is_infrastructure_not_target_failure(self):
  with tempfile.TemporaryDirectory() as d,patch('observer.subprocess.run',return_value=SimpleNamespace(returncode=137,stdout='',stderr='killed')):
   r=Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d)({'argv':[]});self.assertFalse(r['complete']);self.assertIsNone(json.loads(r['content'])['result']['exit_code'])
 def test_signal_and_malformed_results_are_incomplete(self):
  for payload in [{'status':'completed','exit_code':-9},[],None]:
   with tempfile.TemporaryDirectory() as d,patch('observer.subprocess.run',return_value=SimpleNamespace(returncode=0,stdout=json.dumps(payload),stderr='')):
    r=Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d)({'argv':[]});self.assertFalse(r['complete'])
 def test_timeout_cleanup_uncertainty_is_persisted(self):
  import subprocess
  with tempfile.TemporaryDirectory() as d,patch('observer.subprocess.run',side_effect=subprocess.TimeoutExpired('docker',45)):
   o=Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d);r=o({'argv':[]})
   with self.assertRaises(RuntimeError):o({'argv':[]})
   self.assertFalse(r['complete']);self.assertFalse(json.loads(r['content'])['result']['cleanup']['verified_stopped_or_absent'])
 def test_probe_concurrency_is_fail_closed(self):
  with tempfile.TemporaryDirectory() as d:
   o=Observer(image=IMAGE,executable='/workspace/executable',docker='/docker',directory=d);o.lock.acquire()
   try:
    with self.assertRaises(RuntimeError):o({'argv':[]})
   finally:o.lock.release()
if __name__=='__main__':unittest.main()
