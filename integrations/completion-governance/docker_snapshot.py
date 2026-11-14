"""Freeze candidate source and protected authority evidence without running actor code.

Only fixed host configuration selects container, paths and verifier. The archive is
never extracted by this module. It is an audit candidate, not proof of integration.
"""
import hashlib
import io
import json
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path


def sha(data):
    return hashlib.sha256(data).hexdigest()


class DockerSnapshot:
    def __init__(self, *, docker, container, node, admission_module, directory, task_manifest_hash, env, deadline_ms, clock=None):
        self.docker, self.container, self.node = docker, container, node
        self.admission_module = str(Path(admission_module).resolve())
        self.directory = Path(directory)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.task_manifest_hash, self.env = task_manifest_hash, env
        self.deadline_ms = deadline_ms
        self.clock = clock or (lambda: int(time.time() * 1000))

    def _timeout(self):
        remaining = (self.deadline_ms - self.clock()) / 1000
        if remaining <= 0:
            raise RuntimeError('Completion snapshot incomplete: original deadline elapsed')
        return remaining

    def _run(self, args, **kwargs):
        # subprocess.run kills and reaps the child on timeout. Fixed docker exec
        # capture commands cannot start detached jobs. The outer host stops the
        # entire run container at the same deadline, including any server exec.
        try:
            return subprocess.run(args, env=self.env, capture_output=True,
                                  timeout=self._timeout(), **kwargs)
        except subprocess.TimeoutExpired as e:
            raise RuntimeError('Completion snapshot infrastructure timeout; no completion claim') from e

    def _docker(self, *args):
        uid = ['-u', 'agent'] if args and args[0] == 'git' else []
        p = self._run([self.docker, 'exec', *uid, self.container, *args])
        if p.returncode:
            raise RuntimeError('Trusted completion snapshot command failed: ' + p.stderr.decode(errors='replace')[-1000:])
        return p.stdout

    def __call__(self):
        # Git HEAD is immutable; later working-tree edits cannot alter this archive.
        head = self._docker('git', '-C', '/workspace', 'rev-parse', 'HEAD').decode().strip()
        archive = self._docker('git', '-C', '/workspace', 'archive', '--format=tar', head)
        task_bytes = self._docker('cat', '/authority/task.json')
        if sha(task_bytes) != self.task_manifest_hash:
            raise RuntimeError('Protected task manifest changed')
        audit_bytes = self._docker('/opt/bench/node', '-e', "const fs=require('fs');try{process.stdout.write(fs.readFileSync('/authority/state/audit.jsonl'));}catch(e){if(e.code!=='ENOENT')throw e;}")
        task = json.loads(task_bytes)
        audit = [json.loads(line) for line in audit_bytes.splitlines() if line.strip()]
        files = {}
        with tarfile.open(fileobj=io.BytesIO(archive), mode='r:') as tar:
            for member in tar:
                if member.isfile():
                    files[member.name] = sha(tar.extractfile(member).read())
        target = Path(tempfile.mkdtemp(prefix='snapshot-', dir=self.directory))
        (target / 'artifact.tar').write_bytes(archive)
        (target / 'protected-task.json').write_bytes(task_bytes)
        (target / 'protected-audit.jsonl').write_bytes(audit_bytes)
        candidates = []
        for row in audit:
            if row.get('type') != 'broker_response':
                continue
            result = row.get('result', {})
            admitted = result.get('admitted') or result.get('feedback', {}).get('verification')
            if isinstance(admitted, dict) and admitted.get('ok') and admitted.get('receipt'):
                candidates.append(admitted)
        admission = dict(ok=False, source_present=False, error='No current protected admission')
        if candidates:
            admitted = candidates[-1]
            script = '''import {pathToFileURL} from 'node:url';
const {inspectAdmission}=await import(pathToFileURL(process.argv[1]).href);
let s='';for await(const c of process.stdin)s+=c;
try{const x=JSON.parse(s);inspectAdmission(x);console.log(JSON.stringify({ok:true}));}
catch(e){console.log(JSON.stringify({ok:false,error:String(e.message)}));}'''
            p = self._run([self.node, '--input-type=module', '-e', script, self.admission_module],
                               input=json.dumps(dict(task=task, audit=audit, **admitted)).encode(),
)
            if p.returncode:
                raise RuntimeError('Protected admission verifier failed: ' + p.stderr.decode(errors='replace')[-1000:])
            admission = json.loads(p.stdout)
            admission['source_present'] = admitted['receipt']['source_hash'] in files.values()
            admission['receipt_source_hash'] = admitted['receipt']['source_hash']
            admission['generated_artifact_present'] = admitted['receipt']['artifact_hash'] in files.values()
        snapshot = dict(head=head, artifact_digest=sha(archive), audit_digest=sha(audit_bytes),
                        task_manifest_hash=sha(task_bytes), files=files, admission=admission,
                        source='Host-frozen immutable Git HEAD plus protected authority audit; actor code not executed',
                        stored_snapshot=target.name)
        (target / 'snapshot.json').write_text(json.dumps(snapshot, indent=2) + '\n')
        return snapshot
