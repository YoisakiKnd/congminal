#!/usr/bin/env python3
"""Paired native aggregate and full monitor API costs with the System allocator."""
import argparse
import hashlib
import json
import re
import shutil
import statistics
import tempfile
from pathlib import Path
from check_font_discovery import sources,build
from check_sftp_executor import run,identity
from check_review_cost_20261029 import trial as monitor_trial


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    root=Path(__file__).resolve().parents[1]
    fixture=root/'tools/fixtures/review_cpu_cost_20261029.rs'
    result=dict(source_sha256=sources(root),baseline_source_sha256=sources(args.baseline),
        manifests={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},
        baseline_manifests={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},
        fixture=identity(fixture),tool=identity(Path(__file__)),builds={},warmups={},pairs={},
        scope='Unmodified System allocator. Same host before/current aggregate CPU+RAM APIs: five fresh '
        'construct-and-first reads then two warm and seven measured calls separated250ms (beyond old CPU cache); '
        'assert physical memory total and core count agree between cohorts, not volatile used RAM/CPU. '
        'Also existing full monitor API mix:5 cold reads/200 immediate samples/5 process reads, with old CPU '
        'cache coalescing for immediate calls. Both modes two warm trial pairs then5 alternating pairs. '
        'Native constructor mode includes construction; existing monitor cold mode excludes construction. '
        'Monitor time-lp RSS/CPU cover harness/setup/full command, not GUI or whole-process resource bounds. '
        'Process populations and disk free space can change; native aggregate formulas differ from per-core mean.')
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-cpu-cost-') as temp:
            projects={}
            for cohort,source in [('before',args.baseline),('after',root)]:
                target=Path(temp)/cohort;target.mkdir()
                for n in ['src','assets']:shutil.copytree(source/n,target/n)
                for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(source/n,target/n)
                (target/'docs').mkdir();shutil.copy2(source/'docs/configuration.md',target/'docs/configuration.md')
                text=fixture.read_text().replace('/* FRESH */',
                    'let mut source = sysinfo::System::new(); source.refresh_cpu_usage();' if cohort=='before' else
                    'let mut source = crate::monitor::native::Source::default();')
                text=text.replace('/* READ */',
                    '{ source.refresh_cpu_usage(); source.refresh_memory(); (source.global_cpu_usage(),source.cpus().len(),source.used_memory(),source.total_memory()) }' if cohort=='before' else
                    '{ let r=source.sample(&cancel).unwrap(); (r.cpu,r.cores,r.used,r.total) }')
                with (target/'src/monitor/monitor_tests.rs').open('a') as f:f.write('\n'+text)
                binary,result['builds'][cohort]=build(target,[])
                projects[cohort]=(target,binary)
            def trial(name,mode):
                target,binary=projects[name]
                if mode=='monitor':return monitor_trial(target,binary,'monitor')
                r=run([str(binary),'monitor::monitor_tests::cpu_cost::aggregate_reading','--exact','--ignored','--nocapture','--test-threads=1'],target,30)
                match=re.search(r'CPU_COST times=(\[[^\n]+?\]) cold=(\[[^\n]+?\]) cores=(\d+) total=(\d+)',r['stdout']+r['stderr'])
                if r['exit'] or not match:raise RuntimeError(str(r))
                times,cold=json.loads(match[1]),json.loads(match[2])
                if len(times)!=7 or len(cold)!=5:raise RuntimeError(str(r))
                return dict(run=r,times=times,cold=cold,cores=int(match[3]),total=int(match[4]))
            for mode in ['native','monitor']:
                result['warmups'][mode]=[{n:trial(n,mode) for n in ['before','after']} for _ in range(2)]
                result['pairs'][mode]=[]
                for i in range(5):
                    pair={n:trial(n,mode) for n in (['before','after'] if i%2==0 else ['after','before'])}
                    if mode=='native' and any(pair['before'][k]!=pair['after'][k] for k in ['cores','total']):
                        raise RuntimeError('Different core count or physical memory in cohorts')
                    result['pairs'][mode].append(pair)
                    print(json.dumps(dict(mode=mode,pair=i+1)),flush=True)
            result['summary']={}
            for mode,pairs in result['pairs'].items():
                metrics=['times','cold'] if mode=='native' else ['cold','sample-no-disk','sample-with-disk','processes']
                summary={}
                for metric in metrics:
                    def value(cohort):
                        return statistics.median(cohort[metric]) if mode=='native' else cohort['metrics'][metric]['median_us']
                    before=[value(p['before']) for p in pairs];after=[value(p['after']) for p in pairs]
                    changes=[100*(a/b-1) for a,b in zip(after,before)]
                    summary[metric]=dict(before_median=statistics.median(before),after_median=statistics.median(after),unit='ns' if mode=='native' else 'us',median_percent=statistics.median(changes),pairs_percent=changes)
                if mode=='monitor':
                    summary['rss_delta_bytes']=[p['after']['rss']-p['before']['rss'] for p in pairs]
                    summary['cpu_delta_seconds']=[p['after']['user']+p['after']['system']-p['before']['user']-p['before']['system'] for p in pairs]
                result['summary'][mode]=summary
            if sources(root)!=result['source_sha256']:raise RuntimeError('Source changed during comparison')
            result['passed']=True
    except Exception as error:
        result['error']=str(error)
        raise
    finally:
        args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
