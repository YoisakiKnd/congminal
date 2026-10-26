#!/usr/bin/env python3
"""Cold root metadata request coverage and same-load root/lease/snapshot costs."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import tempfile
from check_font_discovery import build, sources
from check_sftp_executor import run, identity


def project(source, target, fixture):
    for name in ['src', 'assets']:
        shutil.copytree(source/name,target/name)
    for name in ['Cargo.toml','Cargo.lock']:
        shutil.copy2(source/name,target/name)
    file=target/'src/resources/budget.rs'
    file.write_text(file.read_text()+'\n'+fixture)


def allocation(root,args,result):
    fixtures=[root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs','root_budget_allocation_cases.rs']]
    result['fixtures']={str(p.relative_to(root)):identity(p) for p in fixtures}
    result['scope']='Cold root metadata self-admission before Arc allocation; first native Mutex lazy header, payload/child parent credit, child root metadata not doubled, original root dropped while child/Lease retain it, final all requested bytes0. No prewarmed Budget or uncharged large root reservation. Stack/allocator/C/OS/global OnceLock static image/context/config/thread runtime and Weak-held root Arc beyond payload are not proved. No runtime dependencies/process/threads added.'
    prefix=['rustup','run',args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-root-budget-') as temporary:
        p=Path(temporary);project(root,p,fixtures[1].read_text())
        shutil.copy2(fixtures[0],p/'src/executor_allocation_meter.rs')
        main=p/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc']=run(prefix+['rustc','-Vv'],p,15)
        binary,result['build']=build(p,prefix)
        result['measurement']=run([str(binary),'resources::budget::root_allocation_probe::','--nocapture','--test-threads=1'],p,30)
        log=result['measurement']['stdout']+result['measurement']['stderr']
        assert result['measurement']['exit']==0,log[-10000:]
        keys=['live','peak','used','budget_peak','allocations','reallocations']
        result['samples']=[dict(step=m[0],**dict(zip(keys,map(int,m[1:])))) for m in re.findall(r'ROOT_ALLOC step=(\w+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)',log)]
        assert len(result['samples'])==5 and all(s['live']<=s['used'] and s['peak']<=s['budget_peak'] for s in result['samples'])
        assert 'ROOT_FINAL refused_new_allocations=0 final=0' in log
        result['layout']=dict(zip(['budget','stats','self_fee'],map(int,re.search(r'budget_size=(\d+) stats_size=(\d+) self_fee=(\d+)',log).groups())))


def cost(root,args,result):
    assert args.baseline
    fixture=root/'tools/fixtures/root_budget_cost_cases.rs'
    result['fixtures']={str(fixture.relative_to(root)):identity(fixture)}
    result['scope']='Same System allocator body,100x100 cold roots with initial snapshot+32B lease+snapshot+drop,100x10000 admitted/released32B leases,100x10000 snapshots. Explicit raw-root versus self-funded-root adapter, two warmups/cohort and5 alternating pairs; normalized payload signature equal320000. macOS time-lp process CPU/RSS include harness and validation; intervals include full operation and drops. No GUI/native latency, OS process/thread/global metadata cost proof.'
    result['baseline_source_sha256']=sources(args.baseline)
    result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    result['adapters']={'before':'fn root(limit:usize)->Arc<Budget>{Budget::new(limit,0)}','after':'fn root(limit:usize)->Arc<Budget>{Budget::root(limit,0).unwrap()}'}
    with tempfile.TemporaryDirectory(prefix='cg-root-budget-cost-') as temporary:
        builds={};result['builds']={}
        for c,source in [('before',args.baseline),('after',root)]:
            p=Path(temporary)/c;p.mkdir();project(source,p,fixture.read_text().replace('// ROOT_ADAPTER',result['adapters'][c]))
            binary,result['builds'][c]=build(p,[]);builds[c]=(p,binary)
        def trial(c):
            p,binary=builds[c]
            r=run(['/usr/bin/time','-lp',str(binary),'resources::budget::root_cost_probe::same_roots_leases_and_snapshots','--exact','--ignored','--nocapture','--test-threads=1'],p,60)
            log=r['stdout']+r['stderr'];m=re.search(r'ROOT_COST signature=(\d+) roots=(\[[^\n]+\]) leases=(\[[^\n]+\]) snapshots=(\[[^\n]+\]) root_fee=(\d+)',log)
            assert r['exit']==0 and m,log[-10000:]
            r.update(cohort=c,signature=int(m[1]),root_fee=int(m[5]))
            for key,raw in zip(['roots','leases','snapshots'],m.groups()[1:4]):
                values=json.loads(raw);assert len(values)==100;r[key]=dict(samples_ns=values,total_ns=sum(values))
            for key,pattern in [('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$'),('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$')]:
                value=re.search(pattern,log,re.M);assert value;r[key]=int(value[1]) if key=='rss_bytes' else float(value[1])
            return r
        result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
        for pair in range(5):
            for c in (['before','after'] if pair%2==0 else ['after','before']):
                r=trial(c);r['pair']=pair;result['runs'].append(r)
                print(json.dumps(dict(cohort=c,pair=pair,roots_ns=r['roots']['total_ns'])),flush=True)
        assert {r['signature'] for r in result['warmups']+result['runs']}=={320000}
        result['summary']={}
        for key in ['roots','leases','snapshots','rss','cpu']:
            values=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']]
                values.append(b['rss_bytes']-a['rss_bytes'] if key=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if key=='cpu' else 100*(b[key]['total_ns']/a[key]['total_ns']-1))
            result['summary'][key]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)


def main():
    p=argparse.ArgumentParser(description=__doc__);m=p.add_mutually_exclusive_group(required=True)
    m.add_argument('--allocation',action='store_true');m.add_argument('--cost',action='store_true')
    p.add_argument('--toolchain');p.add_argument('--baseline',type=Path);p.add_argument('--output',type=Path,required=True);args=p.parse_args()
    root=Path(__file__).resolve().parent.parent
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    try:
        (allocation if args.allocation else cost)(root,args,result)
        assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
