#!/usr/bin/env python3
"""Link task-core admission evidence to current sources; does not complete M4 or thread-runtime accounting."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    folder = root/'docs/evaluations'
    source = {str(p.relative_to(root)): sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))}
    manifests = {n: sha((root/n).read_bytes()) for n in ['Cargo.toml', 'Cargo.lock']}
    reports, identities = {}, {}
    for suffix in ['allocation', 'allocation-msrv', 'cost', 'build', 'file-quota', 'workspace', 'ui-allocation', 'ui-cost']:
        path = folder/f'm4-task-resources-{suffix}.json'
        r = json.loads(path.read_text())
        assert r['source_sha256'] == source and r['manifests_sha256'] == manifests, suffix
        tool = ('check_sftp_executor.py' if suffix in ['file-quota', 'workspace'] else
                'check_ui_state.py' if suffix.startswith('ui-') else
                'check_task_build.py' if suffix == 'build' else 'check_task_resources.py')
        assert r['tool_sha256'] == sha((root/'tools'/tool).read_bytes()), suffix
        for key in ['fixtures', 'allocation_fixtures']:
            for name, item in r.get(key, {}).items():
                data = (root/name).read_bytes()
                assert item['sha256'] == sha(data) and item['source'] == data.decode('utf-8'), name
        reports[suffix] = r
        identities[str(path.relative_to(root))] = sha(path.read_bytes())
    for suffix in ['allocation', 'allocation-msrv']:
        r = reports[suffix]
        assert r['passed'] and r['sanity']['exit'] == r['measurement']['exit'] == 0
        samples = r['samples']
        assert [s['step'] for s in samples] == ['core', 'submitted', 'replaced_retained', 'invalidated_retained', 'core_dropped', 'released', 'limit', 'limit_released']
        assert [s['used'] for s in samples] == [304, 528, 752, 528, 224, 0, 9728, 0]
        assert [s['cores'] for s in samples] == [1, 1, 1, 1, 0, 0, 32, 0]
        assert all(s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] and s['reallocations'] == 0 for s in samples)
        assert samples[0]['live'] == 272 and samples[4]['live'] == 192
        assert r['layout'] == dict(budget=344, stats=296)
        assert 'TASK_FINAL denied_new_allocations=0 final=0' in r['measurement']['stderr']
    for suffix, metrics in [('cost', [('pending', 100), ('polling', 100), ('tokens', 100)]),
                            ('ui-cost', [('update', 5000), ('draw', 500)])]:
        c = reports[suffix]
        assert c['passed'] and len(c['runs']) == 10 and len(c['warmups']) == 4
        assert all(c['builds'][cohort]['exit'] == 0 for cohort in ['before', 'after'])
        assert len({r['signature'] for r in c['runs']+c['warmups']}) == 1
        assert all(sum(r['cohort'] == cohort for r in c['warmups']) == 2 for cohort in ['before', 'after'])
        for pair in range(5):
            assert sorted(r['cohort'] for r in c['runs'] if r['pair'] == pair) == ['after', 'before']
        for r in c['runs']+c['warmups']:
            assert r['exit'] == 0 and r['rss_bytes'] > 0
            for metric, count in metrics:
                assert len(r[metric]['samples_ns']) == count
                assert sum(r[metric]['samples_ns']) == r[metric]['total_ns']
    assert {r['signature'] for r in reports['cost']['runs']} == {200000}
    b = reports['build']
    assert b['passed'] and len(b['checks']) == 10 and all(r['exit'] == 0 for r in b['checks'])
    for index, counts in {1: [583], 2: [586, 18], 6: [10], 7: [18]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', b['checks'][index]['stdout'])] == counts
    for suffix in ['workspace', 'file-quota']:
        r = reports[suffix]
        assert r['sanity']['exit'] == r['measurement']['exit'] == 0
        assert all(s['live'] <= s['parent_used'] and s['peak'] <= s['parent_peak'] for s in r['samples'])
        assert all(r['samples'][-1][key] == 0 for key in ['live', 'parent_used', 'child_used'])
    assert reports['file-quota']['input']['io_headroom'] == 9*1024*1024
    assert reports['file-quota']['queue'] == dict(admitted=13, queued=11, reason='budget')
    assert reports['file-quota']['type_sizes']['budget'] == 344
    for key in ['local_picker', 'exit_view', 'retry', 'paths', 'lookup', 'review']:
        assert reports['workspace'][key]['paint_allocations'] == reports['workspace'][key]['paint_reallocations'] == 0
    ui = reports['ui-allocation']
    assert ui['passed'] and all(ui[key]['exit'] == 0 for key in ['sanity', 'cold', 'warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert ui['frames']['signature'] == reports['ui-cost']['runs'][0]['signature'] == 6821514814623131185
    archive = folder/'baselines/m4-task-resources-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline = json.load(tar.extractfile('source-identity.json'))
        members = {m.name: sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    for suffix in ['cost', 'ui-cost']:
        assert baseline['source_sha256'] == reports[suffix]['baseline_source_sha256']
        assert baseline['manifests_sha256'] == reports[suffix]['baseline_manifests_sha256'] == manifests
    assert baseline['source_sha256'] == json.loads((folder/'m4-link-resources-audit.json').read_text())['source_sha256']
    for name, digest in baseline['source_sha256'].items() | manifests.items():
        assert members[name] == digest
    assert {n: v for n, v in members.items() if n.startswith('assets/')} == {
        str(p.relative_to(root)): sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes = sorted(n for n in source.keys() | baseline['source_sha256'].keys() if source.get(n) != baseline['source_sha256'].get(n))
    assert changes == ['src/cancellation.rs', 'src/main.rs', 'src/resources.rs', 'src/resources/budget.rs', 'src/ssh/helper_resources.rs', 'src/tasks.rs', 'src/workspace/files.rs', 'src/workspace/remote.rs']
    first_path = folder/'m4-task-resources-workspace-first-attempt.json'
    first = json.loads(first_path.read_text())
    assert first['source_sha256'] == source and first['measurement']['exit'] != 0
    assert 'assertion `left == right` failed' in first['measurement']['stderr']
    result = dict(passed=True, goal_complete=False,
                  scope='LatestTask per-budget32 owned cores and paid cancellation state/lazy sync headers; atomic publication, checked version, outside-lock destruction and owner lifetime. Not all background thread counts, runtime/TLS/stack/join/default tokens/dynamic generic buffers/config/native/global/long-run completion.',
                  source_sha256=source, manifests_sha256=manifests, changed_sources=changes,
                  reports_sha256=identities, audit_tool_sha256=sha(Path(__file__).read_bytes()),
                  unresolved_observations=dict(workspace_all_thread_draw_first_attempt=dict(path=str(first_path.relative_to(root)), sha256=sha(first_path.read_bytes()), note='Two allocations in first path-modal all-thread draw interval; source not attributed. Independent same-source repeat passed without changing meter or fixture. Does not prove universal zero allocation.')),
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=sha(archive.read_bytes())),
                  cost_summary=reports['cost']['summary'], ui_cost_summary=reports['ui-cost']['summary'])
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__':
    main()
