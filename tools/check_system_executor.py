#!/usr/bin/env python3
"""有界执行器：临时全线程请求计量，或真实System分配器的短空闲/调用方成本。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import statistics
import subprocess
import tempfile
import threading
import time
from check_ssh_transport import sampler

ALLOCATION = 'sftp::executor::allocation_probe::measured_all_thread_executor_lifecycle_preserves_parent_credit'
SERVICE_ALLOCATION = 'sftp::service::allocation_probe::measured_maximum_typed_requests_and_late_mailboxes_remain_funded'
FILE_SERVICE_ALLOCATION = 'sftp::service::file_allocation_probe::measured_maximum_file_requests_and_late_status_remain_funded'
WORKSPACE_ALLOCATION = 'workspace::remote_files::allocation_probe::measured_workspace_directory_modal_and_late_input_keep_one_parent_reservation'
COST = 'sftp::executor::cost::isolated_executor_idle_and_caller_cost'


def identity(path):
    data = path.read_bytes()
    return {'sha256': hashlib.sha256(data).hexdigest(), 'source': data.decode('utf-8')}


def run(command, cwd, timeout):
    process = subprocess.Popen(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, encoding='utf-8', errors='replace', start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate(timeout=10)
        return dict(command=command, exit=process.returncode, stdout=stdout, stderr=stderr, timed_out=True)
    return dict(command=command, exit=process.returncode, stdout=stdout, stderr=stderr)


def allocation(root, args, result):
    if (args.service or args.workspace or args.file_service) and os.name != 'posix': raise RuntimeError('service/workspace fixture requires Unix')
    cases = 'system_workspace_allocation_cases.rs' if args.workspace else 'file_service_allocation_cases.rs' if args.file_service else 'service_allocation_cases.rs' if args.service else 'executor_allocation_cases.rs'
    fixtures = [root / 'tools/fixtures' / name for name in ['executor_allocation_meter.rs', cases]]
    result['fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    result['scope'] = 'Temporary-copy process-wide Rust requested-byte meter, each allocation carries an epoch header so cross-thread frees and preexisting objects are distinguished. Counts conservative old/new realloc overlap, excludes added meter headers/allocator/C/OS internals and actual stacks/RSS. Actual 2-worker/64-wait/2-result executor and its reaper, newly created threads/TLS, anchored pool, 2x1MiB active payloads, 64x16KiB queued payloads, cancellation, joined cleanup, and 64KiB result retained after pool drop. Parent Budget/notify/test controls and first standard-library thread global initialization are preinitialized. Stack values in the ledger are reservations, not measured commitment. No SSH/SFTP/window/network.'
    if args.service:
        result['scope'] = 'Temporary-copy process-wide epoch-tagged Rust requested-byte meter, conservative old/new realloc overlap. Actual typed Service DTOs, independent input ceiling protecting 9MiB IO, fixed executor/three new threads/TLS/anchored pool; two blocked requests and quota-bounded queued requests each with maximum escaped remote path (16384 raw invalid-UTF8 bytes), 4096-byte helper/config paths and maximum admitted profile fields. One quota-rejected input, atomic source cancellation, joined executor, mailboxes retained after Service drop, then final credit release. Profile snapshot, parent/notify/test buffers and first thread-global initialization preinitialized. Fake work prevents all endpoint/process/network IO; this proves bounded DTO admission/lifetime, not actual SSH/C/allocator/OS/stack/RSS or GUI cost.'
    if args.workspace:
        result['scope'] = 'Temporary-copy all-thread epoch-tagged Rust requested-byte meter. Local picker reads 512 generated entries through the existing SFTP workers with no auth/new directory thread, retains a real bounded listing, then 200 direct warm paints. A synthetic failed-cleanup snapshot of the real completed records measures the actual exit warning renderer and 200 warm direct paints; real cleanup failure and acknowledged exit are verified separately by the isolated SSH Workspace fixture. Manual retry rechecks a 32MiB local source, retains the old record and requires fresh non-overwrite review; 200 direct warm retry-review paints allocate nothing. Existing local PTY, Workspace initial canvas/renderer/profile snapshot and first executor thread-global initialization prewarmed outside metering. Measures Workspace-owned Service/new three threads/TLS/options/pending/view, failing directory request with nonexistent endpoint parent (no socket/SSH), then a synthetic absolute directory and maximum 16KiB/1024-byte path form plus 1023-byte preedit, 200 warm direct form paints with allocation counters and cancelled late 16KiB paste, then an unknown-attributes download form (16KiB local path/1023-byte preedit) with an actual inspect_remote request that fails before socket creation at the nonexistent endpoint parent, 200 warm error-form paints and cancellation, then actual worker-only 32MiB sparse local source lstat/preparation -> checked review/layout -> Esc (no transfer), then 66 sequential actual small-path file DTOs fail before endpoint creation and their completed Status/path records remain in the fixed table (67th refused), then maximum 16KiB raw-invalid-byte local/remote path review with bounded text/index, actual software layout/draw, 200 warm direct review paints with allocation counters and Esc cancel (no new file job), plus synthetic same-child unclassified Backend/Front through actual Workspace protected input/controller and software canvas with maximum 16KiB escaping prompt/1023-byte answer. This synthetic modal is not from the directory task. Close gate joins actual Service, then Workspace/backend dropped while independently retained InputContext/late ProtectedInput keep child parent credit. Final tracked requested bytes and SFTP parent reservation zero, also wait original PTY managed worker native acknowledgements, zero active common-runtime records and zero root usage; test-only session isolation synchronization is initialized with the preexisting Session before epoch; root_total includes separate existing/application resources, not a claim that every UI allocation is individually charged to child. Conservative realloc overlap, excludes meter headers/allocator/C/ObjC/OS/actual stacks/RSS, native window/IME/network/transfer and latency. No production allocator changes.'
    if args.file_service:
        result['scope'] = 'Temporary-copy all-thread epoch requested-byte meter, conservative old/new realloc overlap; excludes meter headers/allocator/C/OS internals and actual stacks/RSS. Actual Service transfer Action/Output DTOs, unique fixed 2-worker/64-wait/16MiB executor/new three threads/TLS, anchored parent lifetime. Maximum admitted SSH profile fields, 4096-byte helper/config, exact maximum 16KiB escaped local+remote paths shared by Status, active two funded worker-generated random remote partial candidates, alternating upload/download requests until quota or queue refuses, source cancellation, confirmed joined shutdown, independent status held after Service/auth/input drop, final zero. Root/profile/input buffers/notify/thread global initialization preinitialized. Inputs are separately capped to protect 9MiB IO; after metadata quota refusal, an actual Rust byte buffer fills the remaining IO allowance while two active partial candidates stay funded, then releases. Fake work only creates bounded random path candidates; no file/SSH/endpoint/network/GUI/real streaming allocation or latency proof.'
    with tempfile.TemporaryDirectory(prefix='congminal-executor-allocation-') as temporary:
        copy = Path(temporary)
        for name in ['src', 'assets']:
            shutil.copytree(root / name, copy / name)
        for name in ['Cargo.toml', 'Cargo.lock']:
            shutil.copy2(root / name, copy / name)
        shutil.copy2(fixtures[0], copy / 'src/executor_allocation_meter.rs')
        main = copy / 'src/main.rs'
        main.write_text(main.read_text() + '\n#[cfg(test)] mod executor_allocation_meter;\n')
        executor = copy / ('src/workspace/remote_files.rs' if args.workspace else 'src/sftp/service.rs' if args.service or args.file_service else 'src/sftp/executor.rs')
        executor.write_text(executor.read_text() + '\n' + fixtures[1].read_text())
        result['rustc'] = run(prefix + ['rustc', '-Vv'], copy, 15)
        # 新工具链曾在内联带头计量器下改变临时String匹配；先验证原快捷键行为。
        result['sanity'] = run(prefix + ['cargo', 'test', '--offline', '--release', 'action::tests', '--', '--nocapture', '--test-threads=1'], copy, 180)
        if result['sanity']['exit']: raise RuntimeError(result['sanity']['stdout'] + result['sanity']['stderr'])
        test = WORKSPACE_ALLOCATION if args.workspace else FILE_SERVICE_ALLOCATION if args.file_service else SERVICE_ALLOCATION if args.service else ALLOCATION
        result['measurement'] = run(prefix + ['cargo', 'test', '--offline', '--release', test,
                                              '--', '--exact', '--nocapture', '--test-threads=1'], copy, 180)
        log = result['measurement']['stdout'] + result['measurement']['stderr']
        keys = ['live', 'peak', 'parent_used', 'parent_peak', 'child_used', 'allocations', 'reallocations']
        tag = 'WORKSPACE_ALLOC' if args.workspace else 'FILE_SERVICE_ALLOC' if args.file_service else 'SERVICE_ALLOC' if args.service else 'EXECUTOR_ALLOC'
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(
            tag + r' step=(\w+) live=(-?\d+) peak=(-?\d+) parent_used=(\d+) parent_peak=(\d+) child_used=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        if args.workspace:
            assert 'WORKSPACE_NATIVE managed_workers=0 root_used=0 active=0' in log
            lookup = re.search(r'WORKSPACE_LOOKUP live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) paint_allocations=(\d+) paint_reallocations=(\d+)',log)
            if not lookup: raise RuntimeError('missing attribute lookup measurement')
            result['lookup'] = dict(zip(['live','peak','child_used','input_used','paint_allocations','paint_reallocations'],map(int,lookup.groups())))
            if int(lookup[5]) or int(lookup[6]): raise RuntimeError('warm lookup error form paint allocates')
            paths = re.search(r'WORKSPACE_PATHS live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) paint_allocations=(\d+) paint_reallocations=(\d+)',log)
            if not paths: raise RuntimeError('missing maximum path form measurement')
            result['paths'] = dict(zip(['live','peak','child_used','input_used','paint_allocations','paint_reallocations'],map(int,paths.groups())))
            if int(paths[5]) or int(paths[6]): raise RuntimeError('warm direct path form paint allocates')
            picker = re.search(r'WORKSPACE_PICKER live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) entries=512 paint_allocations=(\d+) paint_reallocations=(\d+)', log)
            if not picker: raise RuntimeError('missing local picker measurement')
            result['local_picker'] = dict(zip(['live','peak','child_used','input_used','paint_allocations','paint_reallocations'], map(int, picker.groups())))
            if int(picker[5]) or int(picker[6]): raise RuntimeError('warm local picker paint allocates')
            times = re.search(r'WORKSPACE_PICKER_TIMES ns=(\[[^\n]+\])', log)
            if not times: raise RuntimeError('missing local picker paint times')
            raw = json.loads(times[1])
            if len(raw) != 200 or any(type(n) is not int or n < 0 for n in raw): raise RuntimeError('invalid local picker timing samples')
            ordered = sorted(raw)
            result['local_picker']['paint_timing'] = dict(scope='Direct warmed software modal paint under requested-byte instrumentation, Instant overhead and scheduler included; not native input or frame presentation latency. Nearest-rank percentiles.', samples_ns=raw, p50_ns=ordered[99], p95_ns=ordered[189], p99_ns=ordered[197], maximum_ns=ordered[-1])
            exit_view = re.search(r'WORKSPACE_EXIT live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) paint_allocations=(\d+) paint_reallocations=(\d+)', log)
            if not exit_view: raise RuntimeError('missing shutdown warning render measurement')
            result['exit_view'] = dict(zip(['live','peak','child_used','input_used','paint_allocations','paint_reallocations'], map(int, exit_view.groups())))
            if int(exit_view[5]) or int(exit_view[6]): raise RuntimeError('warm shutdown warning paint allocates')
            retry = re.search(r'WORKSPACE_RETRY live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) paint_allocations=(\d+) paint_reallocations=(\d+)', log)
            if not retry: raise RuntimeError('missing manual retry measurement')
            result['retry'] = dict(zip(['live','peak','child_used','input_used','paint_allocations','paint_reallocations'], map(int, retry.groups())))
            if int(retry[5]) or int(retry[6]): raise RuntimeError('warm retry review paint allocates')
            prepare = re.search(r'WORKSPACE_PREPARE live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) length=33554432',log)
            if not prepare: raise RuntimeError('missing source preparation measurement')
            result['preparation'] = dict(zip(['live','peak','child_used','input_used'],map(int,prepare.groups())))
            review = re.search(r'WORKSPACE_REVIEW live=(\d+) peak=(\d+) child_used=(\d+) input_used=(\d+) paint_allocations=(\d+) paint_reallocations=(\d+)',log)
            if not review: raise RuntimeError('missing maximum review measurement')
            result['review'] = dict(zip(['live','peak','child_used','input_used','paint_allocations','paint_reallocations'],map(int,review.groups())))
            if int(review[5]) or int(review[6]): raise RuntimeError('warm direct review paint allocates')
            totals = list(map(int, re.findall(r'WORKSPACE_ALLOC[^\n]* root_total=(\d+)', log)))
            for row, total in zip(result['samples'], totals): row['root_total'] = total
        if args.file_service:
            match = re.search(r'FILE_SERVICE_QUEUE admitted=(\d+) running=2 queued=(\d+) reason=(budget|full) limit=16777216', log)
            if match:
                result['queue'] = dict(admitted=int(match[1]), queued=int(match[2]), reason=match[3])
            if not match or int(match[1]) != int(match[2])+2 or not 2<int(match[1])<=66:
                raise RuntimeError(log[-12000:])
        if args.file_service:
            match = re.search(r'FILE_SERVICE_INPUT input=(\d+) input_limit=(\d+) io_headroom=(\d+) io_fill=(\d+)',log)
            if not match: raise RuntimeError(log[-12000:])
            result['input'] = dict(zip(['used','limit','io_headroom','io_fill'],map(int,match.groups())))
            if int(match[1])>int(match[2]) or int(match[3])!=9*1024*1024: raise RuntimeError('input/IO quota mismatch')
            sizes = re.search(r'FILE_SERVICE_TYPES budget=(\d+) lease=(\d+) stats=(\d+) input_scope=(\d+) input_slots=(\d+)',log)
            if not sizes: raise RuntimeError('missing type sizes')
            result['type_sizes'] = dict(zip(['budget','lease','stats','input_scope','input_slots'],map(int,sizes.groups())))
        if result['measurement']['exit'] or len(result['samples']) != (7 if args.workspace or args.file_service else 6 if args.service else 10):
            raise RuntimeError(log[-12000:])
        if any(s['live'] < 0 or s['live'] > s['parent_used'] or s['peak'] > s['parent_peak'] for s in result['samples']):
            raise RuntimeError('unbacked requested bytes')
        if result['samples'][-1]['live'] or result['samples'][-1]['parent_used']:
            raise RuntimeError('final allocations or parent reservation are not zero')
        print(json.dumps({'samples': result['samples']}), flush=True)


def cost_case(root, binary, mode, sample):
    env = dict(os.environ, CONGMINAL_EXECUTOR_MODE=mode)
    process = subprocess.Popen([binary, COST, '--exact', '--ignored', '--nocapture', '--test-threads=1'],
                               cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, encoding='utf-8', errors='replace', start_new_session=True)
    lines, phases, samples = [], {}, []
    lock = threading.Lock()
    too_much = threading.Event()
    def read():
        length = 0
        for line in process.stdout:
            with lock:
                length += len(line)
                if length > 256 * 1024:
                    too_much.set()
                    continue
                lines.append(line)
                if re.search(r'\bEXECUTOR_IDLE_START mode=', line): phases['start'] = time.monotonic_ns()
                if re.search(r'\bEXECUTOR_IDLE_END mode=', line): phases['end'] = time.monotonic_ns()
    reader = threading.Thread(target=read)
    reader.start()
    started = time.monotonic()
    timed_out = False
    try:
        while process.poll() is None:
            if sample:
                samples.append(dict(ns=time.monotonic_ns(), tree=sample(process.pid)))
            if time.monotonic() - started > 20 or too_much.is_set():
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                break
            time.sleep(.005)
        process.wait(timeout=5)
        reader.join(timeout=5)
        if reader.is_alive(): raise RuntimeError('output reader did not finish')
        log = ''.join(lines)
        result = dict(mode=mode, exit=process.returncode, log=log, samples=samples, phases=phases, timed_out=timed_out)
        match = re.search(r'EXECUTOR_COST mode=(baseline|pool) (.+)', log)
        if not match or match[1] != mode or process.returncode or timed_out:
            raise RuntimeError(log[-10000:])
        result['cost'] = {key: int(value) for key, value in re.findall(r'(\w+)=(\d+)', match[2])}
        raw = re.search(r'EXECUTOR_SUBMIT_SAMPLES ns=([\d,]+)', log)
        result['cost']['sorted_submit_samples_ns'] = list(map(int, raw[1].strip(',').split(','))) if raw else []
        if len(result['cost']['sorted_submit_samples_ns']) != 500 or result['cost']['final_used']:
            raise RuntimeError('missing samples or unreleased budget')
        if set(phases) != {'start', 'end'} or phases['end']-phases['start'] < 400_000_000:
            raise RuntimeError('missing or invalid idle phase boundaries')
        stable = [s for s in samples if phases['start']+50_000_000 <= s['ns'] <= phases['end']-50_000_000 and process.pid in s['tree']]
        if sample and len(stable) < 3: raise RuntimeError('insufficient idle samples')
        result['idle_samples'] = len(stable)
        result['idle_cpu_percent_single_core'] = None
        result['idle_rss_median'] = None
        result['idle_threads_median'] = None
        if len(stable) > 2:
            first, last = stable[0], stable[-1]
            cpu = last['tree'][process.pid]['cpu_ns'] - first['tree'][process.pid]['cpu_ns']
            result['idle_cpu_percent_single_core'] = 100*cpu/(last['ns']-first['ns'])
            result['idle_rss_median'] = statistics.median(s['tree'][process.pid]['rss'] for s in stable)
            result['idle_threads_median'] = statistics.median(s['tree'][process.pid]['threads'] for s in stable)
        return result
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        reader.join(timeout=5)


def cost(root, args, result):
    if args.toolchain: raise RuntimeError('--toolchain is for isolated allocation measurement only')
    result['scope'] = 'Actual System allocator, exact isolated Rust test, no allocation meter. Two warmups per mode, five alternating pairs of 500ms no-pool idle control and live fixed executor idle. Baseline has no executor/jobs and is NOT an equivalent functional alternative; no throughput-regression or replacement claim. Pool mode separately measures 500 sequential admitted primitive jobs, an atomic 2-running+64-waiting source cancel, shutdown call, completion polling and joined cleanup. Includes Rust harness, no network/SSH/SFTP/GUI/native input. Stable libproc samples trim 50ms per edge; short idle observations are not 30-minute or full-RSS proof.'
    build = run(['cargo', 'test', '--offline', '--release', '--no-run', '--message-format=json'], root, 180)
    result['build'] = dict(exit=build['exit'], stderr=build['stderr'])
    if build['exit']: raise RuntimeError(build['stderr'][-12000:])
    artifacts = [json.loads(line) for line in build['stdout'].splitlines() if line.startswith('{')]
    binary = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target', {}).get('name') == 'congminal' and a.get('executable'))
    path = Path(binary)
    result['binary'] = dict(path=str(path), bytes=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest())
    sample, result['sampler'] = sampler()
    result['sampler_tool_sha256'] = hashlib.sha256((root/'tools/check_ssh_transport.py').read_bytes()).hexdigest()
    result['warmups'], result['runs'] = [], []
    for warm in range(2):
        for mode in ['baseline', 'pool']:
            record = cost_case(root, binary, mode, sample)
            result['warmups'].append(record)
            print(json.dumps({'warmup': warm, 'mode': mode, 'exit': record['exit']}), flush=True)
    for pair in range(5):
        for mode in (['baseline', 'pool'] if pair % 2 == 0 else ['pool', 'baseline']):
            record = cost_case(root, binary, mode, sample)
            record['pair'] = pair
            result['runs'].append(record)
            print(json.dumps({'pair': pair, 'mode': mode, 'exit': record['exit'], 'idle_cpu': record['idle_cpu_percent_single_core'], 'idle_threads': record['idle_threads_median']}), flush=True)
    result['summary'] = {}
    for mode in ['baseline', 'pool']:
        rows = [r for r in result['runs'] if r['mode'] == mode]
        fields = ['idle_cpu_percent_single_core', 'idle_rss_median', 'idle_threads_median']
        data = {field: [r[field] for r in rows if r[field] is not None] for field in fields}
        if mode == 'pool':
            for field in ['start_us', 'parent_reserved', 'runtime', 'cancel_66_ns', 'shutdown_call_ns', 'poll_max_ns', 'joined_us', 'submit_p50_ns', 'submit_p95_ns', 'submit_p99_ns', 'submit_max_ns']:
                data[field] = [r['cost'][field] for r in rows]
        result['summary'][mode] = {field: dict(median=statistics.median(values), min=min(values), max=max(values)) if values else None for field, values in data.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--allocation', action='store_true')
    mode.add_argument('--cost', action='store_true')
    parser.add_argument('--toolchain')
    target = parser.add_mutually_exclusive_group()
    target.add_argument('--service', action='store_true', help='allocation mode: measure maximum typed request queue instead of primitive executor payloads')
    target.add_argument('--workspace', action='store_true', help='allocation mode: measure Workspace-owned service/modal and late protected input lifecycle')
    target.add_argument('--file-service', action='store_true', help='allocation mode: measure actual typed transfer requests, bounded admission and late report lifetime')
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    if (args.service or args.workspace or args.file_service) and not args.allocation: parser.error('service/workspace modes require --allocation')
    root = Path(__file__).resolve().parent.parent
    def sources(): return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}
    result = dict(platform=platform.platform(), source_sha256=sources(), tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']})
    try:
        (allocation if args.allocation else cost)(root, args, result)
        if sources() != result['source_sha256']: raise RuntimeError('source changed during measurement')
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2)+'\n')

if __name__ == '__main__': main()
