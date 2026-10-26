#!/usr/bin/env python3
"""Link pool and detection admission/lifetime and frozen-source cost, no native delivery claim."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
from check_font_discovery import build, sources
from check_sftp_executor import identity, run

CASE = 'links::cost_probe::same_pool_protocol_and_hit_work'
ADAPTERS = {
    'before': 'fn insert(links:&mut Links,url:&str,line:u64)->u32{links.insert(url.to_owned(),line)}',
    'after': 'fn insert(links:&mut Links,url:&str,line:u64)->u32{links.insert(url,line)}',
}


def project(source, target, fixture, adapter=''):
    for name in ['src', 'assets']:
        shutil.copytree(source/name, target/name)
    for name in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(source/name, target/name)
    file = target/'src/links.rs'
    file.write_text(file.read_text()+'\n'+fixture.replace('// LINK_ADAPTER', adapter))


def allocation(root, args, result):
    result['scope'] = 'Same-epoch all-thread Rust request meter, conservative realloc old/new overlap. Preinitialized4MiB owner and zero current budget;1024 short URLs and all deque growth, fixed lookup/touch no allocation, sparse prune retained paid capacity, URL copied once then moved without allocation to actual CopyText, original pool dropped while request remains, full-pool independent refusal, final0. Separately actual Terminal visible/OSC/miss detection: exactly two paid final URL allocations, invalid-word0, source drop then output owner drop final0. Stack2048B token and parser/terminal/input/root lock/context initialization outside heap measurement. No launcher command/task/global metadata, OS opener/native latency/protocol/platform/long-run claim.'
    fixtures = [root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs', 'link_allocation_cases.rs']]
    result['allocation_fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-link-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p/'src/executor_allocation_meter.rs')
        main = p/'src/main.rs'
        main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix+['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'links::allocation_probe::', '--nocapture', '--test-threads=1'], p, 60)
        log = result['measurement']['stdout']+result['measurement']['stderr']
        keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'LINK_ALLOC step=(\w+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        assert result['sanity']['exit'] == result['measurement']['exit'] == 0, log[-14000:]
        assert len(result['samples']) == 6 and 'LINK_FINAL denied_new_allocations=0 lookup_new_allocations=0 final=0' in log
        assert all(s['live'] == s['used'] and s['peak'] <= s['budget_peak'] for s in result['samples'])
        result['growth'] = [dict(zip(['entries','capacity','live','peak','used','budget_peak'],map(int,m))) for m in re.findall(r'LINK_GROW entries=(\d+) capacity=(\d+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+)',log)]
        assert len(result['growth'])==9 and all(s['live']==s['used'] and s['peak']<=s['budget_peak'] for s in result['growth'])
        result['entry_size'] = int(re.search(r'entry_size=(\d+)', log)[1])
        assert 'LINK_HITS allocations=2 reallocations=0' in log and 'invalid_new_allocations=0 handoff_new_allocations=0 final=0' in log



def cost(root, args, result, fixture):
    assert args.baseline is not None, '--cost requires frozen --baseline'
    result['scope'] = 'Identical System-allocator fixture body with explicit original String versus borrowed-str insert adapters. Two warmups/cohort then five alternating pairs.200x1024 existing ID get/touch,1000evicting insertions,1000each visible UTF8/OSC/miss detections with coordinates,200x1000 actual OSC8 parser/output/prune frames. Lookup values, IDs, detection URL/ranges and final visible cell link associations fingerprint equal. API timing excludes hash/final owners drop; inserts include original pre-insert argument String creation/new actual Url admission. time-lp whole-process RSS/CPU includes terminal/history setup/hash/destruction; no launcher/system browser/native input/presentation/other-platform/long-run claim. No new runtime process/thread/package.'
    result['adapters'] = {k: dict(source=v, sha256=hashlib.sha256(v.encode()).hexdigest()) for k, v in ADAPTERS.items()}
    result['baseline_source_sha256'] = sources(args.baseline)
    result['baseline_manifests_sha256'] = {n: hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-link-cost-') as temporary:
        result['rustc'] = run(['rustc', '-Vv'], Path(temporary), 15)
        builds, result['builds'] = {}, {}
        for cohort, source in [('before', args.baseline), ('after', root)]:
            p = Path(temporary)/cohort
            p.mkdir()
            project(source, p, fixture, ADAPTERS[cohort])
            binary, result['builds'][cohort] = build(p, [])
            builds[cohort] = p, binary

        def metric(samples):
            ordered = sorted(samples)
            n = len(samples)
            return dict(samples_ns=samples, total_ns=sum(samples), p50_ns=ordered[(n-1)//2], p95_ns=ordered[(n*95-1)//100], p99_ns=ordered[(n*99-1)//100], maximum_ns=ordered[-1])

        def trial(cohort):
            p, binary = builds[cohort]
            proc = subprocess.run(['/usr/bin/time', '-lp', str(binary), CASE, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=p, capture_output=True, text=True, timeout=90)
            log = proc.stdout+proc.stderr
            match = re.search(r'LINK_COST signature=(\d+) lookup=(\[[^\n]+\]) insertion=(\[[^\n]+\]) protocol=(\[[^\n]+\]) retained=(\d+) budget_peak=(\d+) final=0', log)
            assert proc.returncode == 0 and match is not None, log[-14000:]
            r = dict(cohort=cohort, exit=proc.returncode, log=log, signature=int(match[1]), budget_peak=int(match[6]), retained=int(match[5]), lookup=metric(json.loads(match[2])), insertion=metric(json.loads(match[3])), protocol=metric(json.loads(match[4])))
            for mode, raw in re.findall(r'LINK_TIME mode=(\w+) times=(\[[^\n]+\])', log):
                r[mode] = metric(json.loads(raw))
            for key, count in [('lookup', 200), ('insertion', 1000), ('protocol', 200), ('visible', 1000), ('osc', 1000), ('miss', 1000)]:
                assert len(r[key]['samples_ns']) == count
            for key, pattern in [('user_seconds', r'^user\s+([\d.]+)$'), ('system_seconds', r'^sys\s+([\d.]+)$'), ('rss_bytes', r'^\s*(\d+)\s+maximum resident set size$')]:
                value = re.search(pattern, log, re.M)
                assert value is not None, log[-14000:]
                r[key] = int(value[1]) if key == 'rss_bytes' else float(value[1])
            return r

        result['warmups'] = [trial(c) for _ in range(2) for c in ['before', 'after']]
        result['runs'] = []
        for pair in range(5):
            for cohort in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                r = trial(cohort)
                r['pair'] = pair
                result['runs'].append(r)
                print(json.dumps(dict(cohort=cohort, pair=pair, lookup_ns=r['lookup']['total_ns'], protocol_ns=r['protocol']['total_ns'])), flush=True)
        assert len({r['signature'] for r in result['runs']+result['warmups']}) == 1
        result['summary'] = {}
        for key in ['lookup', 'insertion', 'protocol', 'visible', 'osc', 'miss', 'rss', 'cpu']:
            values = []
            for pair in range(5):
                a, b = [next(r for r in result['runs'] if r['cohort'] == c and r['pair'] == pair) for c in ['before', 'after']]
                values.append(b['rss_bytes']-a['rss_bytes'] if key == 'rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if key == 'cpu' else 100*(b[key]['total_ns']/a[key]['total_ns']-1))
            result['summary'][key] = dict(median=statistics.median(values), minimum=min(values), maximum=max(values), pairs=values)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--allocation', action='store_true')
    mode.add_argument('--cost', action='store_true')
    parser.add_argument('--toolchain')
    parser.add_argument('--baseline', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.toolchain and not args.allocation:
        parser.error('--toolchain applies to allocation')
    root = Path(__file__).resolve().parent.parent
    fixture = root/'tools/fixtures/link_cost_cases.rs'
    result = dict(source_sha256=sources(root), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}, tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), fixtures={str(fixture.relative_to(root)): identity(fixture)})
    try:
        allocation(root, args, result) if args.allocation else cost(root, args, result, fixture.read_text())
        assert sources(root) == result['source_sha256']
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
