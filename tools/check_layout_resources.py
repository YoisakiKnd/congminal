#!/usr/bin/env python3
"""Layout snapshots/store all-thread requested-byte proof and same-load cost."""
import argparse,hashlib,json,re,shutil,tempfile,statistics
from pathlib import Path
from check_font_discovery import sources,build
from check_sftp_executor import run,identity


def project(root,p):
 for n in ['src','assets']:shutil.copytree(root/n,p/n)
 for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(root/n,p/n)


def allocation(root,args,result):
 fixtures=[root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs','layout_resources_allocation_cases.rs']];result['fixtures']={str(p.relative_to(root)):identity(p) for p in fixtures};prefix=['rustup','run',args.toolchain] if args.toolchain else []
 result['scope']='Temporary-copy epoch all-thread Rust request meter. Fixed max8-pane/15-node snapshots,format/parse/clone/geometry10000 times zero allocation; actual8PTY-pane snapshot/fit10000 times after READY prewarm zero requests; actual Store Shared/new native thread/TLS,prepaid path scratch,nested parent creation,1000 coalesced snapshots,actual fsync/rename,retained Shared owner after native join,denied metadata rollback,finally0. Parent/context,caller path/notify/fixture tree and singleton supervisor initialized before epoch. Heap comparisons subtract requested2MiB worker stack; excludes meter headers,malloc/C/OS internals/actual stacks/RSS/native GUI/other-platform/long-run. Path discovery before Store and live UI Node conversion/other Workspace storage remain outside this ownership scope.'
 with tempfile.TemporaryDirectory(prefix='cg-layout-allocation-') as temporary:
  p=Path(temporary);project(root,p);shutil.copy2(fixtures[0],p/'src/executor_allocation_meter.rs');f=p/'src/main.rs';f.write_text(f.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n');f=p/'src/workspace/layout.rs';f.write_text(f.read_text()+'\n'+fixtures[1].read_text());result['rustc']=run(prefix+['rustc','-Vv'],p,15);binary,result['build']=build(p,prefix)
  result['sanity']=run([str(binary),'action::tests','--test-threads=1'],p,15);assert result['sanity']['exit']==0
  result['cases']={}
  for case in ['fixed_format_clone_and_geometry_use_no_rust_allocations','real_store_retains_shared_fee_and_retires_all_native_workers','failed_native_save_cleans_temporary_and_returns_all_credit','real_eight_pane_snapshot_and_restore_preflight_allocate_nothing']:
   r=run([str(binary),'workspace::layout::allocation_probe::'+case,'--exact','--nocapture','--test-threads=1'],p,45);result['cases'][case]=r;assert r['exit']==0,r['stdout']+r['stderr'];print(json.dumps(dict(case=case,exit=0)),flush=True)
  logs=''.join(r['stderr'] for r in result['cases'].values());m=re.search(r'LAYOUT_STORE loaded_live=(\d+) loaded_peak=(\d+) loaded_fee=(\d+) saved_live=(\d+) saved_peak=(\d+) saved_fee=(\d+) shared_live=(\d+) shared_fee=(\d+) peak_fee=(\d+) denied_allocations=(\d+) final=0 workers=0',logs);assert m,logs
  result['store']=dict(zip(['loaded_live','loaded_peak','loaded_fee','saved_live','saved_peak','saved_fee','shared_live','shared_fee','peak_fee','denied_allocations'],map(int,m.groups())))
  for k in ['loaded','saved','shared']:assert result['store'][k+'_live']<=result['store'][k+'_fee']
  assert max(result['store']['loaded_peak'],result['store']['saved_peak'])<=result['store']['peak_fee']


def cost(root,args,result):
 assert args.baseline
 fixture=root/'tools/fixtures/layout_resources_cost_cases.rs';result['fixtures']={str(fixture.relative_to(root)):identity(fixture)};result['baseline_source_sha256']=sources(args.baseline);result['baseline_manifests_sha256']={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']};result['attempts']=[];result['scope']='Same System fixture, frozen/current projects. Real8PTY panes/persistence store warm READY before timing;1000 snapshot create/drop,1000 unchanged dirty observations,1000 restore-fit checks,1000 parse/encode cycles and100 sequential complete fsync/rename saves. Counterpart fixed v1 bytes/final layout equal.2warmups/cohort+5alternating pairs,time-lp entire test CPU/RSS includes setup/children/teardown,not API-only/native GUI/IME/input presentation,slow filesystem deadline proof/actual stack/RSS cap/other-platform/long-run. No new app dependencies/threads/processes.'
 with tempfile.TemporaryDirectory(prefix='cg-layout-cost-') as temporary:
  result['builds']={};builds={}
  for cohort,source in [('before',args.baseline),('after',root)]:
   p=Path(temporary)/cohort;p.mkdir();project(source,p);f=p/'src/workspace.rs';f.write_text(f.read_text()+'\n'+fixture.read_text());binary,result['builds'][cohort]=build(p,[]);builds[cohort]=(p,binary)
  def trial(cohort):
   p,binary=builds[cohort];r=run(['/usr/bin/time','-lp',str(binary),'workspace::layout_cost::same_snapshot_preflight_and_save','--exact','--ignored','--nocapture','--test-threads=1'],p,90);log=r['stdout']+r['stderr'];r['cohort']=cohort;result['attempts'].append(r);assert r['exit']==0,log[-12000:];m=re.search(r'LAYOUT_COST signature=(\d+) snapshot=(\[[^\n]+\]) observe=(\[[^\n]+\]) fits=(\[[^\n]+\]) format=(\[[^\n]+\]) save=(\[[^\n]+\]) final=0',log);assert m,log[-12000:];r['signature']=int(m[1])
   for key,raw in zip(['snapshot','observe','fits','format','save'],m.groups()[1:]):v=json.loads(raw);assert len(v)==(100 if key=='save' else 1000);r[key]=dict(samples_ns=v,total_ns=sum(v))
   for key,pattern in [('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$'),('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$')]:m=re.search(pattern,log,re.M);assert m;r[key]=int(m[1]) if key=='rss_bytes' else float(m[1])
   return r
  result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
  for pair in range(5):
   for c in (['before','after'] if pair%2==0 else ['after','before']):
    r=trial(c);r['pair']=pair;result['runs'].append(r);print(json.dumps(dict(cohort=c,pair=pair,signature=r['signature'])),flush=True)
  assert len({r['signature'] for r in result['runs']+result['warmups']})==1;result['summary']={}
  for k in ['snapshot','observe','fits','format','save','rss','cpu']:
   values=[]
   for pair in range(5):
    a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']];values.append(b['rss_bytes']-a['rss_bytes'] if k=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if k=='cpu' else 100*(b[k]['total_ns']/a[k]['total_ns']-1))
   result['summary'][k]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)


def main():
 p=argparse.ArgumentParser(description=__doc__);m=p.add_mutually_exclusive_group(required=True);m.add_argument('--allocation',action='store_true');m.add_argument('--cost',action='store_true');p.add_argument('--baseline',type=Path);p.add_argument('--toolchain');p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent;result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
 try:(allocation if args.allocation else cost)(root,args,result);assert sources(root)==result['source_sha256'];result['passed']=True
 except Exception as error:result['error']=str(error);raise
 finally:args.output.write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
