#!/usr/bin/env python3
"""Bind PTY/watcher/layout native retirement evidence to the current source; not whole roadmap completion."""
import argparse,hashlib,json,re,tarfile
from pathlib import Path


def sha(data):return hashlib.sha256(data).hexdigest()


def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations'
 source={str(p.relative_to(root)):sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))};manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']};reports={};identities={}
 for label in ['allocation','allocation-msrv','build','file-quota','workspace','ui-allocation','cost','ui-cost','pty-cost','preview-entry']:
  file=folder/f'm4-system-threads-{label}.json';r=json.loads(file.read_text());assert r['source_sha256']==source and r.get('manifests_sha256',r.get('manifest_sha256'))==manifests,label
  tool='check_system_threads_build.py' if label=='build' else 'check_system_executor.py' if label in ['file-quota','workspace'] else 'check_auth_parent.py' if label=='preview-entry' else 'check_system_pty_cost.py' if label=='pty-cost' else 'check_system_threads.py'
  assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes()),label
  for key in ['fixtures','allocation_fixtures']:
   for name,item in r.get(key,{}).items():assert item==dict(sha256=sha((root/name).read_bytes()),source=(root/name).read_text()),name
  for name,digest in r.get('support_tools_sha256',{}).items():assert digest==sha((root/'tools'/name).read_bytes())
  reports[label]=r;identities[str(file.relative_to(root))]=sha(file.read_bytes())
 for label in ['allocation','allocation-msrv']:
  r=reports[label];assert r['passed'] and r['sanity']['exit']==r['measurement']['exit']==0
  assert r['model']==dict(work_stack=2097152,reaper_stack=262144,margin=65536,registry_fee=5784,budget_size=368,stats_size=320)
  assert len(r['samples'])==11 and all(0<=s['live']<=s['heap_fee'] and s['peak']<=s['heap_peak'] for s in r['samples'])
  latest=[s for s in r['samples'] if s['mode']=='latest'];private=[s for s in r['samples'] if s['mode']=='private']
  assert [s['step'] for s in latest]==['created','tls_held','released'] and latest[1]['workers']==1 and latest[1]['used']==2163008
  assert [s['step'] for s in private]==['created','tls_held','refused','capture_limit','control_started','joined_with_control','control_released','released']
  assert private[2]['allocations']==private[1]['allocations'] and private[2]['reallocations']==private[1]['reallocations']
  assert private[3]['used']-private[2]['used']==2097152+65536+32768+256 and private[4]['workers']==3
  assert private[5]['used']==71576 and private[5]['workers']==0 and private[6]['used']==5784
  for rows in [latest,private]:assert rows[-1]['live']==rows[-1]['used']==rows[-1]['workers']==0
 b=reports['build'];assert b['passed'] and len(b['checks'])==16 and all(c['exit']==0 for c in b['checks'])
 for index,counts in {1:[597],2:[600,20],6:[11],7:[20],8:[1],9:[7],10:[1],11:[9],12:[1],13:[1]}.items():assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',b['checks'][index]['stdout'])]==counts,index
 for label in ['file-quota','workspace']:
  r=reports[label];assert r['sanity']['exit']==r['measurement']['exit']==0 and len(r['samples'])==7
  assert all(0<=s['live']<=s['parent_used'] and s['peak']<=s['parent_peak'] for s in r['samples'])
  assert all(r['samples'][-1][k]==0 for k in ['live','parent_used','child_used'])
 assert reports['file-quota']['input']['io_headroom']==9*1024*1024 and reports['file-quota']['queue']==dict(admitted=13,queued=11,reason='budget')
 w=reports['workspace'];assert w['samples'][-1]['root_total']==0 and 'WORKSPACE_NATIVE managed_workers=0 root_used=0 active=0' in w['measurement']['stderr']
 for k in ['local_picker','paths','lookup','review','retry','exit_view']:assert w[k]['paint_allocations']==w[k]['paint_reallocations']==0
 ui=reports['ui-allocation'];assert ui['passed'] and all(ui[k]['exit']==0 for k in ['sanity','cold','warm']) and 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
 assert ui['frames']['signature']==6821514814623131185
 for label,metrics in [('cost',['pending','polling','tokens','lifecycle']),('ui-cost',['update','draw']),('pty-cost',['spawn','ready','bulk','close'])]:
  r=reports[label];assert r['passed'] and len(r['runs'])==10 and len(r['warmups'])==4
  assert len({v['signature'] for v in r['runs']+r['warmups']})==1
  for pair in range(5):assert sorted(v['cohort'] for v in r['runs'] if v['pair']==pair)==['after','before']
  for v in r['runs']+r['warmups']:
   assert v['exit']==0 and v['rss_bytes']>0
   for k in metrics:
    count=5000 if k=='update' else 500 if k=='draw' else 10 if label=='pty-cost' else 100
    assert len(v[k]['samples_ns'])==count and sum(v[k]['samples_ns'])==v[k]['total_ns']
   if label=='cost':assert v['idle']['samples']>=10 and v['idle']['threads_min']==v['idle']['threads_max']==4
   if label=='pty-cost':assert v['bytes']==524301
  if label!='pty-cost':assert r['boot_adapters']=={c:'let _runtime_owner=crate::runtime::initialize().unwrap();' for c in ['before','after']}
 assert reports['ui-cost']['runs'][0]['signature']==ui['frames']['signature']
 entry=reports['preview-entry'];assert entry['source_unchanged'] and entry['helper_build']['exit']==entry['parent_build']['exit']==0 and len(entry['cases'])==1 and entry['cases'][0]['passed']
 archive=folder/'baselines/m4-system-threads-before.tar.gz'
 with tarfile.open(archive) as t:
  baseline=json.load(t.extractfile('source-identity.json'));members={m.name:sha(t.extractfile(m).read()) for m in t if m.isfile()}
 assert baseline['source_sha256']==json.loads((folder/'m4-task-runtime-audit.json').read_text())['source_sha256']
 for name,digest in baseline['source_sha256'].items()|manifests.items():assert members[name]==digest,name
 assert baseline['manifests_sha256']==manifests and archive.stat().st_size==685310 and sha(archive.read_bytes())=='c499a9fa9b3e2dc43560931361ceb0a6caac37f355ca92a422afcfd862fabbca'
 assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(p.relative_to(root)):sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
 for label in ['cost','ui-cost','pty-cost']:assert reports[label]['baseline_source_sha256']==baseline['source_sha256'] and reports[label]['baseline_manifests_sha256']==manifests
 changes=sorted(n for n in source.keys()|baseline['source_sha256'].keys() if source.get(n)!=baseline['source_sha256'].get(n))
 assert changes==['src/main.rs','src/pty/flow.rs','src/pty/unix.rs','src/pty/windows.rs','src/runtime.rs','src/runtime_tests.rs','src/session.rs','src/tasks.rs','src/workspace.rs','src/workspace/layout.rs']
 # Counterexamples are historical evidence, deliberately not asserted to match current fixtures/source.
 old=folder/'m4-system-threads-build-before-test-process-isolation.json';failed=json.loads(old.read_text());assert failed['checks'][1]['exit']!=0 and '1 failed' in failed['checks'][1]['stdout']
 diagnostic=folder/'m4-system-threads-workspace-retained.json';d=json.loads(diagnostic.read_text());assert d['measurement']['exit']!=0 and len(d['events'])==1
 assert 'Condvar' in d['events'][0]['symbols'] and 'test_gate' in d['events'][0]['symbols'] and 'live=48' in d['measurement']['stderr']
 assert d['tool_sha256']==sha((root/'tools/check_system_workspace_retained.py').read_bytes())
 for label in ['before-test-process-isolation','before-wait-diagnostic','before-final-log']:
  name=('build' if label=='before-test-process-isolation' else 'workspace')+'-'+label
  file=folder/f'm4-system-threads-{name}.json';assert file.is_file();identities[str(file.relative_to(root))]=sha(file.read_bytes())
 identities[str(diagnostic.relative_to(root))]=sha(diagnostic.read_bytes())
 result=dict(passed=True,goal_complete=False,source_sha256=source,manifests_sha256=manifests,changed_sources=changes,reports_sha256=identities,audit_tool_sha256=sha(Path(__file__).read_bytes()),baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),cost_summary={k:reports[k]['summary'] for k in ['cost','ui-cost','pty-cost']},scope='Common managed PTY reader/writer/waiter, watcher/layout; pre-reservation, owner scope, native-join ticket lifetime and retained cancellation controls. Fixed64 worker records and separate existing SFTP executor. Test session process isolation is cfg(test), first synchronization prewarmed before workspace epoch. Not every allocation/generic payload/config/layout/platform object, native Windows/Linux, hard syscall deadlines/GUI close latency, actual stacks/RSS or whole roadmap acceptance.',unresolved=['All generic payload/config/layout/platform allocation accounting and UI destructors','Native GUI/IME/SSH/SFTP, Windows/Linux and long tests','Earlier two SFTP paint requests remain unattributed; passing current rounds alone is not their diagnosis','M4 remainder, M5/M6 remainder, M7/M8 and terminal compatibility'])
 args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(passed=True,current_reports=len(reports),goal_complete=False)))

if __name__=='__main__':main()
