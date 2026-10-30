#!/usr/bin/env python3
"""Read-only audit of current metadata evidence and preserved previous evidence."""
import hashlib
import json
import re
import tarfile
from pathlib import Path
from check_review_fix_evidence_20261029 import relative, sha


def main():
    root=Path(__file__).resolve().parents[1]
    directory=root/'docs/evaluations/review-metadata-20261029'
    index=json.loads((directory/'index.json').read_text())
    for name,expected in index['files_sha256'].items():
        if sha(relative(directory,name))!=expected:
            raise ValueError('Evidence changed: '+name)
    for name,expected in index['tools_sha256'].items():
        if sha(relative(root,name))!=expected:
            raise ValueError('Tool changed: '+name)
    archive=relative(root/'docs/evaluations',index['baseline_archive']['path'])
    if sha(archive)!=index['baseline_archive']['sha256']:
        raise ValueError('Baseline archive changed')
    with tarfile.open(archive) as frozen:
        baseline={m.name:hashlib.sha256(frozen.extractfile(m).read()).hexdigest()
            for m in frozen.getmembers() if m.isfile() and m.name.startswith('src/') and m.name.endswith('.rs')}
    current={str(p.relative_to(root)):sha(p) for p in sorted((root/'src').rglob('*.rs'))}
    for name in ['allocation-stable.json','allocation-msrv.json','cost.json','metadata-cost.json']:
        data=json.loads((directory/name).read_text())
        if data.get('passed') is not True or data['source_sha256']!=current:
            raise ValueError(name+': failed, incomplete, or source mismatch; rerun without rewriting old hashes')
        producer = ('tools/check_review_metadata_20261029.py' if name.startswith('allocation-') else
            'tools/check_review_metadata_cost_20261029.py' if name == 'metadata-cost.json' else
            'tools/check_review_cost_20261029.py')
        if sha(root/producer) != data['tool']['sha256']:
            raise ValueError(name+': producer changed')
        for path,expected in data['manifests'].items():
            if sha(relative(root,path))!=expected:
                raise ValueError('Manifest changed: '+path)
        if name.startswith('allocation-'):
            for path,record in data['fixtures'].items():
                if sha(relative(root,path))!=record['sha256']:
                    raise ValueError('Allocation fixture changed: '+path)
            expected={'git::metadata_meter::requests','git::metadata_meter::errors','monitor::metadata_meter::network'}
            if set(data['cases'])!=expected:
                raise ValueError('Missing allocation case')
            for case in data['cases'].values():
                text=case['stdout']+case['stderr']
                if case['exit']!=0 or 'METADATA_ALLOC ' not in text or '1 passed; 0 failed' not in text:
                    raise ValueError('Failed or empty allocation case')
        else:
            fixture = 'tools/fixtures/review_metadata_cost_20261029.rs' if name == 'metadata-cost.json' else 'tools/fixtures/review_git_cost_20261029.rs'
            if sha(root/fixture) != data['fixture']['sha256']:
                raise ValueError(name+': cost fixture changed')
            if data['baseline_source_sha256']!=baseline:
                raise ValueError('Cost baseline does not match frozen archive')
            if len(data['pairs'] if name=='metadata-cost.json' else data['pairs']['git'])!=5:
                raise ValueError('Missing cost trials')
            if name=='cost.json':
                for pair in data['pairs']['git']:
                    if pair['before']['signature']!=pair['after']['signature']:
                        raise ValueError('Different Git contents')
    checks=json.loads((directory/'checks.json').read_text())
    expected={'allocation-stable','allocation-msrv','cost','metadata-cost','default','all-features','clippy','msrv','windows','linux','release'}
    if {c['name'] for c in checks}!=expected or any(c['exit']!=0 for c in checks):
        raise ValueError('Incomplete or failed checks')
    for check in checks:
        text=relative(directory,check['stored_log']).read_text()
        if check['name'] in ['default','all-features'] and not re.search(r'test result: ok\. \d+ passed; 0 failed;',text):
            raise ValueError('No passing native test summary')
    old=root/'docs/evaluations/review-fixes-20261029'
    if sha(old/'index.json')!=index['previous_index_sha256']:
        raise ValueError('Previous evidence index changed')
    for name,expected in json.loads((old/'index.json').read_text())['files_sha256'].items():
        if sha(relative(old,name))!=expected:
            raise ValueError('Previous frozen evidence changed: '+name)
    old_archive=json.loads((old/'index.json').read_text())['baseline_archive']
    if sha(relative(root/'docs/evaluations',old_archive['path'])) != old_archive['sha256']:
        raise ValueError('Previous baseline archive changed')
    if any(index[k] for k in ['functional_plan_complete','whole_application_memory_bound_proven','windows_linux_native_acceptance']):
        raise ValueError('Evidence scope incorrectly broadened')
    print(json.dumps(dict(passed=True,sources=len(current),artifacts=len(index['files_sha256']),
        previous_evidence_preserved=True,whole_application_memory_bound_proven=False,windows_linux_native_acceptance=False)))


if __name__=='__main__':
    main()
