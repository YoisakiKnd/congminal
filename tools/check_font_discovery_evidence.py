#!/usr/bin/env python3
"""核对字体发现阶段证据的源码、夹具、冻结基线与实际测量范围；不完成整个计划。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    folder = root / 'docs/evaluations'
    source = {str(p.relative_to(root)): digest(p.read_bytes()) for p in sorted((root / 'src').rglob('*.rs'))}
    manifests = {n: digest((root / n).read_bytes()) for n in ['Cargo.toml', 'Cargo.lock']}
    reports = {}
    identities = {}
    for suffix in ['allocation', 'allocation-msrv', 'cost', 'build', 'file-quota', 'workspace']:
        path = folder / f'm4-font-discovery-{suffix}.json'
        report = json.loads(path.read_text())
        assert report['source_sha256'] == source, suffix
        assert report['manifests_sha256'] == manifests, suffix
        if 'tool_sha256' in report:
            tool = 'check_sftp_executor.py' if suffix in ['file-quota', 'workspace'] else 'check_font_discovery.py'
            assert report['tool_sha256'] == digest((root / 'tools' / tool).read_bytes())
        for key in ['fixture', 'allocation_fixtures', 'fixtures']:
            for name, item in report.get(key, {}).items():
                data = (root / name).read_bytes()
                assert item['sha256'] == digest(data)
                assert item['source'] == data.decode('utf-8')
        identities[str(path.relative_to(root))] = digest(path.read_bytes())
        reports[suffix] = report
    for suffix in ['allocation', 'allocation-msrv']:
        r = reports[suffix]
        assert r['passed'] and r['sanity']['exit'] == r['measurement']['exit'] == r['unicode_query']['exit'] == 0
        samples = r['samples']
        assert [s['step'] for s in samples] == ['generated', 'retained_face', 'generated_released', 'system', 'system_retained', 'system_released']
        assert all(0 <= s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] for s in samples)
        assert all(samples[i]['live'] == samples[i]['used'] == 0 for i in [2, 5])
        unicode = re.findall(r'FONT_UNICODE_QUERY bytes=(\d+) peak=(\d+) budget_peak=(\d+) live=(\d+) used=(\d+)', r['unicode_query']['stderr'])
        assert len(unicode) == 7 and all(int(a[1]) <= int(a[2]) and int(a[3]) == int(a[4]) == 0 for a in unicode)
        assert 'FONT_DISCOVERY_FINAL live=0 used=0 denied_new_allocations=0' in r['measurement']['stderr']
    cost = reports['cost']
    assert cost['passed'] and len(cost['runs']) == 30 and len(cost['warmups']) == 12
    for mode in ['scan', 'lookup', 'load']:
        group = [r for r in cost['runs'] + cost['warmups'] if r['mode'] == mode]
        assert len(group) == 14 and len({r['signature'] for r in group}) == 1
        assert all(r['exit'] == 0 and len(r['times_ns']) == 5 and r['maximum_rss_bytes'] > 0 for r in group)
        if mode == 'scan':
            assert len({(r['faces'], r['names']) for r in group}) == 1
            assert group[0]['faces'] > 0
            assert all(r['log'].count('FONT_DISCOVERY_STATUS limited=false') == 5 for r in group if r['cohort'] == 'after')
    build = reports['build']
    assert build['passed'] and len(build['checks']) == 9 and all(c['exit'] == 0 for c in build['checks'])
    expected = {1: [536], 2: [539, 18], 6: [47]}
    for i, counts in expected.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', build['checks'][i]['stdout'])] == counts
    for suffix in ['file-quota', 'workspace']:
        r = reports[suffix]
        assert r['sanity']['exit'] == r['measurement']['exit'] == 0
        assert all(s['live'] <= s['parent_used'] for s in r['samples'])
        assert all(r['samples'][-1][key] == 0 for key in ['live', 'parent_used', 'child_used'])
    quota = reports['file-quota']
    assert quota['input']['io_headroom'] == 9 * 1024 * 1024
    assert quota['queue']['admitted'] == 13 and quota['queue']['reason'] == 'budget'
    for key in ['local_picker', 'exit_view', 'retry', 'paths', 'lookup', 'review']:
        assert reports['workspace'][key]['paint_allocations'] == reports['workspace'][key]['paint_reallocations'] == 0
    archive = folder / 'baselines/m4-font-discovery-before.tar.gz'
    with tarfile.open(archive) as tar:
        identity = json.load(tar.extractfile('source-identity.json'))
        archived = {m.name: digest(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert identity['source_sha256'] == cost['baseline_source_sha256']
    assert identity['manifests_sha256'] == cost['baseline_manifests_sha256']
    for name, sha in identity['source_sha256'].items() | identity['manifests_sha256'].items():
        assert archived[name] == sha
    assets = {str(p.relative_to(root)): digest(p.read_bytes()) for p in (root / 'assets').rglob('*') if p.is_file()}
    assert {name: sha for name, sha in archived.items() if name.startswith('assets/')} == assets
    failed = json.loads((folder / 'm4-font-discovery-unicode-before-reserve.json').read_text())
    assert failed['unicode_query']['exit'] != 0
    assert 'bytes=2 peak=10 budget_peak=8' in failed['unicode_query']['stderr']
    result = dict(passed=True, goal_complete=False,
                  scope='Font discovery metadata/header workload only; source/fixture/archive and measured release checks verified. No complete M4/M6, GUI latency, FD/kernel cancellation, Windows/Linux runtime or long-run acceptance.',
                  source_sha256=source, manifests_sha256=manifests, reports_sha256=identities,
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=digest(archive.read_bytes())),
                  audit_tool_sha256=digest(Path(__file__).read_bytes()), cost_summary=cost['summary'])
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__':
    main()
