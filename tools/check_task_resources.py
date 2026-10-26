#!/usr/bin/env python3
"""LatestTask owned core/cancellation quota and same-load costs; excludes OS thread runtime."""
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


def project(source, target, fixture):
    for name in ['src', 'assets']:
        shutil.copytree(source/name, target/name)
    for name in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(source/name, target/name)
    path = target/'src/tasks.rs'
    path.write_text(path.read_text()+'\n'+fixture)


def allocation(root, args, result):
    result['scope'] = 'All-thread epoch request meter, preinitialized root budgets/context excluded. Actual SharedRef Arc, first Mutex/Condvar zero wait, paid pending Cancellation first Mutex/Condvar zero wait, replacement while old token retained, invalidate, source core destruction with retained token, final0.32 cores without heap container, 33rd/zero budget refusal zero allocations. No spawned thread, work closure/name/Notify/error/input/result dynamic payload accounting, default Cancellation, native exit, OS thread runtime/TLS/stack or full RSS claim.'
    fixtures = [root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs', 'task_allocation_cases.rs']]
    result['fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-task-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p/'src/executor_allocation_meter.rs')
        main = p/'src/main.rs'
        main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix+['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'tasks::allocation_probe::', '--nocapture', '--test-threads=1'], p, 60)
        log = result['measurement']['stdout']+result['measurement']['stderr']
        assert result['sanity']['exit'] == result['measurement']['exit'] == 0, log[-10000:]
        keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations', 'cores']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'TASK_ALLOC step=(\w+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+) cores=(\d+)', log)]
        assert len(result['samples']) == 8 and 'TASK_FINAL denied_new_allocations=0 final=0' in log
        assert all(s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] for s in result['samples'])
        result['layout'] = dict(zip(['budget', 'stats'], map(int, re.search(r'budget_size=(\d+) stats_size=(\d+)', log).groups())))


def cost(root, args, result):
    assert args.baseline is not None, '--cost requires frozen --baseline'
    result['scope'] = 'Identical System-allocator body in frozen/current projects. Worker held by channel,100 batches x1000 latest pending replacements,100x10000 cancel polling,100x1000 legacy default token create/cancel/zero-wait/drop; actual last result must100000.2 warmups and5 alternating pairs. API intervals exclude validation/construction/destruction; time-lp process CPU/RSS includes harness, one worker/runtime, startup/shutdown and result verification. Does not establish native GUI latency, paid thread runtime, all generic payloads or global shutdown. No runtime dependency/process/thread added.'
    fixture = root/'tools/fixtures/task_cost_cases.rs'
    result['fixtures'] = {str(fixture.relative_to(root)): identity(fixture)}
    result['baseline_source_sha256'] = sources(args.baseline)
    result['baseline_manifests_sha256'] = {n: hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}
    adapters = dict(before='', after='let until=Instant::now()+Duration::from_secs(3); while pool.snapshot().latest_tasks!=0 { assert!(Instant::now()<until); std::thread::yield_now(); } assert_eq!(pool.snapshot().used,0);')
    result['retirement_adapters'] = adapters
    with tempfile.TemporaryDirectory(prefix='cg-task-cost-') as temporary:
        builds, result['builds'] = {}, {}
        result['rustc'] = run(['rustc', '-Vv'], Path(temporary), 15)
        for cohort, source in [('before', args.baseline), ('after', root)]:
            p = Path(temporary)/cohort
            p.mkdir()
            project(source, p, fixture.read_text().replace('// TASK_RETIRE', adapters[cohort]))
            binary, result['builds'][cohort] = build(p, [])
            builds[cohort] = p, binary

        def trial(cohort):
            p, binary = builds[cohort]
            proc = subprocess.run(['/usr/bin/time', '-lp', str(binary), 'tasks::cost_probe::same_pending_replacements_and_cancellation', '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=p, capture_output=True, text=True, timeout=60)
            log = proc.stdout+proc.stderr
            match = re.search(r'TASK_COST signature=(\d+) pending=(\[[^\n]+\]) polling=(\[[^\n]+\]) tokens=(\[[^\n]+\]) budget_peak=(\d+)', log)
            assert proc.returncode == 0 and match is not None, log[-10000:]
            r = dict(cohort=cohort, exit=proc.returncode, log=log, signature=int(match[1]), budget_peak=int(match[5]))
            for key, raw in zip(['pending', 'polling', 'tokens'], match.groups()[1:4]):
                values = json.loads(raw)
                assert len(values) == 100
                r[key] = dict(samples_ns=values, total_ns=sum(values))
            for key, pattern in [('user_seconds', r'^user\s+([\d.]+)$'), ('system_seconds', r'^sys\s+([\d.]+)$'), ('rss_bytes', r'^\s*(\d+)\s+maximum resident set size$')]:
                value = re.search(pattern, log, re.M)
                assert value is not None, log[-10000:]
                r[key] = int(value[1]) if key == 'rss_bytes' else float(value[1])
            return r

        result['warmups'] = [trial(c) for _ in range(2) for c in ['before', 'after']]
        result['runs'] = []
        for pair in range(5):
            for cohort in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                r = trial(cohort)
                r['pair'] = pair
                result['runs'].append(r)
                print(json.dumps(dict(cohort=cohort, pair=pair, pending_ns=r['pending']['total_ns'])), flush=True)
        assert {r['signature'] for r in result['warmups']+result['runs']} == {200000}
        result['summary'] = {}
        for key in ['pending', 'polling', 'tokens', 'rss', 'cpu']:
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
    root = Path(__file__).resolve().parent.parent
    result = dict(source_sha256=sources(root), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}, tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    try:
        allocation(root, args, result) if args.allocation else cost(root, args, result)
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
