#!/usr/bin/env python3
"""Unix传输文件拥有者：临时全线程请求计量，或无计量器的32MiB成功发布成本。"""
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
from check_sftp_executor import identity, run
from check_ssh_transport import sampler

ALLOC = 'sftp::local_transfer::allocation_probe::measured_local_file_streaming_and_retained_partial_paths_have_constant_working_memory'
COST = 'sftp::local_transfer::tests::isolated_local_publish_cost'

def allocations(root, args, result):
    fixtures = [root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs', 'local_transfer_allocation_cases.rs', 'local_transfer_cold_lock.rs']]
    result['fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    result['scope'] = 'Temporary-copy all-thread epoch-tagged Rust requested-byte meter; conservative realloc old/new overlap, excludes added headers/allocator/C ACL/OS/actual stack/RSS. Existing input file, path owners, parent Budget/cancel/test buffers and application process-launch global mutex initialized before metering. Cold global process-launch metadata is a separate M4 gap; this incremental native-file scope does not close it. Actual native Source/private Download, stream 4MiB and 32MiB through fixed 64KiB caller stack buffer with 256KiB quota, file sync, no-replace atomic publish, explicit native closes, independent temporary-path owner retained after objects close, final tracked requested bytes and budget zero. No SSH/network/workspace/executor/window/transfer DTO or latency claim. This covers Rust ownership only; does not account all C/OS allocations or file-cache growth.'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='congminal-local-allocation-') as temporary:
        copy = Path(temporary)
        result['cold_lock_build'] = run(prefix+['rustc','--edition=2024','-O',str(fixtures[2]),'-o',str(copy/'cold-lock')],root,30)
        if result['cold_lock_build']['exit']: raise RuntimeError(result['cold_lock_build']['stderr'])
        result['cold_lock'] = run([str(copy/'cold-lock')],root,10)
        if result['cold_lock']['exit'] or not re.search(r'LOCAL_COLD_LOCK live=\d+ peak=\d+ allocations=\d+ reallocations=\d+',result['cold_lock']['stdout']): raise RuntimeError('cold global lock diagnostic failed')
        for name in ['src','assets']: shutil.copytree(root/name, copy/name)
        for name in ['Cargo.toml','Cargo.lock']: shutil.copy2(root/name, copy/name)
        shutil.copy2(fixtures[0], copy/'src/executor_allocation_meter.rs')
        main = copy/'src/main.rs'; main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        target = copy/'src/sftp/local_transfer.rs'; target.write_text(target.read_text()+'\n'+fixtures[1].read_text())
        result['rustc'] = run(prefix+['rustc','-Vv'], copy, 15)
        result['sanity'] = run(prefix+['cargo','test','--offline','--release','action::tests','--','--test-threads=1'], copy, 180)
        if result['sanity']['exit']: raise RuntimeError(result['sanity']['stdout']+result['sanity']['stderr'])
        result['measurement'] = run(prefix+['cargo','test','--offline','--release',ALLOC,'--','--exact','--nocapture','--test-threads=1'], copy, 180)
        log = result['measurement']['stdout']+result['measurement']['stderr']
        fields = ['bytes','live','peak','used','budget_peak','allocations','reallocations']
        result['samples'] = [dict(step=m[1], **dict(zip(fields,map(int,[m[0]]+list(m[2:]))))) for m in re.findall(r'LOCAL_ALLOC bytes=(\d+) step=(\w+) live=(-?\d+) peak=(-?\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        if result['measurement']['exit'] or len(result['samples'])!=16: raise RuntimeError(log[-12000:])
        for size in [4*1024*1024,32*1024*1024]:
            samples = [s for s in result['samples'] if s['bytes']==size]
            if len(samples)!=8 or any(s['live']<0 or s['live']>s['used'] or s['peak']>s['budget_peak'] for s in samples) or samples[-1]['live'] or samples[-1]['used']: raise RuntimeError('unbacked allocations or final credit not zero')
        print(json.dumps({'samples':result['samples']}),flush=True)

def cost_case(root, binary, mode, sample):
    env = dict(os.environ, CONGMINAL_LOCAL_MODE=mode)
    process = subprocess.Popen([binary,COST,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf-8',errors='replace',start_new_session=True)
    lines, phases, samples = [], {}, []
    lock = threading.Lock()
    overflow = threading.Event()
    def reader():
        size = 0
        for line in process.stdout:
            with lock:
                size += len(line)
                if size>128*1024: overflow.set(); continue
                lines.append(line)
                if re.search(r'\bLOCAL_IO_START mode=',line): phases['start']=time.monotonic_ns()
                if re.search(r'\bLOCAL_IO_END mode=',line): phases['end']=time.monotonic_ns()
    thread=threading.Thread(target=reader,daemon=True);thread.start();started=time.monotonic_ns()
    try:
        while process.poll() is None:
            samples.append({'ns':time.monotonic_ns(),'tree':sample(process.pid)})
            if overflow.is_set() or time.monotonic_ns()-started>30_000_000_000:
                os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5);raise RuntimeError('owned cost fixture timeout/output limit')
            time.sleep(0.005)
        thread.join(timeout=5)
        if thread.is_alive(): raise RuntimeError('cost reader did not finish')
        log=''.join(lines)
        match=re.search(r'LOCAL_COST mode=(\w+) bytes=(\d+) total_ns=(\d+) create_ns=(\d+) write_ns=(\d+) sync_ns=(\d+) commit_ns=(\d+) close_ns=(\d+) peak=(\d+) final=0',log)
        record=dict(mode=mode,exit=process.returncode,log=log,phases=phases,samples=samples)
        if process.returncode or not match or match[1]!=mode or set(phases)!={'start','end'} or phases['end']<=phases['start']: raise RuntimeError(log[-12000:])
        keys=['bytes','total_ns','create_ns','write_ns','sync_ns','commit_ns','close_ns','budget_peak']
        record['cost']=dict(zip(keys,map(int,match.groups()[1:])))
        # Sampler returns numeric PIDs; serialize normalizes keys only when saving JSON.
        stable=[s for s in samples if phases['start']<=s['ns']<=phases['end'] and process.pid in s['tree']]
        record['phase_samples']=len(stable)
        record['phase_rss_median']=statistics.median(s['tree'][process.pid]['rss'] for s in stable) if stable else None
        record['phase_threads_median']=statistics.median(s['tree'][process.pid]['threads'] for s in stable) if stable else None
        record['phase_cpu_percent_single_core']=None
        if len(stable)>=3:
            a,b=stable[0],stable[-1];record['phase_cpu_percent_single_core']=100*(b['tree'][process.pid]['cpu_ns']-a['tree'][process.pid]['cpu_ns'])/(b['ns']-a['ns'])
        return record
    finally:
        if process.poll() is None: os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
        thread.join(timeout=5)

def costs(root,args,result):
    if args.toolchain: raise RuntimeError('--toolchain only applies to allocation')
    result['scope']='Actual System allocator with no meter; two warmups per mode then five alternating pairs. Same 32MiB constant content, private same-directory stage, 0600 file, 64KiB writes, file sync, no-replace link publication, stage cleanup, parent-dir sync and close. Baseline std filesystem happy-path control lacks FD anchoring/replacement checks/checked-close contract and is NOT a fault-equivalent safe alternative. Measures common successful file-component costs, no SSH/network/workspace/thread pool/GUI/presentation; no SFTP regression or package-adoption claim. Native process sampler at 5ms may have too few phase samples for CPU; missing CPU stays null. Includes Rust harness, ignores filesystem cache effects outside process RSS, not a long idle/pressure test.'
    build=run(['cargo','test','--offline','--release','--no-run','--message-format=json'],root,180)
    artifacts=[json.loads(line) for line in build['stdout'].splitlines() if line.startswith('{')]
    result['build']=dict(exit=build['exit'],stderr=build['stderr'],diagnostics=[a['message'].get('rendered','') for a in artifacts if a.get('reason')=='compiler-message'])
    if build['exit']: raise RuntimeError(''.join(result['build']['diagnostics'])+build['stderr'])
    binary=Path(next(a['executable'] for a in artifacts if a.get('reason')=='compiler-artifact' and a.get('target',{}).get('name')=='congminal' and a.get('executable')))
    result['binary']=dict(path=str(binary),bytes=binary.stat().st_size,sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    sample,result['sampler']=sampler();result['sampler_tool_sha256']=hashlib.sha256((root/'tools/check_ssh_transport.py').read_bytes()).hexdigest()
    result['warmups'],result['runs']=[],[]
    for i in range(2):
        for mode in ['baseline','wrapped']:
            record=cost_case(root,binary,mode,sample);result['warmups'].append(record);print(json.dumps({'warmup':i,'mode':mode,'exit':record['exit']}),flush=True)
    for pair in range(5):
        for mode in (['baseline','wrapped'] if pair%2==0 else ['wrapped','baseline']):
            record=cost_case(root,binary,mode,sample);record['pair']=pair;result['runs'].append(record);print(json.dumps({'pair':pair,'mode':mode,'ns':record['cost']['total_ns']}),flush=True)
    result['summary']={}
    for mode in ['baseline','wrapped']:
        rows=[r for r in result['runs'] if r['mode']==mode]
        result['summary'][mode]={field:dict(median=statistics.median(r['cost'][field] for r in rows),min=min(r['cost'][field] for r in rows),max=max(r['cost'][field] for r in rows)) for field in rows[0]['cost']}

def main():
    parser=argparse.ArgumentParser(description=__doc__);mode=parser.add_mutually_exclusive_group(required=True);mode.add_argument('--allocation',action='store_true');mode.add_argument('--cost',action='store_true');parser.add_argument('--toolchain');parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    root=Path(__file__).resolve().parent.parent
    def sources():return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}
    result=dict(platform=platform.platform(),source_sha256=sources(),tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},helper_tool_sha256=hashlib.sha256((root/'tools/check_sftp_executor.py').read_bytes()).hexdigest())
    try:
        (allocations if args.allocation else costs)(root,args,result)
        if sources()!=result['source_sha256']: raise RuntimeError('source changed during measurement')
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
