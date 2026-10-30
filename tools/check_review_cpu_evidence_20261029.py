#!/usr/bin/env python3
"""Read-only source, baseline, admission, ownership and cost evidence audit."""
import hashlib
import json
import re
import tarfile
from pathlib import Path
from check_review_fix_evidence_20261029 import relative,sha


def main():
    root=Path(__file__).resolve().parents[1]
    directory=root/'docs/evaluations/review-cpu-20261029'
    index=json.loads((directory/'index.json').read_text())
    for name,expected in index['files_sha256'].items():
        if sha(relative(directory,name))!=expected:raise ValueError('Evidence changed: '+name)
    for name,expected in index['tools_sha256'].items():
        if sha(relative(root,name))!=expected:raise ValueError('Producer/fixture changed: '+name)
    archive=relative(root/'docs/evaluations',index['baseline_archive']['path'])
    if sha(archive)!=index['baseline_archive']['sha256']:raise ValueError('Baseline archive changed')
    with tarfile.open(archive) as frozen:
        baseline={m.name:hashlib.sha256(frozen.extractfile(m).read()).hexdigest()
            for m in frozen.getmembers() if m.isfile() and m.name.startswith('src/') and m.name.endswith('.rs')}
        baseline_manifests={n:hashlib.sha256(frozen.extractfile(n).read()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    current={str(p.relative_to(root)):sha(p) for p in sorted((root/'src').rglob('*.rs'))}
    expected_cases={'monitor::native::cpu_meter::native_reads','monitor::native::cpu_meter::bounded_linux_inputs','monitor::review_meter::sampler'}
    for name in ['allocation-stable.json','allocation-msrv.json','cost.json']:
        data=json.loads((directory/name).read_text())
        if data.get('passed') is not True or data['source_sha256']!=current:
            raise ValueError(name+': source mismatch or incomplete; rerun, never replace old hashes')
        for path,expected in data['manifests'].items():
            if sha(relative(root,path))!=expected:raise ValueError('Manifest changed: '+path)
        producer='tools/check_review_cpu_cost_20261029.py' if name=='cost.json' else 'tools/check_review_cpu_20261029.py'
        if sha(root/producer)!=data['tool']['sha256']:raise ValueError('Producer changed')
        if name=='cost.json':
            if data['baseline_source_sha256']!=baseline or data['baseline_manifests']!=baseline_manifests:
                raise ValueError('Wrong baseline')
            if data['manifests']['Cargo.lock']!=baseline_manifests['Cargo.lock']:
                raise ValueError('Locked package set unexpectedly changed')
            if sha(root/'tools/fixtures/review_cpu_cost_20261029.rs')!=data['fixture']['sha256']:
                raise ValueError('Cost fixture changed')
            if set(data['pairs'])!={'native','monitor'} or any(len(p)!=5 for p in data['pairs'].values()):
                raise ValueError('Missing cost pairs')
            for pair in data['pairs']['native']:
                if any(pair['before'][k]!=pair['after'][k] for k in ['cores','total']):
                    raise ValueError('Different host metric scope')
            probe=data['legacy_port_probe']
            if probe['exit'] or 'LEGACY_PORTS constructions=32 retained_send_refs=32' not in probe['stdout']+probe['stderr']:
                raise ValueError('Missing pinned-library diagnostic')
        else:
            if set(data['cases'])!=expected_cases:raise ValueError('Missing allocation cases')
            for path,record in data['fixtures'].items():
                if sha(relative(root,path))!=record['sha256']:raise ValueError('Allocation fixture changed')
            for case in data['cases'].values():
                text=case['stdout']+case['stderr']
                if case['exit'] or '1 passed; 0 failed' not in text or not any(m in text for m in ['CPU_ALLOC ','REVIEW_ALLOC ']):
                    raise ValueError('Failed or empty allocation case')
    checks=json.loads((directory/'checks.json').read_text())
    expected={'default','all-features','mach-refs','clippy','msrv','windows','linux','allocation-stable','allocation-msrv','cost','release'}
    if {c['name'] for c in checks}!=expected or any(c['exit'] for c in checks):raise ValueError('Incomplete checks')
    for c in checks:
        text=relative(directory,c['stored_log']).read_text()
        if c['name'] in ['default','all-features','mach-refs'] and not re.search(r'test result: ok\. \d+ passed; 0 failed;',text):
            raise ValueError('Missing native test summary')
        if c['name']=='mach-refs' and '1 passed; 0 failed' not in text:raise ValueError('Empty Mach ownership test')
    for name,expected in index['previous_indices_sha256'].items():
        old=root/'docs/evaluations'/name
        if sha(old/'index.json')!=expected:raise ValueError('Previous index changed')
        old_index=json.loads((old/'index.json').read_text())
        for path,expected in old_index['files_sha256'].items():
            if sha(relative(old,path))!=expected:raise ValueError('Previous evidence changed: '+path)
        baseline=old_index['baseline_archive']
        if sha(relative(root/'docs/evaluations',baseline['path']))!=baseline['sha256']:raise ValueError('Previous archive changed')
    if any(index[k] for k in ['functional_plan_complete','whole_application_memory_bound_proven','windows_linux_native_acceptance']):
        raise ValueError('Scope incorrectly broadened')
    print(json.dumps(dict(passed=True,sources=len(current),artifacts=len(index['files_sha256']),previous_evidence_preserved=True,
        whole_application_memory_bound_proven=False,windows_linux_native_acceptance=False)))


if __name__=='__main__':main()
