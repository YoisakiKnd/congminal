#!/usr/bin/env python3
"""Audit final monitor scope against source, frozen baselines, tests and costs."""
import hashlib,json,re,tarfile
from pathlib import Path
from check_review_fix_evidence_20261029 import relative,sha


def main():
    root=Path(__file__).resolve().parents[1];directory=root/'docs/evaluations/review-monitor-bounds-20261030'
    index=json.loads((directory/'index.json').read_text())
    for n,h in index['files_sha256'].items():
        if sha(relative(directory,n))!=h:raise ValueError('Evidence changed: '+n)
    for n,h in index['tools_sha256'].items():
        if sha(relative(root,n))!=h:raise ValueError('Producer changed: '+n)
    archive=relative(root/'docs/evaluations',index['baseline_archive']['path'])
    if sha(archive)!=index['baseline_archive']['sha256']:raise ValueError('Baseline archive changed')
    with tarfile.open(archive) as frozen:
        baseline={m.name:hashlib.sha256(frozen.extractfile(m).read()).hexdigest() for m in frozen.getmembers() if m.isfile() and m.name.startswith('src/') and m.name.endswith('.rs')}
        baseline_manifests={n:hashlib.sha256(frozen.extractfile(n).read()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
        support={m.name:hashlib.sha256(frozen.extractfile(m).read()).hexdigest() for m in frozen.getmembers() if m.isfile() and (m.name.startswith('assets/') or (m.name.startswith('src/') and not m.name.endswith('.rs')) or m.name=='docs/configuration.md')}
    for n,h in support.items():
        if sha(root/n)!=h:raise ValueError('Compiled data changed from baseline: '+n)
    current={str(p.relative_to(root)):sha(p) for p in sorted((root/'src').rglob('*.rs'))}
    cases={'monitor::disk::bounds_meter::maximum_table','monitor::disk::bounds_meter::native_volume','monitor::review_meter::sampler'}
    for name in ['allocation-stable.json','allocation-msrv.json','cost.json']:
        data=json.loads((directory/name).read_text())
        if data.get('passed') is not True or data['source_sha256']!=current:raise ValueError(name+': incomplete or source mismatch; rerun without rewriting old hashes')
        for n,h in data['manifests'].items():
            if sha(root/n)!=h:raise ValueError('Manifest changed: '+n)
        producer='tools/check_review_monitor_bounds_cost_20261030.py' if name=='cost.json' else 'tools/check_review_monitor_bounds_20261030.py'
        if sha(root/producer)!=data['tool']['sha256']:raise ValueError('Producer changed')
        if name=='cost.json':
            if data['baseline_source_sha256']!=baseline or data['baseline_manifests']!=baseline_manifests:raise ValueError('Wrong cost baseline')
            if sha(root/'tools/fixtures/review_monitor_bounds_cost_20261030.rs')!=data['fixture']['sha256']:raise ValueError('Cost fixture changed')
            if len(data['pairs'])!=5 or len(data['warmups'])!=2:raise ValueError('Missing cost pairs')
            for pair in data['pairs']:
                if any(pair[n]['exit'] for n in ['before','after']):raise ValueError('Failed cost trial')
            if any(data['shapes']['before'][k]!=data['shapes']['after'][k] for k in ['cores','memory']):raise ValueError('Different host metric scope')
            audit=json.loads((directory/'semantics.json').read_text())
            if audit['cost_sha256']!=sha(directory/name):raise ValueError('Stale semantics audit')
            shapes=data['shapes'];labels=[]
            for n in ['before','after']:
                match=re.search(r'label=(.*?) processes=',shapes[n]['run']['stdout']+shapes[n]['run']['stderr'])
                if not match:raise ValueError('Missing volume label')
                labels.append(match[1])
            if not audit['same_volume_this_host'] or labels[0]!=labels[1] or shapes['before']['disk']!=shapes['after']['disk']:raise ValueError('Incorrect same-volume claim')
        else:
            if set(data['cases'])!=cases:raise ValueError('Missing allocation cases')
            for n,record in data['fixtures'].items():
                if sha(root/n)!=record['sha256']:raise ValueError('Allocation fixture changed')
            for case in data['cases'].values():
                text=case['stdout']+case['stderr']
                if case['exit'] or '1 passed; 0 failed' not in text or not any(m in text for m in ['BOUNDS_ALLOC ','REVIEW_ALLOC ']):raise ValueError('Failed or empty allocation case')
    checks=json.loads((directory/'checks.json').read_text())
    expected={'default','all-features','disk-native','mach-refs','clippy','msrv','windows','linux','allocation-stable','allocation-msrv','cost','release'}
    if {c['name'] for c in checks}!=expected or any(c['exit'] for c in checks):raise ValueError('Incomplete command checks')
    for c in checks:
        text=relative(directory,c['stored_log']).read_text()
        if c['name'] in ['default','all-features'] and not re.search(r'test result: ok\. \d+ passed; 0 failed;',text):raise ValueError('Missing passing suite')
        if c['name'] in ['disk-native','mach-refs'] and '1 passed; 0 failed' not in text:raise ValueError('Empty native acceptance')
        if c['name']=='disk-native' and 'DISK_NATIVE ' not in text:raise ValueError('Missing real volume evidence')
    deps=json.loads((directory/'dependencies.json').read_text())
    if deps['exit'] or not deps['retained_unchanged'] or deps['current_lock_sha256']!=sha(root/'Cargo.lock') or deps['before_lock_sha256']!=baseline_manifests['Cargo.lock']:raise ValueError('Dependency evidence mismatch')
    if 'name = "sysinfo"' in (root/'Cargo.lock').read_text() or any('sysinfo::' in p.read_text() for p in (root/'src').rglob('*.rs')):raise ValueError('Foreign table still in production')
    for name,h in index['previous_indices_sha256'].items():
        old=root/'docs/evaluations'/name
        if sha(old/'index.json')!=h:raise ValueError('Previous index changed')
        old_index=json.loads((old/'index.json').read_text())
        for p,h in old_index['files_sha256'].items():
            if sha(relative(old,p))!=h:raise ValueError('Previous evidence changed: '+p)
        a=old_index['baseline_archive']
        if sha(relative(root/'docs/evaluations',a['path']))!=a['sha256']:raise ValueError('Previous baseline changed')
    if any(index[k] for k in ['functional_plan_complete','whole_application_memory_bound_proven','native_malloc_hard_bound_proven','windows_linux_native_acceptance']):raise ValueError('Evidence scope broadened')
    print(json.dumps(dict(passed=True,sources=len(current),artifacts=len(index['files_sha256']),support_files=len(support),previous_evidence_preserved=True,whole_application_memory_bound_proven=False,windows_linux_native_acceptance=False)))


if __name__=='__main__':main()
