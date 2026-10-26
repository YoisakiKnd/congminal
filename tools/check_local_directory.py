#!/usr/bin/env python3
"""本地目录隔离候选：真实 helper、故障回归及五组成功负载成本；不接入默认后端。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import statistics
import subprocess
import threading
import time
from check_sftp_executor import run
from check_ssh_transport import sampler

PREFIX = 'local_directory::isolated::tests::'


def trial(root, binary, helper, mode, sample):
    env = dict(os.environ, CONGMINAL_DIRECTORY_HELPER=str(helper), CONGMINAL_DIRECTORY_MODE=mode)
    process = subprocess.Popen([str(binary), PREFIX+'isolated_directory_batch_cost', '--exact', '--ignored',
                                '--nocapture', '--test-threads=1'], cwd=root, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                               encoding='utf-8', errors='replace', start_new_session=True)
    lines, samples, phases = [], [], {}
    overflow = threading.Event()
    def read():
        size = 0
        for line in process.stdout:
            size += len(line)
            if size > 128*1024:
                overflow.set()
                continue
            lines.append(line)
            if 'DIRECTORY_BATCH_START mode=' in line: phases['start'] = time.monotonic_ns()
            if 'DIRECTORY_BATCH_END mode=' in line: phases['end'] = time.monotonic_ns()
    reader = threading.Thread(target=read)
    reader.start()
    started = time.monotonic_ns()
    record = dict(mode=mode, samples=samples, phases=phases)
    try:
        while process.poll() is None:
            if sample: samples.append(dict(ns=time.monotonic_ns(), tree=sample(process.pid)))
            if time.monotonic_ns()-started > 30_000_000_000 or overflow.is_set():
                raise RuntimeError('owned cost fixture timeout/output cap')
            time.sleep(.005)
        reader.join(timeout=5)
        if reader.is_alive(): raise RuntimeError('owned output reader did not end')
        log = ''.join(lines)
        record.update(exit=process.returncode, log=log)
        match = re.search(r'DIRECTORY_BATCH mode=(direct|isolated) count=50 entries=512 total_ns=(\d+) peak=(\d+) final=0', log)
        if process.returncode or not match or match[1] != mode or set(phases) != {'start','end'}:
            raise RuntimeError(log[-8000:])
        record.update(total_ns=int(match[2]), budget_peak=int(match[3]), count=50, entries=512)
        active = [s for s in samples if phases['start'] <= s['ns'] <= phases['end']]
        record['phase_samples'] = len(active)
        record['parent_rss_peak_observed'] = max((s['tree'].get(process.pid,{}).get('rss',0) for s in active), default=0) or None
        record['children_rss_peak_observed'] = max((sum(v['rss'] for p,v in s['tree'].items() if p != process.pid) for s in active), default=0) or None
        record['tree_rss_peak_observed'] = max((sum(v['rss'] for v in s['tree'].values()) for s in active), default=0) or None
        record['max_simultaneous_children_observed'] = max((max(0,len(s['tree'])-1) for s in active), default=0)
        cpu = {}
        for s in samples:
            for p,v in s['tree'].items(): cpu[p] = max(cpu.get(p,0),v['cpu_ns'])
        record['whole_trial_sampled_cpu_ns_lower_bound'] = sum(cpu.values()) if sample else None
        return record
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        reader.join(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--toolchain')
    parser.add_argument('--runtime-only', action='store_true')
    parser.add_argument('--minimal-helper', action='store_true', help='build the same shared protocol/reader without linking GUI modules')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    def sources(): return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}
    prefix = ['rustup','run',args.toolchain] if args.toolchain else []
    result = dict(scope='Same successful 512-entry local directory model, hidden entries included, all raw names/types/labels and sorted completeness compared. Direct scan versus one supervised same-binary process per query, existing binary pipe/cancel/cleanup adapter, no GUI/SSH or pool integration. Both retain one prewarmed expected listing outside timing. Two process trials per mode warm up; five alternating measured pairs, 50 queries per trial after two unmeasured queries. Generated private files, no payload read. Parent and owned child RSS separately sampled every5ms; sums may double-count shared pages and miss short helper peaks. Per-PID observed CPU maxima are a lower bound over whole trial including harness/setup, NOT full CPU accounting or isolated query CPU. Direct scan lacks process cancellation and is not a fault-equivalent alternative. No native window, whole-process requested-byte proof, stuck kernel filesystem, Windows runtime, long run or default adoption claim.',
                  platform=platform.platform(), source_sha256=sources(), tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}, adopted=False)
    try:
        result['rustc'] = run(prefix+['rustc','-Vv'],root,15)
        result['helper_build'] = run(prefix+['cargo','build','--offline','--release','--features','directory-helper','--bin','congminal'],root,180)
        if result['helper_build']['exit']: raise RuntimeError(result['helper_build']['stderr'])
        helper = root/'target/release/congminal'
        if args.minimal_helper:
            helper = root/'target/release/congminal-directory-candidate'
            result['minimal_build'] = run(prefix+['rustc','--edition=2024','--cfg','feature="directory-helper"','-C','opt-level=3','-C','debuginfo=1',str(root/'src/local_directory/helper_main.rs'),'-o',str(helper)],root,60)
            if result['minimal_build']['exit']: raise RuntimeError(result['minimal_build']['stderr'])
        result['helper_kind'] = 'minimal shared-reader prototype' if args.minimal_helper else 'full application in internal mode'
        result['helper'] = dict(bytes=helper.stat().st_size,sha256=hashlib.sha256(helper.read_bytes()).hexdigest())
        build = run(prefix+['cargo','test','--offline','--release','--no-run','--message-format=json'],root,180)
        artifacts = [json.loads(line) for line in build['stdout'].splitlines() if line.startswith('{')]
        result['test_build'] = dict(exit=build['exit'],stderr=build['stderr'],diagnostics=[a['message'].get('rendered','') for a in artifacts if a.get('reason')=='compiler-message'])
        if build['exit']: raise RuntimeError(build['stderr'])
        binary = Path(next(a['executable'] for a in artifacts if a.get('reason')=='compiler-artifact' and a.get('target',{}).get('name')=='congminal' and a.get('executable')))
        result['unit'] = run([str(binary),'local_directory::','--nocapture','--test-threads=1'],root,15)
        if result['unit']['exit']: raise RuntimeError(result['unit']['stdout']+result['unit']['stderr'])
        env = dict(os.environ,CONGMINAL_DIRECTORY_HELPER=str(helper))
        p = subprocess.run([str(binary),PREFIX+'actual_helper_preserves_metadata_matches_direct_reader_and_five_paired_costs','--exact','--ignored','--nocapture','--test-threads=1'],cwd=root,env=env,capture_output=True,text=True,timeout=30)
        result['actual_helper'] = dict(exit=p.returncode,stdout=p.stdout,stderr=p.stderr)
        rounds = re.findall(r'DIRECTORY_ISOLATED round=(\d+) isolated_ns=(\d+) direct_ns=(\d+) peak=(\d+) final=0',p.stdout+p.stderr)
        if p.returncode or len(rounds)!=7: raise RuntimeError(p.stdout+p.stderr)
        result['same_process_pairs'] = [dict(zip(['round','isolated_ns','direct_ns','budget_peak'],map(int,row))) for row in rounds]
        if not args.runtime_only:
            sample,result['sampler'] = sampler()
            result['sampler_tool_sha256'] = hashlib.sha256((root/'tools/check_ssh_transport.py').read_bytes()).hexdigest()
            result['warmups'],result['runs'] = [],[]
            for i in range(2):
                for mode in ['direct','isolated']: result['warmups'].append(trial(root,binary,helper,mode,sample))
            for pair in range(5):
                for mode in (['direct','isolated'] if pair%2==0 else ['isolated','direct']):
                    row = trial(root,binary,helper,mode,sample);row['pair']=pair;result['runs'].append(row)
                    print(json.dumps(dict(pair=pair,mode=mode,ns=row['total_ns'],children_rss_observed=row['children_rss_peak_observed'])),flush=True)
            pairs=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['pair']==pair and r['mode']==mode) for mode in ['direct','isolated']]
                pairs.append(dict(pair=pair,added_ns_per_query=(b['total_ns']-a['total_ns'])/50,relative_percent=100*(b['total_ns']/a['total_ns']-1)))
            result['paired_summary'] = {key:dict(median=statistics.median(p[key] for p in pairs),minimum=min(p[key] for p in pairs),maximum=max(p[key] for p in pairs)) for key in ['added_ns_per_query','relative_percent']}
            result['pairs'] = pairs
        if sources()!=result['source_sha256']: raise RuntimeError('source changed during measurement')
        result['passed'] = True
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__ == '__main__': main()
