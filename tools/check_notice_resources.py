#!/usr/bin/env python3
"""通知中心：固定记录额度、生命周期计量及冻结源码同负载性能。"""
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

CASE='notifications::cost_probe::same_notice_first_hot_describe_clear_refill_and_muted_work'


def project(source,target,fixture):
    for n in ['src','assets']:shutil.copytree(source/n,target/n)
    for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(source/n,target/n)
    notices=target/'src/notifications.rs';notices.write_text(notices.read_text()+'\n'+fixture)


def allocation(root,args,result):
    result['scope']='Temporary-copy all-thread epoch Rust requested-byte meter (conservative realloc overlap). Parent budget/context/lock/time preinitialized. Actual Center first admitted event,10000bounded accepted events,1000muted IDs and no replay, busy/system-rate gates (no helper), quota-denied distinct Center commands+session end then recovery, actual capacity release on clear preserving dedupe/rate, refill and final drop. No system OS dispatch/LatestTask/global metadata/platform inputs/window/allocator/C/OS/stack/RSS claim; SFTP ring is separate.'
    fixtures=[root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs','notice_allocation_cases.rs']];result['allocation_fixtures']={str(p.relative_to(root)):identity(p) for p in fixtures};prefix=['rustup','run',args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-notice-allocation-') as temporary:
        p=Path(temporary);project(root,p,fixtures[1].read_text());shutil.copy2(fixtures[0],p/'src/executor_allocation_meter.rs');main=p/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n');result['rustc']=run(prefix+['rustc','-Vv'],p,15);binary,result['build']=build(p,prefix)
        result['sanity']=run([str(binary),'action::tests','--test-threads=1'],p,15);result['measurement']=run([str(binary),'notifications::allocation_probe::measured_notice_window_clear_denial_dedupe_and_refill_return_all_credit','--exact','--nocapture','--test-threads=1'],p,20);log=result['measurement']['stdout']+result['measurement']['stderr']
        keys=['live','peak','used','budget_peak','allocations','reallocations'];result['samples']=[dict(step=m[0],**dict(zip(keys,map(int,m[1:])))) for m in re.findall(r'NOTICE_ALLOC step=(\w+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)',log)]
        if result['sanity']['exit'] or result['measurement']['exit'] or len(result['samples'])!=7 or 'NOTICE_FINAL live=0 used=0 hot_new_allocations=0 denied_new_allocations=0 slots=128' not in log:raise RuntimeError(log[-14000:])
        assert all(s['live']<=s['used'] and s['peak']<=s['budget_peak'] for s in result['samples']);result['item_size']=int(re.search(r'item_size=(\d+)',log)[1])


def cost(root,args,result,fixture):
    if args.baseline is None:raise RuntimeError('--cost requires frozen --baseline')
    result['scope']='Same System-allocator fixture in baseline/current temporary copies, two warmups per cohort then five alternating pairs.5000first admission API samples with fresh Centers (Default/drop outside timer),200batches x1000 hot eligible notices,200x1000 bounded formatter calls,5000 explicit clear+refill,200x1000 muted events. Deterministic notice/description/dedupe fingerprint equal; black_box prevents discarded work. time -lp whole test CPU/RSS includes setup, verification and drops. Component timers not native input/OS notification delivery/UI, closed-feature whole-app startup/other-platform/long-run. Clear now intentionally returns capacity; re-admission costs reported separately. No new package/helper/thread.'
    result['baseline_source_sha256']=sources(args.baseline);result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-notice-cost-') as temporary:
        builds={};result['builds']={};result['rustc']=run(['rustc','-Vv'],Path(temporary),15)
        for c,source in [('before',args.baseline),('after',root)]:
            p=Path(temporary)/c;p.mkdir();project(source,p,fixture);binary,built=build(p,[]);builds[c]=(p,binary);result['builds'][c]=built
        def trial(c):
            p,binary=builds[c];proc=subprocess.run(['/usr/bin/time','-lp',str(binary),CASE,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=p,capture_output=True,text=True,timeout=60);log=proc.stdout+proc.stderr
            m=re.search(r'NOTICE_COST signature=(\d+) first=(\[[^\n]+\]) hot=(\[[^\n]+\]) describe=(\[[^\n]+\]) refill=(\[[^\n]+\]) muted=(\[[^\n]+\]) first_retained=(\d+) budget_peak=(\d+) final=0',log)
            if proc.returncode or m is None:raise RuntimeError(log[-14000:])
            r=dict(cohort=c,exit=proc.returncode,log=log,signature=int(m[1]),first_retained=int(m[7]),budget_peak=int(m[8]))
            for key,raw,count in [('first',m[2],5000),('hot',m[3],200),('describe',m[4],200),('refill',m[5],5000),('muted',m[6],200)]:
                samples=json.loads(raw);assert len(samples)==count;ordered=sorted(samples);r[key]=dict(samples_ns=samples,total_ns=sum(samples),p50_ns=ordered[(count-1)//2],p95_ns=ordered[(count*95-1)//100],p99_ns=ordered[(count*99-1)//100],maximum_ns=ordered[-1])
            for key,pattern in [('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$'),('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$')]:
                value=re.search(pattern,log,re.M)
                if value is None:raise RuntimeError(log[-14000:])
                r[key]=float(value[1]) if key!='rss_bytes' else int(value[1])
            return r
        result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
        for pair in range(5):
            for c in (['before','after'] if pair%2==0 else ['after','before']):
                r=trial(c);r['pair']=pair;result['runs'].append(r);print(json.dumps(dict(cohort=c,pair=pair,first_ns=r['first']['total_ns'],hot_ns=r['hot']['total_ns'],refill_ns=r['refill']['total_ns'])),flush=True)
        assert len({r['signature'] for r in result['warmups']+result['runs']})==1
        result['summary']={}
        for key in ['first','hot','describe','refill','muted','rss','cpu']:
            values=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']]
                values.append(100*(b[key]['total_ns']/a[key]['total_ns']-1) if key not in ['rss','cpu'] else b['rss_bytes']-a['rss_bytes'] if key=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'])
            result['summary'][key]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)


def main():
    parser=argparse.ArgumentParser(description=__doc__);mode=parser.add_mutually_exclusive_group(required=True);mode.add_argument('--allocation',action='store_true');mode.add_argument('--cost',action='store_true');parser.add_argument('--toolchain');parser.add_argument('--baseline',type=Path);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    if args.toolchain and not args.allocation:parser.error('--toolchain applies to --allocation')
    root=Path(__file__).resolve().parent.parent;fixture=root/'tools/fixtures/notice_cost_cases.rs';result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={str(fixture.relative_to(root)):identity(fixture)})
    try:
        allocation(root,args,result) if args.allocation else cost(root,args,result,fixture.read_text());assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
