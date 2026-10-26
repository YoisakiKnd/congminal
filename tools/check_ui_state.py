#!/usr/bin/env python3
"""UI装饰与提示栏：真实更新/软件绘制分配计量、冻结源码像素与成本对照。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
from check_font_discovery import build, sources
from check_sftp_executor import identity, run

CASE='ui_state_cost::same_update_and_bar_pixels'


def project(root,p,fixture):
    for name in ['src','assets']:shutil.copytree(root/name,p/name)
    for name in ['Cargo.toml','Cargo.lock']:shutil.copy2(root/name,p/name)
    main=p/'src/main.rs';main.write_text(main.read_text()+'\n'+fixture)


def parse(log):
    m=re.search(r'UI_COST signature=(\d+) used=(\d+) update=(\[[^\n]+\]) draw=(\[[^\n]+\]) final=0',log)
    if m is None:raise RuntimeError(log[-12000:])
    result=dict(signature=int(m[1]),used=int(m[2]))
    for name,raw,count in [('update',m[3],5000),('draw',m[4],500)]:
        samples=json.loads(raw);assert len(samples)==count;ordered=sorted(samples)
        result[name]=dict(samples_ns=samples,total_ns=sum(samples),p50_ns=ordered[(count-1)//2],p95_ns=ordered[(count*95-1)//100],p99_ns=ordered[(count*99-1)//100],maximum_ns=ordered[-1])
    return result


def allocation(root,args,result,fixture):
    result['scope']='Temporary-copy epoch all-thread Rust requested-byte meter, conservative realloc overlap. Cold1026 decorations/64KiB UTF8 text, view owner swap/full-root denial/final release; then real Congminal with preinitialized local PTY/terminal/search1024matches/notification/hover/font/canvas, warm50mixed frames. Actual update_ui5000 times and500 full software draws cycling search+preedit, notification, shell, URL, none. Root/OS/thread startup and retained input state prewarm excluded; no native presentation/IME/clipboard/end-to-end latency, allocator/C/OS/actual stacks/RSS or full Search/config/task accounting claim.'
    prefix=['rustup','run',args.toolchain] if args.toolchain else []
    meter=root/'tools/fixtures/executor_allocation_meter.rs';cold=root/'tools/fixtures/ui_state_allocation_cases.rs'
    result['allocation_fixtures']={str(p.relative_to(root)):identity(p) for p in [meter,cold]}
    measured=fixture.replace('// UI_METER_START','crate::executor_allocation_meter::start();').replace('// UI_METER_STOP','let measured=crate::executor_allocation_meter::stop();assert_eq!((measured.allocations,measured.reallocations,measured.live,measured.peak),(0,0,0,0));eprintln!("UI_WARM allocations=0 reallocations=0 live=0 peak=0");')
    with tempfile.TemporaryDirectory(prefix='cg-ui-allocation-') as temporary:
        p=Path(temporary);project(root,p,measured);shutil.copy2(meter,p/'src/executor_allocation_meter.rs')
        main=p/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        state=p/'src/ui_state.rs';state.write_text(state.read_text()+'\n'+cold.read_text())
        result['rustc']=run(prefix+['rustc','-Vv'],p,15);binary,result['build']=build(p,prefix)
        result['sanity']=run([str(binary),'action::tests','--test-threads=1'],p,15)
        result['cold']=run([str(binary),'ui::state::state_allocation::cold_growth_view_move_denial_and_release_are_funded','--exact','--nocapture','--test-threads=1'],p,15)
        result['warm']=run([str(binary),CASE,'--exact','--ignored','--nocapture','--test-threads=1'],p,60)
        for key in ['sanity','cold','warm']:
            if result[key]['exit']:raise RuntimeError(str(result[key])[-15000:])
        m=re.search(r'UI_COLD live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+) denied_new_allocations=0 final=0',result['cold']['stderr'])
        if not m or 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' not in result['warm']['stderr']:raise RuntimeError('missing UI allocation evidence')
        result['sample']=dict(zip(['live','peak','used','budget_peak','allocations','reallocations'],map(int,m.groups())))
        assert result['sample']['live']<=result['sample']['used'] and result['sample']['peak']<=result['sample']['budget_peak']
        result['frames']=parse(result['warm']['stdout']+result['warm']['stderr'])


def cost(root,args,result,fixture):
    if args.baseline is None:raise RuntimeError('--cost requires --baseline')
    result['scope']='Same fixture in frozen baseline/current System allocator projects, two warmups each then five alternating pairs. Real prewarmed local PTY/app,1024search matches, five rotating modes search/IME-preedit, notification, shell, URL, empty. Per trial5000 update_ui API intervals and500 update+forced full software draws, pixel FNV fingerprint after each draw outside timer. time -lp entire-process CPU/RSS includes startup/prewarm/harness/hash/cleanup, not API-only or native input/presentation/other-platform/long-run. No new package, runtime process or thread.'
    result['baseline_source_sha256']=sources(args.baseline);result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-ui-cost-') as temporary:
        builds={};result['builds']={};result['rustc']=run(['rustc','-Vv'],Path(temporary),15)
        for cohort,source in [('before',args.baseline),('after',root)]:
            p=Path(temporary)/cohort;p.mkdir();project(source,p,fixture);binary,built=build(p,[]);builds[cohort]=(p,binary);result['builds'][cohort]=built
        def trial(cohort):
            p,binary=builds[cohort];process=subprocess.run(['/usr/bin/time','-lp',str(binary),CASE,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=p,capture_output=True,text=True,timeout=60)
            log=process.stdout+process.stderr
            if process.returncode:raise RuntimeError(log[-12000:])
            r=parse(log);r.update(cohort=cohort,exit=process.returncode,log=log)
            for name,pattern in [('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$'),('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$')]:
                m=re.search(pattern,log,re.M)
                if not m:raise RuntimeError(log[-12000:])
                r[name]=float(m[1]) if name!='rss_bytes' else int(m[1])
            return r
        result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
        for pair in range(5):
            for c in (['before','after'] if pair%2==0 else ['after','before']):
                r=trial(c);r['pair']=pair;result['runs'].append(r);print(json.dumps(dict(cohort=c,pair=pair,update_ns=r['update']['total_ns'],draw_ns=r['draw']['total_ns'])),flush=True)
        assert len({r['signature'] for r in result['warmups']+result['runs']})==1
        result['summary']={}
        for mode in ['update','draw','rss','cpu']:
            values=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['pair']==pair and r['cohort']==c) for c in ['before','after']]
                values.append(100*(b[mode]['total_ns']/a[mode]['total_ns']-1) if mode in ['update','draw'] else b['rss_bytes']-a['rss_bytes'] if mode=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'])
            result['summary'][mode]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)


def main():
    parser=argparse.ArgumentParser(description=__doc__);mode=parser.add_mutually_exclusive_group(required=True);mode.add_argument('--allocation',action='store_true');mode.add_argument('--cost',action='store_true');parser.add_argument('--toolchain');parser.add_argument('--baseline',type=Path);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    if args.toolchain and not args.allocation:parser.error('--toolchain applies to --allocation')
    root=Path(__file__).resolve().parent.parent;fixture=root/'tools/fixtures/ui_state_cost_cases.rs'
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={str(fixture.relative_to(root)):identity(fixture)})
    try:
        (allocation if args.allocation else cost)(root,args,result,fixture.read_text());assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
