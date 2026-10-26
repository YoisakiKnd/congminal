#!/usr/bin/env python3
"""核对字体容器与文本阶段的来源和测量证据，不把阶段通过当成完整计划完成。"""
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
    for suffix in ['allocation', 'allocation-msrv', 'cost', 'build', 'file-quota', 'workspace']:
        path = folder / f'm4-font-metadata-{suffix}.json'
        r = json.loads(path.read_text())
        assert r['source_sha256'] == source and r['manifests_sha256'] == manifests
        if 'tool_sha256' in r:
            tool = 'check_sftp_executor.py' if suffix in ['file-quota', 'workspace'] else 'check_font_metadata.py'
            assert r['tool_sha256'] == sha((root/'tools'/tool).read_bytes())
        for name, expected in r.get('helper_sha256', {}).items():
            assert sha((root/name).read_bytes()) == expected
        for key in ['fixture', 'allocation_fixtures', 'fixtures']:
            for name, item in r.get(key, {}).items():
                data = (root/name).read_bytes()
                assert item['sha256'] == sha(data) and item['source'] == data.decode('utf-8')
        identities[str(path.relative_to(root))] = sha(path.read_bytes())
        reports[suffix] = r
    for suffix in ['allocation', 'allocation-msrv']:
        r = reports[suffix]
        assert r['passed'] and r['sanity']['exit'] == r['measurement']['exit'] == 0
        s = r['samples']
        assert [v['step'] for v in s] == ['loaded', 'transferred', 'delivery_only', 'delivered', 'reload_overlap', 'replaced', 'released']
        assert all(0 <= v['live'] <= v['used'] and v['peak'] <= v['budget_peak'] for v in s)
        assert all(s[i]['live'] == s[i]['used'] == 0 for i in [3, 6])
        assert (s[0]['allocations'], s[0]['reallocations']) == (s[1]['allocations'], s[1]['reallocations'])
        assert s[2]['live'] > 0 and s[4]['live'] == 2 * s[5]['live'] > 0
        assert 'FONT_METADATA_FINAL live=0 used=0 handoff_new_allocations=0 denied_new_allocations=0' in r['measurement']['stderr']
    cost = reports['cost']
    assert cost['passed'] and len(cost['runs']) == 30 and len(cost['warmups']) == 12
    for mode in ['scan', 'lookup', 'load']:
        runs = [r for r in cost['runs'] + cost['warmups'] if r['mode'] == mode]
        assert len(runs) == 14 and len({r['signature'] for r in runs}) == 1
        assert all(r['exit'] == 0 and len(r['times_ns']) == 5 and r['maximum_rss_bytes'] > 0 for r in runs)
        if mode == 'scan':
            assert len({(r['faces'], r['names']) for r in runs}) == 1 and runs[0]['faces'] > 0
            assert all(r['log'].count('FONT_DISCOVERY_STATUS limited=false') == 5 for r in runs if r['cohort'] == 'after')
    build = reports['build']
    assert build['passed'] and len(build['checks']) == 9 and all(r['exit'] == 0 for r in build['checks'])
    for index, expected in {1: [541], 2: [544, 18], 6: [52]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', build['checks'][index]['stdout'])] == expected
    for suffix in ['file-quota', 'workspace']:
        r = reports[suffix]
        assert r['sanity']['exit'] == r['measurement']['exit'] == 0
        assert all(s['live'] <= s['parent_used'] for s in r['samples'])
        assert all(r['samples'][-1][k] == 0 for k in ['live', 'parent_used', 'child_used'])
    assert reports['file-quota']['input']['io_headroom'] == 9 * 1024 * 1024
    assert reports['file-quota']['queue']['admitted'] == 13 and reports['file-quota']['queue']['reason'] == 'budget'
    for key in ['local_picker', 'exit_view', 'retry', 'paths', 'lookup', 'review']:
        r = reports['workspace'][key]
        assert r['paint_allocations'] == r['paint_reallocations'] == 0
    archive = folder / 'baselines/m4-font-metadata-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline = json.load(tar.extractfile('source-identity.json'))
        archived = {m.name: sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert baseline['source_sha256'] == cost['baseline_source_sha256']
    assert baseline['manifests_sha256'] == cost['baseline_manifests_sha256'] == manifests
    previous = json.loads((folder/'m4-font-discovery-audit.json').read_text())
    assert baseline['source_sha256'] == previous['source_sha256']
    for name, expected in baseline['source_sha256'].items() | baseline['manifests_sha256'].items():
        assert archived[name] == expected
    assert {name: value for name, value in archived.items() if name.startswith('assets/')} == {str(p.relative_to(root)): sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes = sorted(name for name in source.keys() | baseline['source_sha256'].keys() if source.get(name) != baseline['source_sha256'].get(name))
    assert changes == ['src/font/metadata.rs', 'src/font/mod.rs', 'src/main.rs', 'src/render.rs', 'src/resources/budget.rs']
    history = json.loads((folder/'m4-font-metadata-audit-before-startup-drop.json').read_text())
    assert [name for name in source if source[name] != history['source_sha256'].get(name)] == ['src/main.rs']
    historical = {}
    for name, expected in history['reports_sha256'].items():
        path = root / name.replace('.json', '-before-startup-drop.json')
        assert sha(path.read_bytes()) == expected
        assert json.loads(path.read_text())['source_sha256'] == history['source_sha256']
        historical[str(path.relative_to(root))] = expected
    result = dict(passed=True, goal_complete=False,
                  scope='Owned font Outline Box/fallback slot containers, description/warning buffers and handoff only. Mapping weak-cache/Storage/Arc/error internals, task/global metadata, UI/window latency, actual platform runtimes and long runs remain outside completion.',
                  source_sha256=source, manifests_sha256=manifests, changed_sources=changes,
                  reports_sha256=identities, historical_reports_sha256=historical, audit_tool_sha256=sha(Path(__file__).read_bytes()),
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=sha(archive.read_bytes())),
                  cost_summary=cost['summary'])
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__':
    main()
