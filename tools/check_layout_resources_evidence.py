#!/usr/bin/env python3
"""Verify fixed layout snapshots/store evidence, not full M4 or roadmap completion."""
import argparse,hashlib,json,re,tarfile
from pathlib import Path


def sha(data):return hashlib.sha256(data).hexdigest()


def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations';source={str(f.relative_to(root)):sha(f.read_bytes()) for f in sorted((root/'src').rglob('*.rs'))};manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']};reports={};ids={}
 for label in ['allocation','allocation-msrv','build','cost','workspace','ui-allocation','preview-entry']:
  file=folder/f'm4-layout-resources-{label}.json';r=json.loads(file.read_text());assert r['source_sha256']==source and r.get('manifests_sha256',r.get('manifest_sha256'))==manifests,label
  tool='check_layout_resources_build.py' if label=='build' else 'check_system_executor.py' if label=='workspace' else 'check_system_threads.py' if label=='ui-allocation' else 'check_auth_parent.py' if label=='preview-entry' else 'check_layout_resources.py'
  assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes()),label
  for key in ['fixtures','allocation_fixtures']:
   for name,item in r.get(key,{}).items():assert item==dict(sha256=sha((root/name).read_bytes()),source=(root/name).read_text()),name
  for name,digest in r.get('support_tools_sha256',{}).items():assert sha((root/'tools'/name).read_bytes())==digest
  reports[label]=r;ids[str(file.relative_to(root))]=sha(file.read_bytes())
 for label in ['allocation','allocation-msrv']:
  r=reports[label];assert r['passed'] and r['sanity']['exit']==0 and len(r['cases'])==4 and all(c['exit']==0 for c in r['cases'].values());log=''.join(c['stderr'] for c in r['cases'].values())
  assert 'LAYOUT_INLINE iterations=10000 allocations=0 reallocations=0 live=0 peak=0 layout_bytes=128 buffer_bytes=4104' in log
  assert 'LAYOUT_UI panes=8 iterations=10000 allocations=0 reallocations=0 live=0 peak=0 final=0 workers=0' in log
  m=re.search(r'LAYOUT_PATH bytes=(\d+) over_limit_refused=true final=0',log);assert m and 800<int(m[1])<1024
  m=re.search(r'LAYOUT_IO_FAILURE peak=(\d+) fee=(\d+) final=0 workers=0 temporary=0',log);assert m and int(m[1])<=int(m[2])
  s=r['store'];assert s['shared_live']==s['shared_fee']==520 and s['denied_allocations']==1
  for key in ['loaded','saved','shared']:assert s[key+'_live']<=s[key+'_fee']
  assert max(s['loaded_peak'],s['saved_peak'])<=s['peak_fee']
 b=reports['build'];assert b['passed'] and len(b['checks'])==10 and all(c['exit']==0 for c in b['checks'])
 for index,counts in {1:[600],2:[603,20],6:[10],7:[34]}.items():assert list(map(int,re.findall(r'test result: ok\. (\d+) passed;',b['checks'][index]['stdout'])))==counts,index
 cost=reports['cost'];assert cost['passed'] and len(cost['runs'])==10 and len(cost['warmups'])==4 and len(cost['attempts'])==14
 assert len({r['signature'] for r in cost['runs']+cost['warmups']})==1
 for pair in range(5):assert sorted(r['cohort'] for r in cost['runs'] if r['pair']==pair)==['after','before']
 for r in cost['runs']+cost['warmups']:
  assert r['exit']==0 and r['rss_bytes']>0
  for k in ['snapshot','observe','fits','format','save']:assert len(r[k]['samples_ns'])==(100 if k=='save' else 1000) and sum(r[k]['samples_ns'])==r[k]['total_ns']
 w=reports['workspace'];assert w['sanity']['exit']==w['measurement']['exit']==0 and len(w['samples'])==7 and all(0<=s['live']<=s['parent_used'] and s['peak']<=s['parent_peak'] for s in w['samples'])
 assert all(w['samples'][-1][k]==0 for k in ['live','parent_used','child_used','root_total'])
 assert 'WORKSPACE_NATIVE managed_workers=0 root_used=0 active=0' in w['measurement']['stderr']
 for key in ['paths','lookup','review','retry','exit_view','local_picker']:assert w[key]['paint_allocations']==w[key]['paint_reallocations']==0
 ui=reports['ui-allocation'];assert ui['passed'] and all(ui[k]['exit']==0 for k in ['sanity','cold','warm']) and 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr'] and ui['frames']['signature']==6821514814623131185
 entry=reports['preview-entry'];assert entry['source_unchanged'] and entry['helper_build']['exit']==entry['parent_build']['exit']==0 and len(entry['cases'])==1 and entry['cases'][0]['passed']
 archive=folder/'baselines/m4-layout-resources-before.tar.gz'
 with tarfile.open(archive) as t:
  base=json.load(t.extractfile('source-identity.json'));members={m.name:sha(t.extractfile(m).read()) for m in t if m.isfile()}
 assert archive.stat().st_size==688537 and sha(archive.read_bytes())=='3d24c36c5e33923bc1e4b9f47839148d2cd891e49ecf2e467f864d8f06e1fcd2'
 assert base['source_sha256']==json.loads((folder/'m4-system-threads-audit.json').read_text())['source_sha256']
 assert base['manifests_sha256']==cost['baseline_manifests_sha256']==manifests and cost['baseline_source_sha256']==base['source_sha256']
 for name,digest in base['source_sha256'].items()|manifests.items():assert members[name]==digest,name
 assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(f.relative_to(root)):sha(f.read_bytes()) for f in (root/'assets').rglob('*') if f.is_file()}
 changes=sorted(n for n in source.keys()|base['source_sha256'].keys() if source.get(n)!=base['source_sha256'].get(n));assert changes==['src/workspace.rs','src/workspace/layout.rs']
 for name in ['build-before-clippy','allocation-before-native-path-limit','cost-before-wait-tuple']:
  file=folder/f'm4-layout-resources-{name}.json';historical=json.loads(file.read_text());assert not historical.get('passed',False);ids[str(file.relative_to(root))]=sha(file.read_bytes())
 regression=folder/'m4-layout-resources-cost-before-compact-snapshot.json';old=json.loads(regression.read_text());assert old['passed'] and old['tool_sha256']==cost['tool_sha256'] and old['fixtures']==cost['fixtures'] and old['baseline_source_sha256']==cost['baseline_source_sha256']
 assert old['source_sha256']['src/workspace/layout.rs']!=source['src/workspace/layout.rs'] and {n:v for n,v in old['source_sha256'].items() if n!='src/workspace/layout.rs'}=={n:v for n,v in source.items() if n!='src/workspace/layout.rs'}
 assert old['summary']['snapshot']['minimum']>70 and old['summary']['observe']['minimum']>50
 assert cost['summary']['snapshot']['median']<0 and cost['summary']['observe']['median']<0
 ids[str(regression.relative_to(root))]=sha(regression.read_bytes())
 failure=json.loads((folder/'m4-layout-resources-allocation-before-native-path-limit.json').read_text());assert failure['cases']['real_store_retains_shared_fee_and_retires_all_native_workers']['exit']!=0 and 'InvalidFilename' in failure['error']
 result=dict(passed=True,goal_complete=False,source_sha256=source,manifests_sha256=manifests,changed_sources=changes,reports_sha256=ids,audit_tool_sha256=sha(Path(__file__).read_bytes()),baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),cost_summary=cost['summary'],scope='Fixed8-pane/15-node layout snapshots,4KiB fixed encoding/static errors,owned Store Shared and bounded IO paths,version non-wrap and admitted-before-IO failure/real native retirement.7 current reports; historical counterexamples retained separately. macOS PATH_MAX1024/NAME_MAX255 measured locally: native success909-byte path,not4KiB native path acceptance. Not all UI tree/session/config/platform allocation accounting,native Windows/Linux,hard filesystem/close latency,GUI input/presentation/RSS/long-run or roadmap completion.',unresolved=['Config/parser/ordinary Cancellation and launcher/platform/other UI allocation ownership','Live Node restoration/Workspace containers and input-to-presentation latency','M5/M6 remainder,M7/M8,terminal compatibility,native other-platform and long tests'])
 args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(passed=True,current_reports=len(reports),goal_complete=False)))
if __name__=='__main__':main()
