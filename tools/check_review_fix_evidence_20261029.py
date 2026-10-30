#!/usr/bin/env python3
"""Read-only audit of the frozen review-fix evidence; never rewrites old hashes."""
import hashlib
import json
from pathlib import Path, PurePosixPath


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def relative(base, name):
    path = PurePosixPath(name)
    if path.is_absolute() or '..' in path.parts:
        raise ValueError('Invalid evidence path: '+name)
    return base.joinpath(*path.parts)


def main():
    root = Path(__file__).resolve().parents[1]
    directory = root/'docs/evaluations/review-fixes-20261029'
    index = json.loads((directory/'index.json').read_text())
    for name, expected in index['files_sha256'].items():
        if sha(relative(directory, name)) != expected:
            raise ValueError('Evidence changed: '+name)
    archive = index['baseline_archive']
    if sha(relative(root/'docs/evaluations', archive['path'])) != archive['sha256']:
        raise ValueError('Baseline archive changed')
    current = {str(p.relative_to(root)): sha(p) for p in sorted((root/'src').rglob('*.rs'))}
    for name in ['allocation-stable.json', 'allocation-msrv.json', 'cost.json']:
        data = json.loads((directory/name).read_text())
        if data.get('passed') is not True or data['source_sha256'] != current:
            raise ValueError(name+': source mismatch or incomplete evidence; make a new run, not new old hashes')
        for path, expected in data['manifests'].items():
            if sha(relative(root, path)) != expected:
                raise ValueError(name+': manifest changed: '+path)
        tool = 'tools/check_review_cost_20261029.py' if name == 'cost.json' else 'tools/check_review_resources_20261029.py'
        if sha(root/tool) != data['tool']['sha256']:
            raise ValueError(name+': producer tool changed')
        if name == 'cost.json':
            if sha(root/'tools/fixtures/review_git_cost_20261029.rs') != data['fixture']['sha256']:
                raise ValueError('Cost fixture changed')
            for pair in data['pairs']['git']:
                if pair['before']['signature'] != pair['after']['signature']:
                    raise ValueError('Different Git contents')
        else:
            for path, record in data['fixtures'].items():
                if sha(relative(root, path)) != record['sha256']:
                    raise ValueError('Allocation fixture changed: '+path)
            if len(data['cases']) != 4:
                raise ValueError('Missing allocation cases')
            for case in data['cases'].values():
                if case['exit'] != 0 or 'REVIEW_ALLOC ' not in case['stdout']+case['stderr']:
                    raise ValueError('Failed or empty allocation case')
    checks = json.loads((directory/'checks.json').read_text())
    expected = {'allocation-stable', 'allocation-msrv', 'cost', 'default', 'all-features', 'clippy', 'msrv', 'windows', 'linux', 'release'}
    if {c['name'] for c in checks} != expected or any(c['exit'] != 0 for c in checks):
        raise ValueError('Incomplete or failed checks')
    if index['functional_plan_complete'] or index['whole_application_memory_bound_proven'] or index['windows_linux_native_acceptance']:
        raise ValueError('Evidence scope incorrectly broadened')
    print(json.dumps(dict(passed=True, sources=len(current), artifacts=len(index['files_sha256']),
        whole_application_memory_bound_proven=False, windows_linux_native_acceptance=False)))


if __name__ == '__main__':
    main()
