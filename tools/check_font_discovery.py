#!/usr/bin/env python3
"""字体发现：冻结源码同负载对照及临时请求计量；不改生产分配器。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from check_sftp_executor import run, identity

CASE='font::discovery_cost::same_catalog_queries_and_loaded_glyphs'


def sources(root):return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}


def project(source,destination,fixture):
    for name in ['src','assets']:shutil.copytree(source/name,destination/name)
    for name in ['Cargo.toml','Cargo.lock']:shutil.copy2(source/name,destination/name)
    p=destination/'src/font/mod.rs';p.write_text(p.read_text()+'\n'+fixture)


def build(root,prefix):
    r=run(prefix+['cargo','test','--offline','--release','--no-run','--message-format=json'],root,240)
    artifacts=[json.loads(line) for line in r['stdout'].splitlines() if line.startswith('{')]
    record=dict(exit=r['exit'],stderr=r['stderr'],diagnostics=[a['message'].get('rendered','') for a in artifacts if a.get('reason')=='compiler-message'])
    if r['exit']:raise RuntimeError(str(record))
    return Path(next(a['executable'] for a in artifacts if a.get('reason')=='compiler-artifact' and a.get('target',{}).get('name')=='congminal' and a.get('executable'))),record


def trial(root,binary,mode):
    env=dict(os.environ,CONGMINAL_FONT_MODE=mode)
    if sys.platform!='darwin':raise RuntimeError('CPU/RSS comparison currently uses macOS /usr/bin/time -lp')
    p=subprocess.run(['/usr/bin/time','-lp',str(binary),CASE,'--exact','--ignored','--nocapture','--test-threads=1'],cwd=root,env=env,capture_output=True,text=True,timeout=60)
    log=p.stdout+p.stderr
    match=re.search(r'FONT_DISCOVERY_COST mode=(scan|lookup|load) times=(\[[^\n]+\]) signature=(\d+) faces=(\d+) names=(\d+) budget_peak=(\d+) final=0',log)
    if p.returncode or not match or match[1]!=mode:raise RuntimeError(log[-12000:])
    times=json.loads(match[2]);assert len(times)==5
    def number(pattern):
        found=re.search(pattern,log,re.M)
        if found is None:raise RuntimeError(log[-12000:])
        return float(found[1])
    return dict(exit=p.returncode,log=log,mode=mode,times_ns=times,total_ns=sum(times),signature=int(match[3]),faces=int(match[4]),names=int(match[5]),budget_peak=int(match[6]),user_seconds=number(r'^user\s+([\d.]+)$'),system_seconds=number(r'^sys\s+([\d.]+)$'),maximum_rss_bytes=int(number(r'^\s*(\d+)\s+maximum resident set size$')))


def costs(root,args,result,fixture):
    if args.baseline is None:raise RuntimeError('--cost requires frozen --baseline project')
    result['scope']='Same native system-font directory and default load settings, before/current temporary copies. Per scan trial five fresh catalogs; cached lookup trial five batches of1000 queries after catalog warmup; load trial five fresh default Fonts plus independently verified A/Chinese/snowman/emoji glyph fingerprint after timing. Scan/catalog ordering normalized for content fingerprint only; query total is sum of measured API intervals, excludes signature checks. Two warmups/cohort/mode then five alternating pairs. macOS time -lp reports entire test-process CPU (0.01s resolution) and high-water RSS including harness, verification and drops; those are not API-only or steady-state metrics. No runtime child process/library added. Source font files/environment are local, not vendored or immutable; this is not cold disk, GUI/input-to-display, other-platform, long-run or complete RSS/adoption evidence.'
    result['baseline_source_sha256']=sources(args.baseline)
    result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-font-cost-') as temp:
        result['rustc']=run(['rustc','-Vv'],Path(temp),15)
        builds={};result['builds']={}
        for cohort,source in [('before',args.baseline),('after',root)]:
            p=Path(temp)/cohort;p.mkdir();text=fixture.replace('// FONT_DISCOVERY_STATUS','eprintln!("FONT_DISCOVERY_STATUS limited={}",faces.limited);' if cohort=='after' else '')
            project(source,p,text);binary,built=build(p,[]);builds[cohort]=(p,binary);result['builds'][cohort]=built
        result['warmups'],result['runs']=[],[]
        for mode in ['scan','lookup','load']:
            for i in range(2):
                for cohort in ['before','after']:
                    r=trial(*builds[cohort],mode);r['cohort']=cohort;result['warmups'].append(r)
            for pair in range(5):
                for cohort in (['before','after'] if pair%2==0 else ['after','before']):
                    r=trial(*builds[cohort],mode);r.update(cohort=cohort,pair=pair);result['runs'].append(r)
                    print(json.dumps(dict(mode=mode,cohort=cohort,pair=pair,total_ns=r['total_ns'],faces=r['faces'])),flush=True)
        result['summary']={}
        for mode in ['scan','lookup','load']:
            pairs=[]
            for pair in range(5):
                a,b=[next(r for r in result['runs'] if r['mode']==mode and r['pair']==pair and r['cohort']==c) for c in ['before','after']]
                if a['signature']!=b['signature'] or a['faces']!=b['faces'] or a['names']!=b['names']:raise RuntimeError('catalog/query/loaded-glyph fingerprints differ')
                if mode=='scan' and b['log'].count('FONT_DISCOVERY_STATUS limited=false')!=5:raise RuntimeError('ordinary system catalog limited')
                pairs.append(100*(b['total_ns']/a['total_ns']-1))
            result['summary'][mode]=dict(median_percent=statistics.median(pairs),minimum_percent=min(pairs),maximum_percent=max(pairs),pairs_percent=pairs)
            for label,measure in [('rss_delta_bytes',lambda r:r['maximum_rss_bytes']),('cpu_delta_seconds',lambda r:r['user_seconds']+r['system_seconds'])]:
                deltas=[]
                for pair in range(5):
                    a,b=[next(r for r in result['runs'] if r['mode']==mode and r['pair']==pair and r['cohort']==c) for c in ['before','after']]
                    deltas.append(measure(b)-measure(a))
                result['summary'][mode][label]=dict(median=statistics.median(deltas),minimum=min(deltas),maximum=max(deltas),pairs=deltas)


def allocations(root,args,result,fixture):
    result['scope']='Temporary-copy epoch-tagged all-thread Rust requested-byte meter, conservative realloc overlap. Generated sfnt with64x1024B preferred names and real bounded name/prefix/OS-header path, retained shared FaceInfo after catalog drop, then actual native system-font catalog and selected shared record lifetime, quota rejection with zero new allocations, final zero. Parent budget/Scope/env/fs globals/fixture bytes preinitialized, no font mappings/raster/window. Cold work reservation1MiB; actual owned metadata/control-block/index costs remain until last shared reference. Excludes added meter headers/allocator/C/OS/actual stack and opaque environment/system internals; not complete font pipeline, whole-app/RSS/FD cancellation or cross-platform runtime proof.'
    prefix=['rustup','run',args.toolchain] if args.toolchain else []
    meter=root/'tools/fixtures/executor_allocation_meter.rs';case=root/'tools/fixtures/font_discovery_allocation_cases.rs'
    result['allocation_fixtures']={str(p.relative_to(root)):identity(p) for p in [meter,case]}
    with tempfile.TemporaryDirectory(prefix='cg-font-allocation-') as temp:
        p=Path(temp);project(root,p,'')
        shutil.copy2(meter,p/'src/executor_allocation_meter.rs');main=p/'src/main.rs';main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        target=p/'src/font/discover.rs';target.write_text(target.read_text()+'\n'+case.read_text())
        result['rustc']=run(prefix+['rustc','-Vv'],p,15);binary,result['build']=build(p,prefix)
        result['sanity']=run([str(binary),'action::tests','--test-threads=1'],p,15)
        result['unicode_query']=run([str(binary),'font::discover::discovery_allocation::unicode_query_conversion_peak_is_admitted_before_allocating','--exact','--nocapture','--test-threads=1'],p,15)
        result['measurement']=run([str(binary),'font::discover::discovery_allocation::actual_discovery_results_shared_refs_denial_and_cleanup_stay_funded','--exact','--nocapture','--test-threads=1'],p,30)
        log=result['measurement']['stdout']+result['measurement']['stderr'];keys=['live','peak','used','budget_peak','allocations','reallocations']
        result['samples']=[dict(step=m[0],**dict(zip(keys,map(int,m[1:])))) for m in re.findall(r'FONT_DISCOVERY_ALLOC step=(\w+) live=(-?\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)',log)]
        if result['sanity']['exit'] or result['unicode_query']['exit'] or result['measurement']['exit'] or len(result['samples'])!=6 or 'FONT_DISCOVERY_FINAL live=0 used=0 denied_new_allocations=0' not in log:raise RuntimeError(str(result['unicode_query'])+log[-12000:])
        if any(s['live']<0 or s['live']>s['used'] or s['peak']>s['budget_peak'] for s in result['samples']):raise RuntimeError('unbacked font metadata')
        if result['samples'][-1]['live'] or result['samples'][-1]['used']:raise RuntimeError('font metadata not released')


def main():
    parser=argparse.ArgumentParser(description=__doc__);mode=parser.add_mutually_exclusive_group(required=True);mode.add_argument('--cost',action='store_true');mode.add_argument('--allocation',action='store_true');parser.add_argument('--toolchain');parser.add_argument('--baseline',type=Path);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    if args.toolchain and not args.allocation:parser.error('--toolchain applies to --allocation')
    root=Path(__file__).resolve().parent.parent;fixture=root/'tools/fixtures/font_discovery_cost_cases.rs'
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixture={str(fixture.relative_to(root)):identity(fixture)})
    try:
        (costs if args.cost else allocations)(root,args,result,fixture.read_text())
        if sources(root)!=result['source_sha256']:raise RuntimeError('source changed during measurement')
        result['passed']=True
    finally:
        args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
