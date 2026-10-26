#!/usr/bin/env python3
"""渲染工作集：冻结源码的五组像素/成本对照，或临时副本全线程请求计量。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
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

COST = 'render::working_set_cost::same_pixels_full_and_idle_views'
COLD = 'render::working_allocation::cold_work_growth_overlap_move_denial_and_release_are_funded'


def sources(root):
    return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}


def copy_project(source, destination, fixture, allocation=False):
    for name in ['src','assets']: shutil.copytree(source/name,destination/name)
    for name in ['Cargo.toml','Cargo.lock']: shutil.copy2(source/name,destination/name)
    render = destination/'src/render.rs'
    render.write_text(render.read_text()+'\n'+fixture)
    if allocation:
        shutil.copy2(Path(__file__).parent/'fixtures/executor_allocation_meter.rs',destination/'src/executor_allocation_meter.rs')
        main=destination/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')


def build(root, prefix):
    built=run(prefix+['cargo','test','--offline','--release','--no-run','--message-format=json'],root,240)
    artifacts=[json.loads(line) for line in built['stdout'].splitlines() if line.startswith('{')]
    result=dict(exit=built['exit'],stderr=built['stderr'],diagnostics=[a['message'].get('rendered','') for a in artifacts if a.get('reason')=='compiler-message'])
    if built['exit']: raise RuntimeError(str(result))
    binary=Path(next(a['executable'] for a in artifacts if a.get('reason')=='compiler-artifact' and a.get('target',{}).get('name')=='congminal' and a.get('executable')))
    return binary,result


def parse(log):
    rows={}
    for mode,total,pixels,raw in re.findall(r'RENDER_WORK_END mode=(full|idle) total_ns=(\d+) pixels=(\d+) samples=(\[[^\n]+\])',log):
        samples=json.loads(raw)
        if len(samples)!=2000 or any(type(n) is not int or n<0 for n in samples): raise RuntimeError('invalid frame samples')
        ordered=sorted(samples)
        rows[mode]=dict(total_ns=int(total),pixels=int(pixels),samples_ns=samples,p50_ns=ordered[999],p95_ns=ordered[1899],p99_ns=ordered[1979],maximum_ns=ordered[-1])
    if set(rows)!={'full','idle'} or 'RENDER_WORK_CLEAN final=0' not in log: raise RuntimeError(log[-8000:])
    return rows


def trial(root,binary,outline,sample):
    env=dict(os.environ,CONGMINAL_RENDER_OUTLINE=str(int(outline)))
    process=subprocess.Popen([str(binary),COST,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,encoding='utf-8',errors='replace',start_new_session=True)
    lines=[];observations=[];overflow=threading.Event()
    def read():
        size=0
        for line in process.stdout:
            size+=len(line)
            if size>256*1024:overflow.set();continue
            lines.append(line)
    reader=threading.Thread(target=read);reader.start();start=time.monotonic()
    try:
        while process.poll() is None:
            if sample: observations.append(dict(ns=time.monotonic_ns(),tree=sample(process.pid)))
            if time.monotonic()-start>60 or overflow.is_set(): raise RuntimeError('owned render fixture timeout/output cap')
            time.sleep(.005)
        reader.join(timeout=5)
        if reader.is_alive(): raise RuntimeError('render reader did not finish')
        log=''.join(lines)
        if process.returncode:raise RuntimeError(log[-12000:])
        rows=parse(log)
        return dict(exit=process.returncode,log=log,frames=rows,samples=observations,rss_peak_observed=max((sum(v['rss'] for v in o['tree'].values()) for o in observations),default=0) or None)
    finally:
        if process.poll() is None:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
        reader.join(timeout=5)


def cost(root,args,result,fixture):
    if args.baseline is None: raise RuntimeError('--cost requires --baseline frozen project')
    result['scope']='Same System-allocator Rust fixture appended to frozen baseline and current temporary copies. Two visible terminal models/canvases and ViewState swaps, 160x48 plus81x25, ASCII/bold/composition/emoji/flags/keycaps; exact combined pixel hash equality asserted for both cohorts. Bitmap via mapped-font quota0, and normal native outline selection; fonts/glyph caches/workspaces warmed before measurement. Two warmups per cohort, then five alternating pairs per font mode, 2000 forced-full and2000 unchanged frames per trial. Per-frame nearest-rank P95/P99 are software draw only; trial total includes view swaps and checks. libproc5ms whole-trial RSS includes harness/font discovery/caches and misses peaks; no native input-to-presentation, closed-feature startup, new dependency adoption, all application memory or 30-minute claim.'
    result['baseline_source_sha256']=sources(args.baseline)
    result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    sample,result['sampler']=sampler()
    with tempfile.TemporaryDirectory(prefix='cg-render-cost-') as temporary:
        result['rustc']=run(['rustc','-Vv'],Path(temporary),15)
        projects={};result['builds']={}
        for mode,source in [('before',args.baseline),('after',root)]:
            project=Path(temporary)/mode;project.mkdir();copy_project(source,project,fixture)
            binary,built=build(project,[]);projects[mode]=(project,binary);result['builds'][mode]=built
        result['warmups'],result['runs']=[],[]
        for outline in [False,True]:
            name='outline' if outline else 'bitmap'
            for i in range(2):
                for mode in ['before','after']:
                    row=trial(*projects[mode],outline,sample);row.update(font=name,cohort=mode);result['warmups'].append(row)
            for pair in range(5):
                for mode in (['before','after'] if pair%2==0 else ['after','before']):
                    row=trial(*projects[mode],outline,sample);row.update(font=name,cohort=mode,pair=pair);result['runs'].append(row)
                    print(json.dumps(dict(font=name,cohort=mode,pair=pair,full_ns=row['frames']['full']['total_ns'],idle_ns=row['frames']['idle']['total_ns'])),flush=True)
        result['paired_summary']={}
        for font in ['bitmap','outline']:
            for frame in ['full','idle']:
                ratios=[]
                for pair in range(5):
                    a,b=[next(r for r in result['runs'] if r['font']==font and r['cohort']==mode and r['pair']==pair) for mode in ['before','after']]
                    if a['frames'][frame]['pixels']!=b['frames'][frame]['pixels']:raise RuntimeError('baseline/current pixels differ')
                    ratios.append(100*(b['frames'][frame]['total_ns']/a['frames'][frame]['total_ns']-1))
                result['paired_summary'][font+'_'+frame]=dict(median_percent=statistics.median(ratios),minimum_percent=min(ratios),maximum_percent=max(ratios),pairs_percent=ratios)


def allocation(root,args,result,fixture):
    result['scope']='Temporary-copy all-thread epoch Rust requested-byte meter, conservative old/new realloc overlap. Cold Render workset growth up to2048rows/4096cols, current+view row keys, ligature/ASCII word/emoji buffers, ownership swap, denied expansion zero new allocations, release; live and overlap peaks bounded by Render/total credits. Separate fully warmed actual draw2000full+2000idle/two views/bitmap and outline; no production allocator changes. Parent budgets/font discovery/fonts/grids/canvases preinitialized for warm epoch. Meter excludes headers/allocator/C/ObjC/OS/actual stack/RSS and initial metadata; no whole-app or native latency cap.'
    prefix=['rustup','run',args.toolchain] if args.toolchain else []
    fixture=fixture.replace('// RENDER_METER_START','crate::executor_allocation_meter::start();').replace('// RENDER_METER_STOP','let counts=crate::executor_allocation_meter::stop();assert_eq!(counts.live,0);eprintln!("RENDER_WARM_ALLOC allocations={} reallocations={} live={} peak={} final=0",counts.allocations,counts.reallocations,counts.live,counts.peak);')
    fixture+='\n'+(root/'tools/fixtures/render_working_allocation_cases.rs').read_text()
    with tempfile.TemporaryDirectory(prefix='cg-render-alloc-') as temporary:
        project=Path(temporary);copy_project(root,project,fixture,True)
        result['rustc']=run(prefix+['rustc','-Vv'],project,15)
        binary,result['build']=build(project,prefix)
        result['sanity']=run([str(binary),'action::tests','--test-threads=1'],project,15)
        if result['sanity']['exit']:raise RuntimeError('instrumented action sanity failed')
        result['cold']=run([str(binary),COLD,'--exact','--nocapture','--test-threads=1'],project,15)
        log=result['cold']['stdout']+result['cold']['stderr']
        match=re.search(r'RENDER_ALLOC live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) final=0 allocations=(\d+) reallocations=(\d+)',log)
        if result['cold']['exit'] or not match:raise RuntimeError(log[-10000:])
        result['cold_counts']=dict(zip(['live','peak','used','budget_peak','allocations','reallocations'],map(int,match.groups())))
        result['warm']=[]
        for outline in [False,True]:
            row=trial(project,binary,outline,None);row['font']='outline' if outline else 'bitmap'
            match=re.search(r'RENDER_WARM_ALLOC allocations=(\d+) reallocations=(\d+) live=(\d+) peak=(\d+) final=0',row['log'])
            if not match:raise RuntimeError(row['log'][-10000:])
            row['counts']=dict(zip(['allocations','reallocations','live','peak'],map(int,match.groups())))
            if any(row['counts'].values()):raise RuntimeError('fully warmed render still allocates')
            result['warm'].append(row)


def main():
    parser=argparse.ArgumentParser(description=__doc__);mode=parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--cost',action='store_true');mode.add_argument('--allocation',action='store_true')
    parser.add_argument('--baseline',type=Path);parser.add_argument('--toolchain');parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    if args.toolchain and not args.allocation:parser.error('--toolchain applies to allocation only')
    root=Path(__file__).resolve().parent.parent
    fixture=root/'tools/fixtures/render_working_set_cases.rs';others=[root/'tools/fixtures/render_working_allocation_cases.rs',root/'tools/fixtures/executor_allocation_meter.rs']
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={str(p.relative_to(root)):identity(p) for p in [fixture]+others})
    try:
        (cost if args.cost else allocation)(root,args,result,fixture.read_text())
        if sources(root)!=result['source_sha256']:raise RuntimeError('source changed during measurement')
        result['passed']=True
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
