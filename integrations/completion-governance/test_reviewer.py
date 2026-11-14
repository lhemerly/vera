import json
import tempfile
import unittest
from pathlib import Path
from reviewer import Reviewer

class GovernanceTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.now=100
        self.snap=dict(artifact_digest='a'*64,audit_digest='b'*64,files={'core.vera':'c','host.rs':'d'},admission=dict(ok=True,source_present=True))
        self.r=Reviewer(run_id='run',task_hash='t',deadline_ms=1000,directory=self.tmp.name,snapshot=lambda:self.snap,clock=lambda:self.now)
        self.p=dict(status='ready',remaining_obligation_ids=[],core_artifact_paths=['core.vera','host.rs'],adapter_artifact_paths=['host.rs'],test_evidence=[dict(command='./test.sh',result='passed')],blocker=dict(kind='none',detail=''))
    def tearDown(self):self.tmp.cleanup()
    def test_missing_proposal_continues(self):
        r=self.r.review({'final_text':'done'});self.assertEqual(r['decision'],'continue');self.assertEqual(r['missing'][0]['id'],'completion-report')
    def test_missing_adapter_and_partial_continue(self):
        self.p.update(status='partial',adapter_artifact_paths=[],remaining_obligation_ids=['command-a'])
        r=self.r.review({'proposal':self.p});self.assertEqual(r['decision'],'continue');self.assertTrue({'adapter','command-a','partial-task'} <= {x['id'] for x in r['missing']})
    def test_no_forged_admission(self):
        self.snap['admission']=dict(ok=False,error='pending petition');r=self.r.review({'proposal':self.p,'admission':{'ok':True}});self.assertEqual(r['decision'],'continue')
    def test_ready_is_candidate_not_semantic_acceptance(self):
        r=self.r.review({'proposal':self.p});self.assertEqual(r['decision'],'ready_for_audit');self.assertEqual(r['completion_scope'],'accepted_stop_not_task_correctness');self.assertIn('unverified',r['summary']);self.assertTrue(Path(self.tmp.name,'review-0001.json').exists())
    def test_original_deadline_not_request(self):
        self.now=1001;self.r.snapshot=lambda:(_ for _ in ()).throw(AssertionError('Must not capture after deadline'));r=self.r.review({'proposal':self.p,'deadline_ms':999999});self.assertEqual(r['decision'],'time_expired');self.assertEqual(r['deadline_ms'],1000)
    def test_declared_blocker_not_truth_claim(self):
        self.p.update(status='blocked',remaining_obligation_ids=['adapter'],blocker=dict(kind='stalled',detail='No new justified hypothesis after contrasting observations'))
        r=self.r.review({'proposal':self.p});self.assertEqual(r['decision'],'blocked');self.assertIn('NOT verified',r['summary'])
    def test_artifact_paths_must_exist(self):
        self.p['adapter_artifact_paths']=['../../pretend'];r=self.r.review({'proposal':self.p});self.assertEqual(r['decision'],'continue')
    def test_bad_snapshot_fails_closed(self):
        self.snap.clear()
        with self.assertRaises(RuntimeError):self.r.review({'proposal':self.p})
    def test_cancel_stops_without_claim(self):
        self.r.cancelled=lambda:True;self.assertEqual(self.r.review({})['decision'],'cancelled')

if __name__=='__main__':unittest.main()
