#!/usr/bin/env python3
"""字体映射阶段的源码/基线/实际分配与同负载证据核对；完整计划继续执行。"""
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
    root = Path(__file__).resolve().parent.parent; folder = root/'docs/evaluations'
    sources = {str(p.relative_to(root)): sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))}
    manifests = {n: sha((root/n).read_bytes()) for n in ['Cargo.toml', 'Cargo.lock']}
    reports = {}; identities = {}
    for suffix in ['allocation', 'allocation-msrv', 'cost', 'build', 'workspace']:
        path = folder/f'm4-font-map-{suffix}.json'; r = json.loads(path.read_text())
        assert r['source_sha256'] == sources and r['manifests_sha256'] == manifests
        if 'tool_sha256' in r:
            tool = 'check_sftp_executor.py' if suffix == 'workspace' else 'check_font_map.py'
            assert r['tool_sha256'] == sha((root/'tools'/tool).read_bytes())
        for name, digest in r.get('helper_sha256', {}).items(): assert sha((root/name).read_bytes()) == digest
        for key in ['fixture', 'allocation_fixtures', 'fixtures']:
            for name, item in r.get(key, {}).items():
                data = (root/name).read_bytes(); assert item['sha256'] == sha(data) and item['source'] == data.decode('utf-8')
        identities[str(path.relative_to(root))] = sha(path.read_bytes()); reports[suffix] = r
    for suffix in ['allocation', 'allocation-msrv']:
        r = reports[suffix]; assert r['passed'] and r['sanity']['exit'] == r['measurement']['exit'] == 0
        samples = r['samples']; assert [s['step'] for s in samples] == ['mapped', 'alias', 'slice', 'unmapped', 'owned', 'owned_released', 'errors', 'fonts', 'glyphs', 'released']
        assert all(0 <= s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] for s in samples)
        assert samples[0]['allocations'] == samples[1]['allocations'] == samples[2]['allocations']
        assert samples[0]['mappings'] == samples[1]['mappings'] == samples[2]['mappings'] == 1
        assert samples[3]['live'] == samples[3]['mapped'] == samples[3]['mappings'] == 0
        assert samples[4]['live'] > 0 and samples[5]['live'] == samples[6]['live'] == 0
        assert samples[7]['live'] > 0 and samples[8]['live'] > 0
        assert all(samples[-1][key] == 0 for key in ['live', 'used', 'mapped', 'mappings'])
        assert 'FONT_MAP_FINAL live=0 used=0 mapped=0 mappings=0 denied_new_allocations=0' in r['measurement']['stderr']
    cost = reports['cost']; assert cost['passed'] and len(cost['runs']) == 30 and len(cost['warmups']) == 12
    assert sha(cost['injected_cost_source'].encode()) == cost['injected_cost_sha256']
    assert 'let mut times=[0u64;50]' in cost['injected_cost_source']
    for mode in ['scan', 'lookup', 'load']:
        rows = [r for r in cost['runs']+cost['warmups'] if r['mode'] == mode]
        assert len(rows) == 14 and len({r['signature'] for r in rows}) == 1
        assert all(r['exit'] == 0 and len(r['times_ns']) == 50 and r['maximum_rss_bytes'] > 0 for r in rows)
        if mode == 'scan':
            assert len({(r['faces'], r['names']) for r in rows}) == 1 and rows[0]['faces'] > 0
            assert all(r['log'].count('FONT_DISCOVERY_STATUS limited=false') == 50 for r in rows if r['cohort'] == 'after')
    build = reports['build']; assert build['passed'] and len(build['checks']) == 9 and all(c['exit'] == 0 for c in build['checks'])
    for i, counts in {1: [546], 2: [549, 18], 6: [57]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;', build['checks'][i]['stdout'])] == counts
    workspace = reports['workspace']; assert workspace['sanity']['exit'] == workspace['measurement']['exit'] == 0
    assert all(s['live'] <= s['parent_used'] for s in workspace['samples'])
    assert all(workspace['samples'][-1][key] == 0 for key in ['live', 'parent_used', 'child_used'])
    for key in ['local_picker', 'exit_view', 'retry', 'paths', 'lookup', 'review']:
        assert workspace[key]['paint_allocations'] == workspace[key]['paint_reallocations'] == 0
    archive = folder/'baselines/m4-font-map-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline = json.load(tar.extractfile('source-identity.json'))
        members = {m.name: sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert baseline['source_sha256'] == cost['baseline_source_sha256'] == json.loads((folder/'m4-font-metadata-audit.json').read_text())['source_sha256']
    assert baseline['manifests_sha256'] == cost['baseline_manifests_sha256'] == manifests
    for name, digest in baseline['source_sha256'].items() | manifests.items(): assert members[name] == digest
    assert {n: digest for n, digest in members.items() if n.startswith('assets/')} == {str(p.relative_to(root)): sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes = sorted(n for n in sources.keys() | baseline['source_sha256'].keys() if sources.get(n) != baseline['source_sha256'].get(n))
    assert changes == ['src/font/gsub.rs', 'src/font/metadata.rs', 'src/font/mmap.rs', 'src/font/mmap_cache.rs', 'src/font/mmap_lock.rs', 'src/font/mod.rs', 'src/font/ttf.rs']
    failed = json.loads((folder/'m4-font-map-allocation-before-static-lock.json').read_text())
    assert failed['measurement']['exit'] != 0 and 'FONT_MAP_UNFUNDED step=released live=64' in failed['measurement']['stderr']
    short_path = folder/'m4-font-map-cost-short-samples.json'; short = json.loads(short_path.read_text())
    assert short['source_sha256'] == sources and short['passed']
    assert short['tool_sha256'] == sha((folder/'baselines/check-font-map-short.py').read_bytes())
    assert all(len(r['times_ns']) == 5 for r in short['runs'])
    result = dict(passed=True, goal_complete=False,
                  scope='Local font Storage/Arc/weak cache/file identity/lock/fixed errors and representative actual Fonts load/glyph request accounting only. Excludes root context/global task/UI metadata, allocator/C/OS/stack/RSS, native presentation, kernel close/deadline, other-platform runtime and long-run acceptance.',
                  source_sha256=sources, manifests_sha256=manifests, changed_sources=changes,
                  reports_sha256=identities, audit_tool_sha256=sha(Path(__file__).read_bytes()),
                  baseline=dict(path=str(archive.relative_to(root)), bytes=archive.stat().st_size, sha256=sha(archive.read_bytes())),
                  short_samples_sha256=sha(short_path.read_bytes()), cost_summary=cost['summary'])
    args.output.write_text(json.dumps(result, indent=2)+'\n'); print(json.dumps(dict(passed=True, reports=len(reports), goal_complete=False)))


if __name__ == '__main__': main()
