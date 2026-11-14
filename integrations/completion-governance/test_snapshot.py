import hashlib
import io
import json
import tarfile
import tempfile
import unittest
import subprocess
from unittest.mock import patch
from pathlib import Path
from docker_snapshot import DockerSnapshot

class SnapshotTests(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.task=b'{"id":"example"}'
  buf=io.BytesIO()
  with tarfile.open(fileobj=buf,mode='w') as t:
   data=b'original';m=tarfile.TarInfo('main.rs');m.size=len(data);t.addfile(m,io.BytesIO(data))
   data=b'not extracted';m=tarfile.TarInfo('../escape');m.size=len(data);t.addfile(m,io.BytesIO(data))
  self.archive=buf.getvalue();self.s=DockerSnapshot(docker='unused',container='fixed',node='node',admission_module='admission.js',directory=self.tmp.name,task_manifest_hash=hashlib.sha256(self.task).hexdigest(),env={},deadline_ms=1000,clock=lambda:100)
  def fake(*args):
   if args[0]=='git':return b'abc\n' if 'rev-parse' in args else self.archive
   if args[0]=='cat':return self.task
   if args[0]=='/opt/bench/node':return b''
   raise AssertionError(args)
  self.s._docker=fake
 def tearDown(self):self.tmp.cleanup()
 def test_empty_authority_is_missing_admission_not_completion(self):
  d=self.s();self.assertFalse(d['admission']['ok']);self.assertEqual(d['artifact_digest'],hashlib.sha256(self.archive).hexdigest());self.assertIn('main.rs',d['files'])
 def test_changed_task_fails_closed(self):
  self.task=b'{"id":"changed"}'
  with self.assertRaisesRegex(RuntimeError,'manifest changed'):self.s()
 def test_expired_capture_never_launches(self):
  self.s.clock=lambda:1001
  with patch('docker_snapshot.subprocess.run') as run:
   with self.assertRaisesRegex(RuntimeError,'deadline elapsed'):self.s._run(['unused'])
   run.assert_not_called()
 def test_capture_timeout_fails_closed_and_remaining_clock(self):
  with patch('docker_snapshot.subprocess.run',side_effect=subprocess.TimeoutExpired('cmd',.9)) as run:
   with self.assertRaisesRegex(RuntimeError,'infrastructure timeout'):self.s._run(['unused'])
   self.assertEqual(run.call_args.kwargs['timeout'],.9)
 def test_git_runs_as_actor(self):
  del self.s._docker
  with patch.object(self.s,'_run',return_value=subprocess.CompletedProcess([],0,b'head',b'')) as run:
   self.s._docker('git','-C','/workspace','rev-parse','HEAD')
   self.assertEqual(run.call_args.args[0][:5],['unused','exec','-u','agent','fixed'])
 def test_archive_never_extracted(self):
  d=self.s();p=Path(self.tmp.name,d['stored_snapshot']);self.assertTrue((p/'artifact.tar').exists());self.assertFalse((Path(self.tmp.name)/'escape').exists());self.assertFalse((p/'main.rs').exists())

if __name__=='__main__':unittest.main()
