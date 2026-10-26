#!/usr/bin/env python3
"""搜索查询/KMP/检查点资源：冷/暖请求计量与冻结源码同负载成本。"""
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

CASE='search::cost_probe::same_query_edits_history_scans_checkpoints_and_navigation'


def project(source,target,fixture):
    for n in ['src','assets']:shutil.copytree(source/n,target/n)
    for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(source/n,target/n)
    search=target/'src/search.rs';search.write_text(search.read_text()+'\n'+fixture)


def allocation(root,args,result):
    result['scope']='Temporary-copy all-thread epoch Rust requested bytes; conservative realloc overlap. Query1024ASCII/KMP1024 chars/prefix/recent/saved checkpoint/match1024 slots and1024B UTF8 preedit from cold Search in own pool, actual populated terminal and input fixtures preinitialized outside epoch. Within the same epoch (keeping cold allocations tagged through final frees), compare before/after warm counters: actual output revision restore,100 restart/backspace/insert/preedit/rescan cycles, smaller-query/backward navigation, full-pool new Search refusal zero allocations. Query/preedit remain paid after close; final owning pool zero. No input event/platform/clipboard/terminal allocation/root budget startup/allocator/C/OS/actual stacks/RSS/UI presentation or all metadata claim.'
    fixtures=[root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs','search_allocation_cases.rs']];result['allocation_fixtures']={str(p.relative_to(root)):identity(p) for p in fixtures}
    prefix=['rustup','run',args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-search-allocation-') as temporary:
        p=Path(temporary);project(root,p,fixtures[1].read_text());shutil.copy2(fixtures[0],p/'src/executor_allocation_meter.rs');main=p/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc']=run(prefix+['rustc','-Vv'],p,15);binary,result['build']=build(p,prefix)
        result['sanity']=run([str(binary),'action::tests','--test-threads=1'],p,15)
        result['measurement']=run([str(binary),'search::allocation_probe::','--nocapture','--test-threads=1'],p,30)
        log=result['measurement']['stdout']+result['measurement']['stderr']
        m=re.search(r'SEARCH_COLD live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)',log)
        if result['sanity']['exit'] or result['measurement']['exit'] or m is None or 'SEARCH_WARM allocations=0 reallocations=0 live=0 peak=0' not in log or 'denied_new_allocations=0 final=0' not in log:raise RuntimeError(log[-14000:])
        result['sample']=dict(zip(['live','peak','used','budget_peak','allocations','reallocations'],map(int,m.groups())))
        assert result['sample']['live']<=result['sample']['used'] and result['sample']['peak']<=result['sample']['budget_peak']
        result['patterns']=[dict(zip(['chars','live','peak','used','budget_peak'],map(int,m))) for m in re.findall(r'SEARCH_PATTERN chars=(\d+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) final=0',log)]
        assert len(result['patterns'])==7 and all(s['live']<=s['used'] and s['peak']<=s['budget_peak'] for s in result['patterns'])


def cost(root,args,result,fixture):
    if args.baseline is None:raise RuntimeError('--cost requires frozen --baseline')
    result['scope']='Same System-allocator fixture in frozen baseline/current temporary copies.1000backspace+insert API intervals (no scan in interval),100 restart+drain scans,100 backward boundary navigation+drain scans,1000 actual tail output changes with sync+drain timed after parser output. Identical history/Unicode/KMP inputs; selection/match ranges/pending fingerprint equal. Two warmups each, five alternating pairs. time -lp entire test CPU/RSS includes fixture/history construction/parser/hash/drop outside API timers; not UI/input/presentation latency, fixed external corpus, other-platform or long-run. No added package/runtime child/thread.'
    result['baseline_source_sha256']=sources(args.baseline);result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-search-cost-') as temporary:
        builds={};result['builds']={};result['rustc']=run(['rustc','-Vv'],Path(temporary),15)
        for c,source in [('before',args.baseline),('after',root)]:
            p=Path(temporary)/c;p.mkdir();project(source,p,fixture);binary,built=build(p,[]);builds[c]=(p,binary);result['builds'][c]=built
        def trial(c):
            p,binary=builds[c];proc=subprocess.run(['/usr/bin/time','-lp',str(binary),CASE,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=p,capture_output=True,text=True,timeout=60);log=proc.stdout+proc.stderr
            m=re.search(r'SEARCH_COST signature=(\d+) edits=(\[[^\n]+\]) scans=(\[[^\n]+\]) navigation=(\[[^\n]+\]) checkpoints=(\[[^\n]+\]) retained=(\d+) budget_peak=(\d+) final=0',log)
            if proc.returncode or m is None:raise RuntimeError(log[-12000:])
            r=dict(cohort=c,exit=proc.returncode,log=log,signature=int(m[1]),retained=int(m[6]),budget_peak=int(m[7]))
            for key,raw,count in [('edits',m[2],1000),('scans',m[3],100),('navigation',m[4],100),('checkpoints',m[5],1000)]:
                samples=json.loads(raw);assert len(samples)==count;ordered=sorted(samples);r[key]=dict(samples_ns=samples,total_ns=sum(samples),p50_ns=ordered[(count-1)//2],p95_ns=ordered[(count*95-1)//100],p99_ns=ordered[(count*99-1)//100],maximum_ns=ordered[-1])
            for key,pattern in [('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$'),('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$')]:
                value=re.search(pattern,log,re.M)
                if value is None:raise RuntimeError(log[-12000:])
                r[key]=float(value[1]) if key!='rss_bytes' else int(value[1])
            return r
        result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
        for pair in range(5):
            for c in (['before','after'] if pair%2==0 else ['after','before']):
                r=trial(c);r['pair']=pair;result['runs'].append(r);print(json.dumps(dict(cohort=c,pair=pair,edits_ns=r['edits']['total_ns'],scans_ns=r['scans']['total_ns'],navigation_ns=r['navigation']['total_ns'])),flush=True)
        assert len({r['signature'] for r in result['warmups']+result['runs']})==1
        result['summary']={}
        for key in ['edits','scans','navigation','checkpoints','rss','cpu']:
            values=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']]
                values.append(100*(b[key]['total_ns']/a[key]['total_ns']-1) if key in ['edits','scans','navigation','checkpoints'] else b['rss_bytes']-a['rss_bytes'] if key=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'])
            result['summary'][key]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)


def main():
    parser=argparse.ArgumentParser(description=__doc__);mode=parser.add_mutually_exclusive_group(required=True);mode.add_argument('--allocation',action='store_true');mode.add_argument('--cost',action='store_true');parser.add_argument('--toolchain');parser.add_argument('--baseline',type=Path);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    if args.toolchain and not args.allocation:parser.error('--toolchain applies to --allocation')
    root=Path(__file__).resolve().parent.parent;fixture=root/'tools/fixtures/search_cost_cases.rs'
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={str(fixture.relative_to(root)):identity(fixture)})
    try:
        allocation(root,args,result) if args.allocation else cost(root,args,result,fixture.read_text());assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
