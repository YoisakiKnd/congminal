#!/usr/bin/env python3
"""核对通知记录资源阶段的来源、分配、语义及相邻组件证据。"""
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
    folder = root / 'docs/evaluations'
    source = {str(p.relative_to(root)): sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))}
    manifests = {n: sha((root/n).read_bytes()) for n in ['Cargo.toml', 'Cargo.lock']}
    reports, identities = {}, {}
    for suffix in ['allocation', 'allocation-msrv', 'cost', 'build', 'file-quota', 'workspace',
                   'ui-allocation', 'ui-cost', 'layout']:
        path = folder / f'm4-notice-resources-{suffix}.json'
        r = json.loads(path.read_text())
        assert r['source_sha256'] == source and r['manifests_sha256'] == manifests, suffix
        if 'tool_sha256' in r:
            tool = ('check_sftp_executor.py' if suffix in ['file-quota', 'workspace'] else
                    'check_ui_state.py' if suffix.startswith('ui-') else
                    'check_notice_layout.py' if suffix == 'layout' else 'check_notice_resources.py')
            assert r['tool_sha256'] == sha((root/'tools'/tool).read_bytes()), suffix
        for key in ['fixtures', 'allocation_fixtures']:
            for name, item in r.get(key, {}).items():
                data = (root/name).read_bytes()
                assert item['sha256'] == sha(data) and item['source'] == data.decode('utf-8')
        reports[suffix] = r
        identities[str(path.relative_to(root))] = sha(path.read_bytes())
    for suffix in ['allocation', 'allocation-msrv']:
        r = reports[suffix]
        assert r['passed'] and r['sanity']['exit'] == r['measurement']['exit'] == 0
        samples = r['samples']
        assert [s['step'] for s in samples] == ['first', 'full', 'denied', 'recovered', 'cleared', 'refilled', 'released']
        size = 128*r['item_size']
        assert r['item_size'] == 48
        assert [s['live'] for s in samples] == [size, size, size, 2*size, 0, size, 0]
        assert all(s['live'] == s['used'] and s['peak'] <= s['budget_peak'] and s['reallocations'] == 0 for s in samples)
        assert [s['allocations'] for s in samples] == [1, 1, 1, 2, 2, 3, 3]
        assert 'NOTICE_FINAL live=0 used=0 hot_new_allocations=0 denied_new_allocations=0 slots=128' in r['measurement']['stderr']
    for suffix, metrics in [('cost', [('first', 5000), ('hot', 200), ('describe', 200), ('refill', 5000), ('muted', 200)]),
                            ('ui-cost', [('update', 5000), ('draw', 500)])]:
        c = reports[suffix]
        assert c['passed'] and len(c['runs']) == 10 and len(c['warmups']) == 4
        assert len({r['signature'] for r in c['runs']+c['warmups']}) == 1
        assert all(c['builds'][cohort]['exit'] == 0 for cohort in ['before', 'after'])
        assert all(sum(r['cohort'] == cohort for r in c['warmups']) == 2 for cohort in ['before', 'after'])
        for pair in range(5):
            assert sorted(r['cohort'] for r in c['runs'] if r['pair'] == pair) == ['after', 'before']
        for r in c['runs']+c['warmups']:
            assert r['exit'] == 0 and r['rss_bytes'] > 0
            for metric, count in metrics:
                assert len(r[metric]['samples_ns']) == count
                assert sum(r[metric]['samples_ns']) == r[metric]['total_ns']
    b = reports['build']
    assert b['passed'] and len(b['checks']) == 9 and all(r['exit'] == 0 for r in b['checks'])
    for index, counts in {1: [556], 2: [559, 18], 6: [14]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', b['checks'][index]['stdout'])] == counts
    for suffix in ['workspace', 'file-quota']:
        r = reports[suffix]
        assert r['sanity']['exit'] == r['measurement']['exit'] == 0
        assert all(s['live'] <= s['parent_used'] for s in r['samples'])
        assert all(r['samples'][-1][key] == 0 for key in ['live', 'parent_used', 'child_used'])
    assert reports['file-quota']['input']['io_headroom'] == 9*1024*1024
    assert reports['file-quota']['queue'] == dict(admitted=13, queued=11, reason='budget')
    assert 'paint_allocations=0 paint_reallocations=0' in reports['workspace']['measurement']['stderr']
    ui = reports['ui-allocation']
    assert ui['passed'] and all(ui[key]['exit'] == 0 for key in ['sanity', 'cold', 'warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert len(ui['frames']['update']['samples_ns']) == 5000 and len(ui['frames']['draw']['samples_ns']) == 500
    assert ui['frames']['signature'] == reports['ui-cost']['runs'][0]['signature'] == 6821514814623131185
    layout = reports['layout']
    assert layout['passed'] and len(layout['checks']) == 2
    assert [(r['mode'], r['item'], r['center'], r['budget']) for r in layout['checks']] == [
        ('default', 32, 120, 304), ('preview', 48, 120, 304)]
    assert all(r['exit'] == r['build']['exit'] == 0 for r in layout['checks'])
    archive = folder/'baselines/m4-notice-resources-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline = json.load(tar.extractfile('source-identity.json'))
        members = {m.name: sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    for suffix in ['cost', 'ui-cost']:
        assert baseline['source_sha256'] == reports[suffix]['baseline_source_sha256']
        assert baseline['manifests_sha256'] == reports[suffix]['baseline_manifests_sha256'] == manifests
    assert baseline['source_sha256'] == json.loads((folder/'m4-search-resources-audit.json').read_text())['source_sha256']
    for name, digest in baseline['source_sha256'].items() | manifests.items():
        assert members[name] == digest
    assert {n: v for n, v in members.items() if n.startswith('assets/')} == {
        str(p.relative_to(root)): sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes = sorted(n for n in source.keys() | baseline['source_sha256'].keys() if source.get(n) != baseline['source_sha256'].get(n))
    assert changes == ['src/main.rs', 'src/notifications.rs', 'src/resources/budget.rs']
    result = dict(passed=True, goal_complete=False,
                  scope='Center owned fixed notice record storage only; same-work semantic/allocation/cost, adjacent UI/SFTP and native release type layout. System text/OS commands/gates/tasks, sidebar strings, root metadata, native delivery/latency/platform/long-run remain outside completion.',
                  source_sha256=source, manifests_sha256=manifests, changed_sources=changes,
                  reports_sha256=identities, audit_tool_sha256=sha(Path(__file__).read_bytes()),
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=sha(archive.read_bytes())),
                  cost_summary=reports['cost']['summary'], ui_cost_summary=reports['ui-cost']['summary'])
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__':
    main()
