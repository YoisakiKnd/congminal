#!/usr/bin/env python3
"""核对 Shell 记录/目录阶段的来源、分配和同负载证据，不完成全计划。"""
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
        path = folder/f'm4-shell-resources-{suffix}.json'
        r = json.loads(path.read_text())
        assert r['source_sha256'] == source and r['manifests_sha256'] == manifests, suffix
        tool = ('check_sftp_executor.py' if suffix in ['file-quota', 'workspace'] else
                'check_ui_state.py' if suffix.startswith('ui-') else
                'check_shell_build.py' if suffix == 'build' else 'check_shell_resources.py')
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
        assert [s['step'] for s in samples] == ['first', 'full', 'warm', 'directory', 'replaced', 'denied', 'recovered', 'retained', 'released']
        assert r['type_sizes'] == dict(command=88, event=32)
        assert [s['live'] for s in samples] == [480, 92160, 92160, 95225, 95225, 95225, 95705, 92640, 0]
        assert all(s['live'] == s['used'] and s['peak'] <= s['budget_peak'] for s in samples)
        assert [s['allocations'] for s in samples] == [2, 14, 14, 16, 18, 18, 20, 20, 20]
        assert [s['reallocations'] for s in samples] == [0]+[12]*8
        assert samples[-1]['peak'] == 137216
        assert [s['commands'] for s in r['growth']] == [2, 4, 8, 16, 32, 64, 128, 256, 512, 1024]
        assert all(s['live'] <= s['peak'] <= s['budget_peak'] for s in r['growth'])
        assert 'SHELL_FINAL live=0 used=0 warm_new_allocations=0 denied_new_allocations=0' in r['measurement']['stderr']
    for suffix, metrics in [('cost', [('first', 5000), ('warm', 200), ('parsing', 200), ('directories', 5000)]),
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
    assert all(r['first_retained'] == 480 and r['retained'] == 182409 for r in reports['cost']['runs'])
    b = reports['build']
    assert b['passed'] and len(b['checks']) == 10 and all(r['exit'] == 0 for r in b['checks'])
    for index, counts in {1: [560], 2: [563, 18], 6: [10], 7: [1]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', b['checks'][index]['stdout'])] == counts
    for suffix in ['workspace', 'file-quota']:
        r = reports[suffix]
        assert r['sanity']['exit'] == r['measurement']['exit'] == 0
        assert all(s['live'] <= s['parent_used'] for s in r['samples'])
        assert all(r['samples'][-1][key] == 0 for key in ['live', 'parent_used', 'child_used'])
    assert reports['file-quota']['input']['io_headroom'] == 9*1024*1024
    assert reports['file-quota']['queue'] == dict(admitted=13, queued=11, reason='budget')
    assert reports['file-quota']['type_sizes']['budget'] == 312
    for key in ['local_picker', 'exit_view', 'retry', 'paths', 'lookup', 'review']:
        assert reports['workspace'][key]['paint_allocations'] == reports['workspace'][key]['paint_reallocations'] == 0
    ui = reports['ui-allocation']
    assert ui['passed'] and all(ui[key]['exit'] == 0 for key in ['sanity', 'cold', 'warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert len(ui['frames']['update']['samples_ns']) == 5000 and len(ui['frames']['draw']['samples_ns']) == 500
    assert ui['frames']['signature'] == reports['ui-cost']['runs'][0]['signature'] == 6821514814623131185
    archive = folder/'baselines/m4-shell-resources-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline = json.load(tar.extractfile('source-identity.json'))
        members = {m.name: sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    for suffix in ['cost', 'ui-cost']:
        assert baseline['source_sha256'] == reports[suffix]['baseline_source_sha256']
        assert baseline['manifests_sha256'] == reports[suffix]['baseline_manifests_sha256'] == manifests
    assert baseline['source_sha256'] == json.loads((folder/'m4-notice-resources-audit.json').read_text())['source_sha256']
    for name, digest in baseline['source_sha256'].items() | manifests.items():
        assert members[name] == digest
    assert {n: v for n, v in members.items() if n.startswith('assets/')} == {
        str(p.relative_to(root)): sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes = sorted(n for n in source.keys() | baseline['source_sha256'].keys() if source.get(n) != baseline['source_sha256'].get(n))
    assert changes == ['src/main.rs', 'src/resources/budget.rs', 'src/search.rs', 'src/session.rs', 'src/shell.rs', 'src/term/mod.rs', 'src/ui_state.rs', 'src/workspace.rs']
    result = dict(passed=True, goal_complete=False,
                  scope='Shell owned command/event buffers and directory host/path, trusted two-slice UI match batch; lifecycle/denial/semantic/cost and adjacent UI/SFTP. CopyOutput/clipboard, parser own storage, links/config/platform/task/root metadata, native latency/platform/long-run remain outside completion.',
                  source_sha256=source, manifests_sha256=manifests, changed_sources=changes,
                  reports_sha256=identities, audit_tool_sha256=sha(Path(__file__).read_bytes()),
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=sha(archive.read_bytes())),
                  cost_summary=reports['cost']['summary'], ui_cost_summary=reports['ui-cost']['summary'])
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__':
    main()
