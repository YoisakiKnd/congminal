#!/usr/bin/env python3
"""Managed LatestTask worker/supervisor admission and native exit, allocations or same-load costs."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
import threading
import time
from check_font_discovery import build,sources
from check_sftp_executor import run,identity
from check_ssh_transport import sampler
from check_ui_state import parse as parse_ui,allocation as ui_allocation


def project(root,p):
    for n in ['src','assets']:shutil.copytree(root/n,p/n)
    for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(root/n,p/n)


def allocation(root,args,result):
    fixtures=[root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs','system_thread_allocation_cases.rs']]
    result['fixtures']={str(p.relative_to(root)):identity(p) for p in fixtures}
    result['scope']='All-thread epoch Rust request meter in temporary copy. Private new supervisor/worker/first synchronization/TLS/header allocations through native worker and supervisor join, core retained then final0; independently actual LatestTask core dies before blocked native TLS, runtime fee stays, finally0. Parent budget/context and test-control Arc flags initialized outside epoch; global supervisor preinitialized before actual LatestTask interval, private supervisor measured cold. Spin/yield test-only TLS gate has no new heap and is not a cost/idle claim. Heap allowance subtracts explicit 2MiB worker and256KiB supervisor stack reserves so they cannot cover missing heap metadata;64KiB metadata allowance per thread is a conservative bound validated here, not actual stack/RSS/C/allocator coverage. Artificial denial reservation excluded from heap allowance observations and has zero allocations. Common runtime now also used by PTY/layout/watcher; this probe measures runtime ownership and retained cancellation control, not all generic captured payloads or SFTP independent executor/global GUI/OS/RSS/long-run acceptance.'
    prefix=['rustup','run',args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-task-runtime-allocation-') as temporary:
        p=Path(temporary);project(root,p)
        runtime=p/'src/runtime.rs';runtime.write_text(runtime.read_text()+'\n'+fixtures[1].read_text())
        shutil.copy2(fixtures[0],p/'src/executor_allocation_meter.rs');main=p/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc']=run(prefix+['rustc','-Vv'],p,15);binary,result['build']=build(p,prefix)
        result['sanity']=run([str(binary),'action::tests','--test-threads=1'],p,15)
        result['measurement']=run([str(binary),'runtime::allocation_probe::','--nocapture','--test-threads=1'],p,60)
        log=result['measurement']['stdout']+result['measurement']['stderr']
        assert result['sanity']['exit']==result['measurement']['exit']==0,log[-12000:]
        keys=['live','peak','used','heap_fee','heap_peak','workers','allocations','reallocations']
        result['samples']=[dict(mode=m[0],step=m[1],**dict(zip(keys,map(int,m[2:])))) for m in re.findall(r'RUNTIME_ALLOC mode=(\w+) step=(\w+) live=(\d+) peak=(\d+) used=(\d+) heap_fee=(\d+) heap_peak=(\d+) workers=(\d+) allocations=(\d+) reallocations=(\d+)',log)]
        assert len(result['samples'])==11 and all(s['live']<=s['heap_fee'] and s['peak']<=s['heap_peak'] for s in result['samples'])
        assert 'denied_new_allocations=0 final=0' in log
        result['model']=dict(zip(['work_stack','reaper_stack','margin','registry_fee','budget_size','stats_size'],map(int,re.search(r'RUNTIME_MODEL work_stack=(\d+) reaper_stack=(\d+) margin=(\d+) registry_fee=(\d+) budget_size=(\d+) stats_size=(\d+)',log).groups())))


def costs(root,args,result):
    assert args.baseline
    ui=args.ui_cost
    fixture=root/'tools/fixtures'/('runtime_ui_cost_cases.rs' if ui else 'runtime_cost_cases.rs')
    result['fixtures']={str(fixture.relative_to(root)):identity(fixture)}
    result['support_tools_sha256']={n:hashlib.sha256((root/'tools'/n).read_bytes()).hexdigest() for n in ['check_ssh_transport.py','check_ui_state.py']}
    result['baseline_source_sha256']=sources(args.baseline)
    result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    result['boot_adapters']={cohort:'let _runtime_owner=crate::runtime::initialize().unwrap();' for cohort in ['before','after']}
    result['scope']=('Same System allocator actual PTY/app warm UI workload,5000 updates and500 full software draws,1024 matches and5 modes,pixel fingerprints equal. Both cohorts initialize their existing supervisor before app and keep it alive through measurement; current PTY threads additionally use the registry. No native window/IME/input latency or complete managed/OS/stack/RSS bound.' if ui else 'Same System allocator fixture body. First standard thread global state and process Budget initialized before boot interval. Both boot adapters create their existing supervisor and warm first sync; startup is measured once per process, not a stable boot latency claim. Constructor only after boot,100x1000 pending replacements,100x10000 polling,100x1000 legacy token lifecycle,100 one-shot task create/submit/result/drop API cycles. Each cycle settled outside timer to original owner usage; both cohorts retain native join ownership; current additionally splits heap/stack tickets and enters the owner scope centrally.500ms blocked worker/idle supervisor sampled every5ms by libproc; test harness is included, no GUI/native/system subprocess load or whole-app idle claim.64KiB metadata/requested stack allowances not actual stacks/RSS guarantee.')+' Two warmups each then5 alternating pairs; time-lp CPU/RSS entire test process includes setup,verification,settlement and all drops. No new package, application subprocess or supervisor in this stage; the existing common supervisor now owns PTY/watcher/layout threads.'
    sample,result['sampler']=sampler();assert sample is not None
    with tempfile.TemporaryDirectory(prefix='cg-task-runtime-cost-') as temporary:
        result['rustc']=run(['rustc','-Vv'],Path(temporary),15);builds={};result['builds']={}
        for cohort,source in [('before',args.baseline),('after',root)]:
            p=Path(temporary)/cohort;p.mkdir();project(source,p)
            file=p/('src/main.rs' if ui else 'src/tasks.rs')
            file.write_text(file.read_text()+'\n'+fixture.read_text().replace('// RUNTIME_BOOT',result['boot_adapters'][cohort]))
            binary,result['builds'][cohort]=build(p,[]);builds[cohort]=(p,binary)
        case='ui_state_cost::same_update_and_bar_pixels' if ui else 'tasks::runtime_cost_probe::same_pending_replacements_and_cancellation'
        def trial(cohort):
            p,binary=builds[cohort]
            proc=subprocess.Popen(['/usr/bin/time','-lp',str(binary),case,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=p,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,start_new_session=True)
            lines=[];phases={};samples=[]
            def read():
                for line in proc.stdout:
                    lines.append(line)
                    if 'RUNTIME_IDLE_START' in line:phases['start']=time.monotonic_ns()
                    if 'RUNTIME_IDLE_END' in line:phases['end']=time.monotonic_ns()
            reader=threading.Thread(target=read);reader.start();start=time.monotonic()
            try:
                while proc.poll() is None:
                    samples.append(dict(ns=time.monotonic_ns(),tree=sample(proc.pid)))
                    if time.monotonic()-start>60:raise RuntimeError('owned benchmark timeout')
                    time.sleep(.005)
                reader.join(5);assert not reader.is_alive()
            finally:
                if proc.poll() is None:
                    import os,signal
                    os.killpg(proc.pid,signal.SIGKILL);proc.wait();reader.join(5)
            log=''.join(lines);assert proc.returncode==0,log[-12000:]
            r=dict(cohort=cohort,exit=proc.returncode,log=log,samples=samples,phases=phases)
            if ui:r.update(parse_ui(log))
            else:
                match=re.search(r'RUNTIME_COST boot_ns=(\d+) create_ns=(\d+) signature=(\d+) lifecycle=(\[[^\n]+\]) pending=(\[[^\n]+\]) polling=(\[[^\n]+\]) tokens=(\[[^\n]+\]) budget_peak=(\d+)',log)
                assert match,log[-12000:]
                r.update(boot_ns=int(match[1]),create_ns=int(match[2]),signature=int(match[3]),budget_peak=int(match[8]))
                for key,raw in zip(['lifecycle','pending','polling','tokens'],match.groups()[3:7]):
                    values=json.loads(raw);assert len(values)==100;r[key]=dict(samples_ns=values,total_ns=sum(values))
                assert set(phases)=={'start','end'}
                idle=[s for s in samples if phases['start']<=s['ns']<=phases['end']];assert len(idle)>=10
                def target(s):return max(s['tree'].items(),key=lambda v:v[1]['threads'])
                a,b=target(idle[0]),target(idle[-1]);assert a[0]==b[0]
                r['idle']=dict(samples=len(idle),duration_ns=idle[-1]['ns']-idle[0]['ns'],cpu_delta_ns=b[1]['cpu_ns']-a[1]['cpu_ns'],threads_min=min(target(s)[1]['threads'] for s in idle),threads_max=max(target(s)[1]['threads'] for s in idle),rss_peak=max(target(s)[1]['rss'] for s in idle))
            for key,pattern in [('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$'),('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$')]:
                value=re.search(pattern,log,re.M);assert value;r[key]=int(value[1]) if key=='rss_bytes' else float(value[1])
            return r
        result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
        for pair in range(5):
            for c in (['before','after'] if pair%2==0 else ['after','before']):
                r=trial(c);r['pair']=pair;result['runs'].append(r)
                print(json.dumps(dict(cohort=c,pair=pair,signature=r['signature'])),flush=True)
        assert {r['signature'] for r in result['runs']+result['warmups']}==({6821514814623131185} if ui else {204200})
        result['summary']={}
        for key in (['update','draw'] if ui else ['pending','polling','tokens','lifecycle','create_ns','boot_ns','idle_cpu_ns','idle_threads'])+['rss','cpu']:
            values=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']]
                if key=='rss':value=b['rss_bytes']-a['rss_bytes']
                elif key=='cpu':value=b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds']
                elif key=='boot_ns':value=b[key]-a[key]
                elif key=='idle_cpu_ns':value=b['idle']['cpu_delta_ns']-a['idle']['cpu_delta_ns']
                elif key=='idle_threads':value=b['idle']['threads_max']-a['idle']['threads_max']
                elif key=='create_ns':value=100*(b[key]/a[key]-1)
                else:value=100*(b[key]['total_ns']/a[key]['total_ns']-1)
                values.append(value)
            result['summary'][key]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)


def warm_ui(root,args,result):
    fixture=root/'tools/fixtures/runtime_ui_cost_cases.rs'
    result['fixtures']={str(fixture.relative_to(root)):identity(fixture)}
    result['support_tools_sha256']={'check_ui_state.py':hashlib.sha256((root/'tools/check_ui_state.py').read_bytes()).hexdigest()}
    text=fixture.read_text().replace('// RUNTIME_BOOT','let _runtime_owner=crate::runtime::initialize().unwrap();')
    ui_allocation(root,args,result,text)
    result['scope']+=' Shared supervisor is explicitly initialized and native sync bootstrapped before actual app/warm frames, remains alive during interval and shuts down only after metering. Its cold metadata/worker/TLS coverage is independently tested, not excluded then claimed paid by this UI interval.'


def main():
    p=argparse.ArgumentParser(description=__doc__);mode=p.add_mutually_exclusive_group(required=True);mode.add_argument('--allocation',action='store_true');mode.add_argument('--cost',action='store_true');mode.add_argument('--ui-cost',action='store_true');mode.add_argument('--ui-allocation',action='store_true');p.add_argument('--baseline',type=Path);p.add_argument('--toolchain');p.add_argument('--output',type=Path,required=True);args=p.parse_args()
    root=Path(__file__).resolve().parent.parent
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    try:
        (allocation if args.allocation else warm_ui if args.ui_allocation else costs)(root,args,result);assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
