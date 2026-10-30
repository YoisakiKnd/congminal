#!/usr/bin/env python3
"""Five paired full-monitor API trials; no allocation meter in timing copies."""
import argparse,hashlib,json,re,statistics,tempfile
from pathlib import Path
from check_font_discovery import sources,build
from check_sftp_executor import run,identity
from check_review_cost_20261029 import project,trial


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline',type=Path,required=True);parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();root=Path(__file__).resolve().parents[1];fixture=root/'tools/fixtures/review_monitor_bounds_cost_20261030.rs'
    result=dict(source_sha256=sources(root),baseline_source_sha256=sources(args.baseline),manifests={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},baseline_manifests={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},fixture=identity(fixture),tool=identity(Path(__file__)),builds={},shapes={},warmups=[],pairs=[],scope='System allocator. Same host/default public monitor API mix:5 cold sample calls after construction,200 immediate calls (disk every10),5 process refreshes. Two warm trial pairs then5 alternating pairs. Cold excludes source construction. Root/read-only-parent selection bug corrected; old/new disk shapes are recorded, not falsely required identical. Same core count/memory scope asserted; process populations/free space/access can change. macOS time-lp CPU/RSS cover full command/harness/setup, not only APIs, native memory bounds or GUI/exit latency.')
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-bounds-cost-') as temp:
            projects={}
            for name,source in [('before',args.baseline),('after',root)]:
                target=Path(temp)/name;target.mkdir();project(source,target,'')
                with (target/'src/monitor/monitor_tests.rs').open('a') as f:f.write('\n'+fixture.read_text())
                binary,result['builds'][name]=build(target,[]);projects[name]=(target,binary)
                r=run([str(binary),'monitor::monitor_tests::bounds_shape::live_shape','--exact','--ignored','--nocapture','--test-threads=1'],target,60)
                match=re.search(r'BOUNDS_SHAPE cores=(\d+) memory=(\d+) disk=(\d+) used=(\d+)',r['stdout']+r['stderr'])
                if r['exit'] or not match:raise RuntimeError(str(r))
                result['shapes'][name]=dict(run=r,cores=int(match[1]),memory=int(match[2]),disk=int(match[3]),used=int(match[4]))
            if any(result['shapes']['before'][k]!=result['shapes']['after'][k] for k in ['cores','memory']):raise RuntimeError('Different host metric scope')
            for _ in range(2):result['warmups'].append({n:trial(*projects[n],'monitor') for n in ['before','after']})
            for i in range(5):
                result['pairs'].append({n:trial(*projects[n],'monitor') for n in (['before','after'] if i%2==0 else ['after','before'])});print(json.dumps(dict(pair=i+1)),flush=True)
            result['summary']={}
            for metric in ['cold','sample-no-disk','sample-with-disk','processes']:
                before=[p['before']['metrics'][metric]['median_us'] for p in result['pairs']];after=[p['after']['metrics'][metric]['median_us'] for p in result['pairs']];changes=[100*(a/b-1) for a,b in zip(after,before)]
                result['summary'][metric]=dict(before_median_us=statistics.median(before),after_median_us=statistics.median(after),paired_median_percent=statistics.median(changes),pairs_percent=changes)
            result['summary']['rss_delta_bytes']=[p['after']['rss']-p['before']['rss'] for p in result['pairs']]
            result['summary']['cpu_delta_seconds']=[p['after']['user']+p['after']['system']-p['before']['user']-p['before']['system'] for p in result['pairs']]
            if sources(root)!=result['source_sha256']:raise RuntimeError('Source changed during comparison')
            result['passed']=True
    except Exception as error:result['error']=str(error);raise
    finally:args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
