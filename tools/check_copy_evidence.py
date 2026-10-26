#!/usr/bin/env python3
"""核对 Outgoing clipboard stage的来源、分配和同负载证据，不完成全计划。"""
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
        path = folder/f'm4-copy-resources-{suffix}.json'
        r = json.loads(path.read_text())
        assert r['source_sha256'] == source and r['manifests_sha256'] == manifests, suffix
        tool = ('check_sftp_executor.py' if suffix in ['file-quota', 'workspace'] else
                'check_ui_state.py' if suffix.startswith('ui-') else
                'check_copy_build.py' if suffix == 'build' else 'check_copy_resources.py')
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
        groups = {mode: [s for s in r['samples'] if s['mode'] == mode] for mode in ['payload', 'output']}
        assert [s['step'] for s in groups['payload']] == ['grown', 'pending', 'platform_owned', 'cleared', 'wide', 'denied', 'released']
        assert [s['live'] for s in groups['payload']] == [2097152, 2097152, 2097152, 0, 10485762, 10485762, 0]
        assert [s['used'] for s in groups['payload']] == [2097152, 2097152, 2097152, 0, 18874372, 18874372, 0]
        assert [s['allocations'] for s in groups['payload']] == [19, 19, 19, 19, 21, 21, 21]
        assert all(s['reallocations'] == 18 for s in r['samples'])
        assert all(s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] for s in r['samples'])
        assert groups['payload'][0]['peak'] == groups['payload'][0]['budget_peak'] == 3145728
        assert [s['step'] for s in groups['output']] == ['ready', 'source_dropped', 'released']
        assert [s['live'] for s in groups['output']] == [2097152, 2097152, 0]
        assert all(s['used'] == s['live'] and s['peak'] == 3145728 for s in groups['output'])
        assert 'COPY_PAYLOAD_FINAL denied_new_allocations=0 owner_other_used=0 final=0' in r['measurement']['stderr']
        assert re.search(r'COPY_OUTPUT_FINAL bytes=1920000 steps=\d+ owner_other_used=0 root_final=0', r['measurement']['stderr'])
    for suffix, metrics in [('cost', [('short', 1000), ('large', 8), ('direct', 5000), ('encoding', 8)]),
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
    from check_copy_resources import ADAPTERS
    assert reports['cost']['adapters'] == {k: dict(source=v, sha256=sha(v.encode())) for k, v in ADAPTERS.items()}
    for r in reports['cost']['runs']+reports['cost']['warmups']:
        assert 8 <= len(r['slices']['samples_ns']) <= 16384
    b = reports['build']
    assert b['passed'] and len(b['checks']) == 10 and all(r['exit'] == 0 for r in b['checks'])
    for index, counts in {1: [568], 2: [571, 18], 6: [11], 7: [12]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', b['checks'][index]['stdout'])] == counts
    for suffix in ['workspace', 'file-quota']:
        r = reports[suffix]
        assert r['sanity']['exit'] == r['measurement']['exit'] == 0
        assert all(s['live'] <= s['parent_used'] for s in r['samples'])
        assert all(r['samples'][-1][key] == 0 for key in ['live', 'parent_used', 'child_used'])
    assert reports['file-quota']['input']['io_headroom'] == 9*1024*1024
    assert reports['file-quota']['queue'] == dict(admitted=13, queued=11, reason='budget')
    assert reports['file-quota']['type_sizes']['budget'] == 320
    for key in ['local_picker', 'exit_view', 'retry', 'paths', 'lookup', 'review']:
        assert reports['workspace'][key]['paint_allocations'] == reports['workspace'][key]['paint_reallocations'] == 0
    ui = reports['ui-allocation']
    assert ui['passed'] and all(ui[key]['exit'] == 0 for key in ['sanity', 'cold', 'warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert len(ui['frames']['update']['samples_ns']) == 5000 and len(ui['frames']['draw']['samples_ns']) == 500
    assert ui['frames']['signature'] == reports['ui-cost']['runs'][0]['signature'] == 6821514814623131185
    archive = folder/'baselines/m4-copy-resources-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline = json.load(tar.extractfile('source-identity.json'))
        members = {m.name: sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    for suffix in ['cost', 'ui-cost']:
        assert baseline['source_sha256'] == reports[suffix]['baseline_source_sha256']
        assert baseline['manifests_sha256'] == reports[suffix]['baseline_manifests_sha256'] == manifests
    assert baseline['source_sha256'] == json.loads((folder/'m4-shell-resources-audit.json').read_text())['source_sha256']
    for name, digest in baseline['source_sha256'].items() | manifests.items():
        assert members[name] == digest
    assert {n: v for n, v in members.items() if n.startswith('assets/')} == {
        str(p.relative_to(root)): sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes = sorted(n for n in source.keys() | baseline['source_sha256'].keys() if source.get(n) != baseline['source_sha256'].get(n))
    assert changes == ['src/main.rs', 'src/platform/clipboard.rs', 'src/platform/macos.rs', 'src/platform/mod.rs', 'src/platform/win32.rs', 'src/platform/x11.rs', 'src/resources/budget.rs', 'src/shell.rs', 'src/ssh/auth_controller.rs', 'src/term/memory.rs', 'src/workspace.rs']
    result = dict(passed=True, goal_complete=False,
                  scope='Outgoing copy buffer/request/platform-owner quota, Windows UTF16/GlobalAlloc request overlap, window copy epoch/no stale publication; actual Rust allocation lifetimes, same-work cost and adjacent UI/SFTP. Incoming plain paste/link detection/native OS clipboard protocols, root/context/lock/platform/config/task metadata, native latency/platform/long-run remain outside completion.',
                  source_sha256=source, manifests_sha256=manifests, changed_sources=changes,
                  reports_sha256=identities, audit_tool_sha256=sha(Path(__file__).read_bytes()),
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=sha(archive.read_bytes())),
                  cost_summary=reports['cost']['summary'], ui_cost_summary=reports['ui-cost']['summary'])
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__':
    main()
