#!/usr/bin/env python3
"""Bind removed configuration-copy evidence, not complete parser/config resource admission."""
import argparse,hashlib,json,re,tarfile
from pathlib import Path


def sha(data):return hashlib.sha256(data).hexdigest()


def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations';source={str(f.relative_to(root)):sha(f.read_bytes()) for f in sorted((root/'src').rglob('*.rs'))};manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']};reports={};ids={}
 for label in ['build','allocation','allocation-msrv','cost','ui-allocation','workspace','ssh-positive','ssh-interrupt','preview-entry']:
  file=folder/f'm4-config-borrow-{label}.json';r=json.loads(file.read_text());assert r['source_sha256']==source,label
  if not label.startswith('ssh-'):assert r.get('manifests_sha256',r.get('manifest_sha256'))==manifests,label
  tool='check_config_borrow_build.py' if label=='build' else 'check_system_threads.py' if label=='ui-allocation' else 'check_system_executor.py' if label=='workspace' else 'check_ssh_transport.py' if label.startswith('ssh-') else 'check_auth_parent.py' if label=='preview-entry' else 'check_config_borrow.py'
  assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes()),label
  for key in ['fixtures','allocation_fixtures']:
   for name,item in r.get(key,{}).items():assert item==dict(sha256=sha((root/name).read_bytes()),source=(root/name).read_text()),name
  for name,digest in r.get('support_tools_sha256',{}).items():assert sha((root/'tools'/name).read_bytes())==digest
  reports[label]=r;ids[str(file.relative_to(root))]=sha(file.read_bytes())
 b=reports['build'];assert b['passed'] and len(b['checks'])==12 and all(c['exit']==0 for c in b['checks'])
 for index,counts in {1:[601],2:[604,20],6:[10],7:[34],8:[10],9:[1]}.items():assert list(map(int,re.findall(r'test result: ok\. (\d+) passed;',b['checks'][index]['stdout'])))==counts,index
 assert 'borrowed_launch_overrides_dimensions_and_survives_source_drop ... ok' in b['checks'][8]['stdout']
 for label in ['allocation','allocation-msrv']:
  r=reports[label];assert r['passed'] and len(r['cases'])==2 and all(len(c)==2 and all(t['exit']==0 for t in c.values()) for c in r['cases'].values())
  a,z=r['summary']['before'],r['summary']['after'];assert a['font_signature']==z['font_signature']==4385046365196518928
  assert a['command']==dict(allocations=69,reallocations=0,live=34764,peak=34764,request_bytes=96)
  assert z['command']==dict(allocations=0,reallocations=0,live=0,peak=0,request_bytes=64)
  assert a['font']==dict(allocations=106,reallocations=0,live=253142,peak=253290) and z['font']==dict(allocations=5,reallocations=0,live=148,peak=148)
  assert a['font_request_bytes']==1432 and z['font_request_bytes']==40
 c=reports['cost'];assert c['passed'] and len(c['warmups'])==4 and len(c['runs'])==10 and len(c['attempts'])==14
 assert {r['signature'] for r in c['warmups']+c['runs']}=={4385046365196518928}
 for pair in range(5):assert sorted(r['cohort'] for r in c['runs'] if r['pair']==pair)==['after','before']
 for r in c['warmups']+c['runs']:
  assert r['exit']==0 and r['rss_bytes']>0
  for k in ['request','add','ready','close']:assert len(r[k]['samples_ns'])==(1000 if k=='request' else 10) and sum(r[k]['samples_ns'])==r[k]['total_ns']
 w=reports['workspace'];assert w['sanity']['exit']==w['measurement']['exit']==0 and all(0<=s['live']<=s['parent_used'] and s['peak']<=s['parent_peak'] for s in w['samples'])
 assert all(w['samples'][-1][k]==0 for k in ['live','parent_used','child_used','root_total']) and 'WORKSPACE_NATIVE managed_workers=0 root_used=0 active=0' in w['measurement']['stderr']
 for k in ['paths','lookup','review','retry','exit_view','local_picker']:assert w[k]['paint_allocations']==w[k]['paint_reallocations']==0
 ui=reports['ui-allocation'];assert ui['passed'] and all(ui[k]['exit']==0 for k in ['sanity','cold','warm']) and 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr'] and ui['frames']['signature']==6821514814623131185
 for label in ['ssh-positive','ssh-interrupt']:
  r=reports[label];assert r['exit_code']==0 and not r['observed_client_children_still_live'] and 'SSH_NATIVE_CLEANUP used=0 mappings=0' in r['log']
 assert len(reports['ssh-positive']['rounds'])==5 and 'SSH_NATIVE_INTERRUPTED_AND_RECOVERED' in reports['ssh-interrupt']['log']
 e=reports['preview-entry'];assert e['source_unchanged'] and e['helper_build']['exit']==e['parent_build']['exit']==0 and len(e['cases'])==1 and e['cases'][0]['passed']
 archive=folder/'baselines/m4-config-borrow-before.tar.gz'
 with tarfile.open(archive) as t:
  base=json.load(t.extractfile('source-identity.json'));members={m.name:sha(t.extractfile(m).read()) for m in t if m.isfile()}
 assert archive.stat().st_size==692526 and sha(archive.read_bytes())=='118cc0e6d0060fd9aeafea6186f8fedf48d4d20e1f90e25d1d21f7c44106c5e0'
 assert base['source_sha256']==json.loads((folder/'m4-layout-resources-audit.json').read_text())['source_sha256'] and base['manifests_sha256']==manifests
 for name,digest in base['source_sha256'].items()|manifests.items():assert members[name]==digest,name
 assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(f.relative_to(root)):sha(f.read_bytes()) for f in (root/'assets').rglob('*') if f.is_file()}
 for label in ['allocation','allocation-msrv','cost']:assert reports[label]['baseline_source_sha256']==base['source_sha256'] and reports[label]['baseline_manifests_sha256']==manifests and reports[label]['adapters']==c['adapters']
 changes=sorted(n for n in source.keys()|base['source_sha256'].keys() if source.get(n)!=base['source_sha256'].get(n));assert changes==['src/config/mod.rs','src/main.rs','src/pty/mod.rs','src/pty/unix.rs','src/pty/windows.rs','src/session.rs','src/workspace.rs','src/workspace/remote.rs']
 for name in ['src/main.rs','src/workspace.rs','src/workspace/remote.rs']:assert '.config.clone()' not in (root/name).read_text()
 text=(root/'src/config/mod.rs').read_text()
 for name in ['Config','ShellConfig']:assert re.search(r'#\[cfg_attr\(test, derive\(Clone\)\)\]\npub struct '+name+r'\b',text)
 result=dict(passed=True,goal_complete=False,source_sha256=source,manifests_sha256=manifests,changed_sources=changes,reports_sha256=ids,audit_tool_sha256=sha(Path(__file__).read_bytes()),baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),allocation_summary=reports['allocation']['summary'],cost_summary=c['summary'],scope='Synchronous borrowed launch Config/Shell/CommandRef at default/new pane/restore/SSH replacement,smaller font input,production Config/ShellConfig Clone removed;exact actual command allocation hook plus verified font producer adapters,full tests/real local source-drop,isolated SSH+SFTP regression. Remaining148B font copy,config/parser/native argument/environment conversion and other metadata are not fully admitted;not native Windows/Linux/GUI/presentation/RSS/long-run or roadmap completion.',legacy_ssh_evidence='SSH tool captures source hashes but not Cargo manifests; manifests unchanged versus frozen baseline and current full build,not a new dependency claim.',unresolved=['Config loading/parser/persistent output/new-old overlap and quota refusal behavior','Remaining font request clones and native launcher/environment allocations','M4 remainder,M5/M6 remainder,M7/M8,terminal compatibility,native other-platform and long tests'])
 args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(passed=True,current_reports=len(reports),goal_complete=False)))
if __name__=='__main__':main()
