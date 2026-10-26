#!/usr/bin/env python3
"""字体文件映射/控制块/固定弱缓存/错误：真实加载请求计量与冻结源码成本对照。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import os
import sys
import tempfile
from check_font_discovery import build, project, sources
from check_sftp_executor import identity, run


def costs(root, args, result, fixture):
    if args.baseline is None or sys.platform != 'darwin':
        raise RuntimeError('cost requires frozen --baseline and macOS time -lp')
    result['scope'] = 'Same System-allocator font directory, cached queries and default load/glyph verification fixture in frozen baseline/current temporary copies. Fifty fresh scans or default loads per trial; lookup fifty batches of1000 queries after warm catalog. Two warmups per cohort/mode then five alternating pairs. API timing excludes verification/result drops; load excludes later glyph generation. Entire-process time -lp CPU/RSS includes harness, signature, drops/mappings/cache; CPU resolution0.01s. Fixed local system fonts, not cold disk, startup-to-window, native input/presentation, other-platform runtime or long-run. No added package/runtime child/thread. Short five-iteration trial preserved separately because of variance.'
    assert fixture.count('let mut times=[0u64;5]') == 1
    fixture = fixture.replace('let mut times=[0u64;5]', 'let mut times=[0u64;50]')
    result['injected_cost_source'] = fixture
    result['injected_cost_sha256'] = hashlib.sha256(fixture.encode()).hexdigest()
    result['baseline_source_sha256'] = sources(args.baseline)
    result['baseline_manifests_sha256'] = {n: hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-font-map-cost-') as temporary:
        result['rustc'] = run(['rustc', '-Vv'], Path(temporary), 15)
        projects = {}; result['builds'] = {}
        for cohort, source in [('before', args.baseline), ('after', root)]:
            p = Path(temporary)/cohort; p.mkdir()
            code = fixture.replace('// FONT_DISCOVERY_STATUS', 'eprintln!("FONT_DISCOVERY_STATUS limited={}",faces.limited);' if cohort == 'after' else '')
            project(source, p, code); binary, built = build(p, []); projects[cohort] = (p, binary); result['builds'][cohort] = built
        def trial(cohort, mode):
            p, binary = projects[cohort]
            command = ['/usr/bin/time', '-lp', str(binary), 'font::discovery_cost::same_catalog_queries_and_loaded_glyphs', '--exact', '--ignored', '--nocapture', '--test-threads=1']
            process = subprocess.run(command, cwd=p, env=dict(os.environ, CONGMINAL_FONT_MODE=mode), capture_output=True, text=True, timeout=60)
            log = process.stdout + process.stderr
            m = re.search(r'FONT_DISCOVERY_COST mode=(scan|lookup|load) times=(\[[^\n]+\]) signature=(\d+) faces=(\d+) names=(\d+) budget_peak=(\d+) final=0', log)
            if process.returncode or m is None or m[1] != mode: raise RuntimeError(log[-12000:])
            times = json.loads(m[2]); assert len(times) == 50
            def number(pattern):
                value = re.search(pattern, log, re.M)
                if value is None: raise RuntimeError(log[-12000:])
                return float(value[1])
            return dict(cohort=cohort, mode=mode, exit=process.returncode, log=log, times_ns=times, total_ns=sum(times), signature=int(m[3]), faces=int(m[4]), names=int(m[5]), budget_peak=int(m[6]), user_seconds=number(r'^user\s+([\d.]+)$'), system_seconds=number(r'^sys\s+([\d.]+)$'), maximum_rss_bytes=int(number(r'^\s*(\d+)\s+maximum resident set size$')))
        result['warmups'], result['runs'] = [], []
        for mode in ['scan', 'lookup', 'load']:
            for _ in range(2):
                for cohort in ['before', 'after']: result['warmups'].append(trial(cohort, mode))
            for pair in range(5):
                for cohort in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                    r = trial(cohort, mode); r['pair'] = pair; result['runs'].append(r)
                    print(json.dumps(dict(cohort=cohort, mode=mode, pair=pair, total_ns=r['total_ns'])), flush=True)
        result['summary'] = {}
        for mode in ['scan', 'lookup', 'load']:
            deltas = {'time_percent': [], 'rss_delta_bytes': [], 'cpu_delta_seconds': []}
            for pair in range(5):
                a, b = [next(r for r in result['runs'] if r['mode'] == mode and r['pair'] == pair and r['cohort'] == c) for c in ['before', 'after']]
                assert (a['signature'], a['faces'], a['names']) == (b['signature'], b['faces'], b['names'])
                if mode == 'scan': assert b['log'].count('FONT_DISCOVERY_STATUS limited=false') == 50
                deltas['time_percent'].append(100*(b['total_ns']/a['total_ns']-1))
                deltas['rss_delta_bytes'].append(b['maximum_rss_bytes']-a['maximum_rss_bytes'])
                deltas['cpu_delta_seconds'].append(b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'])
            result['summary'][mode] = {k: dict(median=statistics.median(v), minimum=min(v), maximum=max(v), pairs=v) for k, v in deltas.items()}


def allocation(root, args, result):
    result['scope'] = 'Temporary-copy all-thread epoch Rust requested-byte meter with conservative realloc overlap. Budget/thread context, input filesystem fixture/config/FaceInfo preinitialized. Cold real map/hardlink cache hit/slice final release; direct real owned-read fallback (does not force OS mmap failure),16 retained malformed-font errors and fixed writer, real native default Fonts::load and A/Chinese/snowman/ZWJ materialization, final requested/managed/mapped/count zero, full-budget refusal zero new allocations. Preinitialized input metadata stays charged until dropped. Fixed cache slots have static storage, not heap. Excludes meter headers/allocator/C/OS/actual stack/RSS, budget context global metadata, native UI/kernel deadline, other-platform runtime/long-run acceptance.'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    fixtures = [root / 'tools/fixtures' / n for n in ['executor_allocation_meter.rs', 'font_map_allocation_cases.rs']]
    result['allocation_fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    with tempfile.TemporaryDirectory(prefix='cg-font-map-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p / 'src/executor_allocation_meter.rs')
        main = p / 'src/main.rs'
        main.write_text(main.read_text() + '\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix + ['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'font::map_allocation::cold_real_mapping_owned_fallback_fixed_errors_and_fonts_release_all_credit', '--exact', '--nocapture', '--test-threads=1'], p, 30)
        log = result['measurement']['stdout'] + result['measurement']['stderr']
        keys = ['live', 'peak', 'used', 'budget_peak', 'mapped', 'mappings', 'allocations', 'reallocations']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'FONT_MAP_ALLOC step=(\w+) live=(-?\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) mapped=(\d+) mappings=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        if result['sanity']['exit'] or result['measurement']['exit'] or len(result['samples']) != 10 or 'FONT_MAP_FINAL live=0 used=0 mapped=0 mappings=0 denied_new_allocations=0' not in log:
            raise RuntimeError(log[-14000:])
        if any(s['live'] < 0 or s['live'] > s['used'] or s['peak'] > s['budget_peak'] for s in result['samples']):
            raise RuntimeError('unfunded font map/loaded glyphs')


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
        parser.error('--toolchain applies to --allocation')
    root = Path(__file__).resolve().parent.parent
    fixture = root / 'tools/fixtures/font_discovery_cost_cases.rs'
    helpers = [root / 'tools' / n for n in ['check_font_discovery.py', 'check_sftp_executor.py', 'check_ssh_transport.py']]
    result = dict(source_sha256=sources(root), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}, tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), helper_sha256={str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in helpers}, fixture={str(fixture.relative_to(root)): identity(fixture)})
    try:
        if args.cost:
            costs(root, args, result, fixture.read_text())
        else:
            allocation(root, args, result)
        if sources(root) != result['source_sha256']:
            raise RuntimeError('source changed during measurement')
        result['passed'] = True
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
