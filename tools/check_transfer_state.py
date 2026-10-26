#!/usr/bin/env python3
"""固定传输报告的全线程额度计量/普通分配器更新成本；不验收完整SSH/GUI传输。"""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import statistics
import tempfile
from check_sftp_executor import identity, run

ALLOCATION = 'sftp::transfer_state::allocation_probe::measured_fixed_report_updates_and_maximum_paths_under_actual_pool_remain_funded'
COST = 'sftp::transfer_state::cost::isolated_coalesced_progress_cost'


def allocation(root, args, result):
    fixtures = [root/'tools/fixtures'/name for name in ['executor_allocation_meter.rs', 'transfer_state_allocation_cases.rs']]
    result['fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    result['scope'] = 'Temporary-copy all-thread epoch requested-byte meter, conservative old/new realloc overlap; headers, allocator/C/OS internals and actual stacks/RSS excluded. Parent, notify, input raw 16KiB bytes and application thread global initialization preinitialized. Measures anchored 16MiB child, actual funded report/three maximum escaping paths, 1000001 updates plus coalesced reads, publication/finish, independent partial path after writer/reader/child handle drop. Separately measures actual fixed 2-worker/64-wait/2-result executor/new three threads/TLS with same report DTOs and maximum paths until child budget rejects admission, cancellation, actual joined shutdown, reports retained after pool drop, then zero. No file/network/GUI/Service transfer DTO/latency/throughput claim. Queue capacity does not guarantee every maximum-sized input fits quota.'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='congminal-transfer-report-allocation-') as temporary:
        copy = Path(temporary)
        for name in ['src', 'assets']:
            shutil.copytree(root/name, copy/name)
        for name in ['Cargo.toml', 'Cargo.lock']:
            shutil.copy2(root/name, copy/name)
        shutil.copy2(fixtures[0], copy/'src/executor_allocation_meter.rs')
        main = copy/'src/main.rs'
        main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        target = copy/'src/sftp/transfer_state.rs'
        target.write_text(target.read_text()+'\n'+fixtures[1].read_text())
        result['rustc'] = run(prefix+['rustc', '-Vv'], copy, 15)
        result['sanity'] = run(prefix+['cargo', 'test', '--offline', '--release', 'action::tests', '--', '--test-threads=1'], copy, 180)
        if result['sanity']['exit']:
            raise RuntimeError(result['sanity']['stdout']+result['sanity']['stderr'])
        result['measurement'] = run(prefix+['cargo', 'test', '--offline', '--release', ALLOCATION, '--', '--exact', '--nocapture', '--test-threads=1'], copy, 180)
        log = result['measurement']['stdout']+result['measurement']['stderr']
        keys = ['live', 'peak', 'parent_used', 'parent_peak', 'child_used', 'allocations', 'reallocations']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'TRANSFER_REPORT_ALLOC step=(\w+) live=(-?\d+) peak=(-?\d+) parent_used=(\d+) parent_peak=(\d+) child_used=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        match = re.search(r'TRANSFER_REPORT_QUEUE admitted=(\d+) running=2 queued=(\d+) refused=budget limit=(\d+)', log)
        if result['measurement']['exit'] or len(result['samples']) != 11 or not match:
            raise RuntimeError(log[-16000:])
        result['queue'] = dict(zip(['admitted', 'queued', 'limit'], map(int, match.groups())))
        if not (2 < result['queue']['admitted'] < 66 and result['queue']['queued']+2 == result['queue']['admitted'] and result['queue']['limit'] == 16*1024*1024):
            raise RuntimeError('invalid bounded admission')
        samples = result['samples']
        if any(s['live'] < 0 or s['live'] > s['parent_used'] or s['peak'] > s['parent_peak'] or s['child_used'] > 16*1024*1024 for s in samples):
            raise RuntimeError('unbacked report bytes')
        if any(samples[i]['allocations'] != samples[0]['allocations'] or samples[i]['reallocations'] != samples[0]['reallocations'] for i in [1, 2]):
            raise RuntimeError('updates or publication allocated')
        if any(samples[i]['live'] or samples[i]['parent_used'] for i in [4, 9, 10]):
            raise RuntimeError('report credit not zero')
        print(json.dumps(dict(samples=samples, queue=result['queue'])), flush=True)


def cost(root, args, result):
    if args.toolchain:
        raise RuntimeError('--toolchain is only supported with --allocation')
    result['scope'] = 'Actual System allocator, isolated release test; common prebuilt report/path setup excluded from timed loop. Each mode has 1000000 monotonic counter updates and 976 reads; report mode updates/reads actual funded mutex slot, baseline keeps primitive caller counters without shared reporting. Baseline is weaker, not equivalent functionality. Two warmups per mode and five alternating pairs, absolute incremental ns/update only; no percentage adoption threshold, SSH/SFTP/file IO, GUI latency, RSS/CPU sampling or contention/long-idle claim.'
    result['build'] = run(['cargo', 'test', '--offline', '--release', '--no-run', '--message-format=json'], root, 180)
    if result['build']['exit']:
        raise RuntimeError(result['build']['stderr'])
    artifacts = []
    for line in result['build']['stdout'].splitlines():
        item = json.loads(line)
        if item.get('reason') == 'compiler-artifact' and item.get('profile', {}).get('test') and item.get('target', {}).get('name') == 'congminal' and item.get('executable'):
            artifacts.append(item['executable'])
    if len(artifacts) != 1:
        raise RuntimeError('ambiguous test binary')
    result['runs'] = []
    pairs = []
    for index in range(7):
        order = ['baseline', 'report'] if index % 2 == 0 else ['report', 'baseline']
        current = {}
        for mode in order:
            row = run(['env', 'CONGMINAL_TRANSFER_STATE_MODE='+mode, artifacts[0], COST, '--exact', '--ignored', '--nocapture', '--test-threads=1'], root, 15)
            log = row['stdout']+row['stderr']
            match = re.search(r'TRANSFER_STATE_COST mode=(\w+) updates=1000000 reads=976 elapsed_ns=(\d+) checksum=(\d+) final=0', log)
            if row['exit'] or not match or match[1] != mode:
                raise RuntimeError(log[-12000:])
            row.update(mode=mode, warmup=index < 2, elapsed_ns=int(match[2]), checksum=int(match[3]))
            result['runs'].append(row)
            current[mode] = row['elapsed_ns']
        if index >= 2:
            pairs.append((current['report']-current['baseline'])/1_000_000)
    if len({row['checksum'] for row in result['runs']}) != 1:
        raise RuntimeError('control checksum differs')
    result['incremental_ns_per_update'] = dict(values=pairs, median=statistics.median(pairs), min=min(pairs), max=max(pairs))
    print(json.dumps(result['incremental_ns_per_update']), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--allocation', action='store_true')
    mode.add_argument('--cost', action='store_true')
    parser.add_argument('--toolchain')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    def sources():
        return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}
    result = dict(platform=platform.platform(), source_sha256=sources(), tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), helper_tool_sha256=hashlib.sha256((root/'tools/check_sftp_executor.py').read_bytes()).hexdigest(), manifests_sha256={name: hashlib.sha256((root/name).read_bytes()).hexdigest() for name in ['Cargo.toml', 'Cargo.lock']})
    try:
        (allocation if args.allocation else cost)(root, args, result)
        if sources() != result['source_sha256']:
            raise RuntimeError('source changed during measurement')
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
