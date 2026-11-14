import unittest,tempfile,json,sys,base64,shutil,hashlib
from unittest.mock import patch
from pathlib import Path
from replay import Ledger,TRUST,b64,un64
class ReplayTests(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name);self.ref=self.root/'ref.py';self.candidate=self.root/'candidate.py';self.program="import sys,pathlib; data=sys.stdin.buffer.read(); print('out');sys.stderr.write('err');pathlib.Path('result').write_bytes(data);sys.exit(2)"
  self.ref.write_text(self.program);self.candidate.write_text(self.program)
  self.ledger=Ledger({'directory':str(self.root/'ledger'),'process_timeout_seconds':1,'reference':{'command':[sys.executable,str(self.ref)]},'candidate':{'command':[sys.executable,str(self.candidate)]}})
 def tearDown(self):self.tmp.cleanup()
 def case(self,**kw):return self.ledger.observe({'stdin_b64':b64(b'\x00\xff'),'output_paths':['result'],'obligation_ids':['original-1'],**kw})
 def test_exact_bytes_nonzero_exit_and_file_replay(self):
  c=self.case();self.assertEqual(c['status'],'recorded');self.assertEqual(c['trust'],TRUST);r=self.ledger.replay([c['id']])['results'][0];self.assertEqual(r['status'],'match');self.assertEqual(r['actual']['exit_code'],2);self.assertEqual(un64(r['actual']['outputs'][0]['data_b64']),b'\x00\xff');self.assertTrue(self.ledger.summary()['cases'][0]['current_match'])
 def test_retains_old_pass_and_fails_prior_case_after_new_feature_mutation(self):
  c=self.case();self.ledger.replay([c['id']]);self.candidate.write_text(self.program.replace("print('out')","print('new')"));summary=self.ledger.summary()['cases'][0];self.assertFalse(summary['current_match']);self.assertEqual(summary['last_replay_status'],'match');r=self.ledger.replay([c['id']])['results'][0];self.assertEqual(r['status'],'mismatch');self.assertIn('stdout_b64',r['differences']);self.assertEqual(len(self.ledger.load(c['id'])['replays']),2)
 def test_reference_expected_bytes_cannot_be_supplied(self):
  c=self.case(expected={'stdout':'fiction'});self.assertEqual(c['status'],'rejected');self.assertNotIn('reference_runs',c)
 def test_socket_or_stateful_cases_explicitly_unsupported(self):
  for interaction in ['socket','stateful','interactive']:
   c=self.case(interaction=interaction);self.assertEqual(c['status'],'unsupported');self.assertEqual(self.ledger.replay([c['id']])['results'][0]['status'],'not_comparable')
 def test_nondeterminism_not_normalized_into_pass(self):
  self.ref.write_text('import uuid;print(uuid.uuid4())');c=self.case();self.assertEqual(c['status'],'unstable');self.assertEqual(len(c['reference_runs']),2)
 def test_timeout_preserves_partial_output_and_is_not_pass(self):
  self.ref.write_text("import time;print('partial',flush=True);time.sleep(5)");self.ledger.timeout=.2;c=self.case();self.assertEqual(c['status'],'incomplete');self.assertIn(b'partial',un64(c['reference_runs'][0]['stdout_b64']))
 def test_fresh_same_path_fixture_no_leaked_state(self):
  self.ref.write_text("from pathlib import Path; p=Path('count'); print(Path.cwd()); print(p.exists());p.write_text('1')");c=self.case(output_paths=['count']);self.assertEqual(c['status'],'recorded');self.assertEqual(c['reference_runs'][0]['stdout_b64'],c['reference_runs'][1]['stdout_b64'])
 def test_fixture_escape_and_output_link_rejected(self):
  self.assertEqual(self.case(files=[{'path':'../escape','data_b64':''}])['status'],'rejected')
  self.ref.write_text("from pathlib import Path;Path('link').symlink_to('/etc/passwd')");c=self.case(output_paths=['link']);self.assertEqual(c['status'],'incomplete');self.assertEqual(c['reference_runs'][0]['outputs'][0]['status'],'unsupported_symlink')
 def test_changed_reference_invalidates_current_match_without_erasing_history(self):
  c=self.case();self.ledger.replay([c['id']]);self.ref.write_text(self.program+'\n# changed');self.assertFalse(self.ledger.summary()['cases'][0]['current_match']);self.assertEqual(self.ledger.replay([c['id']])['results'][0]['status'],'reference_changed')
 def test_selected_output_mismatch_even_when_console_same(self):
  c=self.case();self.candidate.write_text(self.program.replace("write_bytes(data)","write_bytes(b'wrong')"));r=self.ledger.replay([c['id']])['results'][0];self.assertEqual(r['status'],'mismatch');self.assertEqual(list(r['differences']),['outputs'])
 def test_candidate_self_change_invalidates_apparent_match(self):
  c=self.case();self.candidate.write_text(self.program.replace('sys.exit(2)',"pathlib.Path(__file__).write_text('changed');sys.exit(2)"));r=self.ledger.replay([c['id']])['results'][0];self.assertEqual(r['status'],'incomplete');self.assertIn('changed during replay',r['reason'])
 def test_reference_can_be_recorded_before_candidate_exists(self):
  self.candidate.unlink();ledger=Ledger(self.ledger.config);c=ledger.observe({});self.assertEqual(c['status'],'recorded');r=ledger.replay([c['id']])['results'][0];self.assertEqual(r['status'],'incomplete');self.assertFalse(ledger.summary()['cases'][0]['current_match'])
 def test_execute_only_reference_uses_explicit_metadata_without_read_or_permission_change(self):
  binary=self.root/'oracle';shutil.copyfile('/bin/echo',binary);h=hashlib.sha256(binary.read_bytes()).hexdigest();binary.chmod(0o111)
  cfg={**self.ledger.config,'reference':{'command':[str(binary)],'artifact_metadata':{str(binary):{'sha256':h,'provenance':'host supplied pinned test artifact'}}}}
  original=Path.read_bytes
  def guarded(p):
   if p==binary:raise AssertionError('must not read execute-only oracle')
   return original(p)
  with patch.object(Path,'read_bytes',guarded):c=Ledger(cfg).observe({'argv':['sample']})
  self.assertEqual(c['status'],'recorded');self.assertEqual(c['reference_identity']['files'][0]['verification'],'provided_not_recomputed_by_actor');self.assertEqual(binary.stat().st_mode & 0o777,0o111)
 def test_interruption_preserves_first_reference_outcome_and_pending_not_pass(self):
  execute=self.ledger.execute;calls=0
  def interrupted(*a):
   nonlocal calls
   calls+=1
   if calls==2:raise KeyboardInterrupt()
   return execute(*a)
  with patch.object(self.ledger,'execute',interrupted):
   with self.assertRaises(KeyboardInterrupt):self.case()
  records=[json.loads(p.read_text()) for p in self.ledger.root.glob('*.json')];self.assertEqual(len(records),1);self.assertEqual(records[0]['status'],'pending');self.assertEqual(len(records[0]['reference_runs']),1);self.assertFalse(self.ledger.summary()['cases'][0]['current_match'])
 def test_interruption_preserves_completed_replay_and_next_pending(self):
  a=self.case();b=self.case();execute=self.ledger.execute;calls=0
  def interrupted(*args):
   nonlocal calls
   calls+=1
   if calls==2:raise KeyboardInterrupt()
   return execute(*args)
  with patch.object(self.ledger,'execute',interrupted):
   with self.assertRaises(KeyboardInterrupt):self.ledger.replay([a['id'],b['id']])
  self.assertEqual(self.ledger.load(a['id'])['replays'][-1]['status'],'match');self.assertEqual(self.ledger.load(b['id'])['replays'][-1]['status'],'pending');rows={r['id']:r for r in self.ledger.summary()['cases']};self.assertTrue(rows[a['id']]['current_match']);self.assertFalse(rows[b['id']]['current_match'])
 def test_candidate_cannot_pin_a_claimed_hash_over_changed_bytes(self):
  cfg={**self.ledger.config,'candidate':{**self.ledger.config['candidate'],'artifact_metadata':{str(self.candidate):{'sha256':'0'*64,'provenance':'claimed'}}}}
  with self.assertRaisesRegex(ValueError,'must be recomputed'):Ledger(cfg)
 def test_check_retains_mismatch_repair_and_regression_on_exact_case(self):
  self.candidate.write_text(self.program.replace("print('out')","print('wrong')"))
  request={'argv':['literal input'],'stdin_b64':b64(b'\x00\xff'),'files':[{'path':'fixture','data_b64':b64(b'exact fixture')}],'output_paths':['result'],'obligation_ids':['original-1']}
  c=self.ledger.check(request);self.assertEqual(c['status'],'recorded');self.assertEqual(c['replays'][0]['status'],'mismatch');self.assertEqual(c['request']['files'],request['files']);self.assertEqual(c['request']['argv'],request['argv']);first=c['replays'][0]
  self.candidate.write_text(self.program);self.assertFalse(self.ledger.summary()['cases'][0]['current_match']);self.ledger.replay([c['id']]);self.assertTrue(self.ledger.summary()['cases'][0]['current_match'])
  self.candidate.write_text(self.program.replace("print('out')","print('regressed')"));self.assertFalse(self.ledger.summary()['cases'][0]['current_match']);self.ledger.replay([c['id']]);saved=self.ledger.load(c['id']);self.assertEqual([r['status'] for r in saved['replays']],['mismatch','match','mismatch']);self.assertEqual(saved['replays'][0],first);self.assertEqual(saved['request'],c['request'])
 def test_check_absent_candidate_keeps_observation_and_incomplete_baseline(self):
  self.candidate.unlink();c=self.ledger.check({});self.assertEqual(c['status'],'recorded');self.assertEqual(c['replays'][0]['status'],'incomplete');self.assertFalse(self.ledger.summary()['cases'][0]['current_match'])
 def test_check_interruption_keeps_reference_and_pending_baseline(self):
  execute=self.ledger.execute
  def interrupted(spec,request):
   if spec is self.ledger.config['candidate']:raise KeyboardInterrupt()
   return execute(spec,request)
  with patch.object(self.ledger,'execute',interrupted):
   with self.assertRaises(KeyboardInterrupt):self.ledger.check({})
  records=[json.loads(p.read_text()) for p in self.ledger.root.glob('*.json')];self.assertEqual(len(records),1);c=records[0];self.assertEqual(c['status'],'recorded');self.assertEqual(len(c['reference_runs']),2);self.assertEqual(c['replays'][0]['status'],'pending');self.assertFalse(self.ledger.summary()['cases'][0]['current_match'])
 def test_check_noncomparable_reference_never_passes(self):
  c=self.ledger.check({'interaction':'socket'});self.assertEqual(c['status'],'unsupported');self.assertEqual(c['replays'][0]['status'],'not_comparable');self.assertFalse(self.ledger.summary()['cases'][0]['current_match'])
 def test_no_shell_interpolation(self):
  self.ref.write_text('import sys;print(sys.argv[1:])');c=self.case(argv=['; touch /tmp/not-a-command','$(whoami)']);self.assertEqual(c['status'],'recorded');self.assertIn(b'$(whoami)',un64(c['reference_runs'][0]['stdout_b64']))
if __name__=='__main__':unittest.main()
