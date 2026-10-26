#!/usr/bin/env python3
"""Bind managed LatestTask runtime evidence; does not complete all application threads or M4."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


def sha(data):return hashlib.sha256(data).hexdigest()


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);args=p.parse_args()
    root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations'
    source={str(p.relative_to(root)):sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))}
    manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']}
    reports={};identities={}
    for suffix in ['allocation','allocation-msrv','cost','build','file-quota','workspace','ui-allocation','ui-cost','preview-entry']:
        path=folder/f'm4-task-runtime-{suffix}.json';r=json.loads(path.read_text())
        assert r['source_sha256']==source and r.get('manifests_sha256',r.get('manifest_sha256'))==manifests,suffix
        tool=('check_task_runtime_build.py' if suffix=='build' else 'check_sftp_executor.py' if suffix in ['workspace','file-quota'] else 'check_auth_parent.py' if suffix=='preview-entry' else 'check_task_runtime.py')
        assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes()),suffix
        for key in ['fixtures','allocation_fixtures']:
            for name,item in r.get(key,{}).items():
                data=(root/name).read_bytes();assert item['sha256']==sha(data) and item['source']==data.decode('utf-8'),name
        for name,digest in r.get('support_tools_sha256',{}).items():assert sha((root/'tools'/name).read_bytes())==digest
        reports[suffix]=r;identities[str(path.relative_to(root))]=sha(path.read_bytes())
    for suffix in ['allocation','allocation-msrv']:
        r=reports[suffix];assert r['passed'] and r['sanity']['exit']==r['measurement']['exit']==0
        assert r['model']==dict(work_stack=2097152,reaper_stack=262144,margin=65536,registry_fee=5272,budget_size=368,stats_size=320)
        assert len(r['samples'])==9 and all(s['live']<=s['heap_fee'] and s['peak']<=s['heap_peak'] for s in r['samples'])
        latest=[s for s in r['samples'] if s['mode']=='latest'];private=[s for s in r['samples'] if s['mode']=='private']
        assert [s['step'] for s in latest]==['created','tls_held','released']
        assert latest[1]['workers']==1 and latest[1]['used']==2163024
        assert [s['step'] for s in private]==['created','tls_held','refused','capture_limit','joined','released']
        assert private[3]['workers']==2 and private[3]['used']-private[2]['used']==2097152+65536+32768+256
        assert private[2]['allocations']==private[1]['allocations'] and private[2]['reallocations']==private[1]['reallocations']
        assert private[-2]['live']==4504 and private[-2]['used']==5272
        assert all(rows[-1]['live']==rows[-1]['used']==rows[-1]['workers']==0 for rows in [latest,private])
    b=reports['build'];assert b['passed'] and len(b['checks'])==13 and all(c['exit']==0 for c in b['checks'])
    for index,counts in {1:[592],2:[595,20],6:[11],7:[20],8:[1],9:[5],10:[1]}.items():assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',b['checks'][index]['stdout'])]==counts
    for index in [8,10]:assert 'RUNTIME_UNCONFIRMED' not in b['checks'][index]['stdout'] or 'retained=2162688' in b['checks'][index]['stdout']
    for suffix in ['root-self','root-self-msrv']:
        path=folder/f'm4-task-runtime-{suffix}.json';r=json.loads(path.read_text())
        assert r['source_sha256']==source and r['manifests_sha256']==manifests and r['passed'] and r['measurement']['exit']==0
        assert r['tool_sha256']==sha((root/'tools/check_root_budget.py').read_bytes())
        for name,item in r['fixtures'].items():assert item==dict(sha256=sha((root/name).read_bytes()),source=(root/name).read_text())
        assert r['layout']==dict(budget=368,stats=320,self_fee=496)
        assert [s['live'] for s in r['samples']]==[448,512,896,4992,4992]
        assert all(s['live']<=s['used'] and s['peak']<=s['budget_peak'] for s in r['samples'])
        assert 'ROOT_FINAL refused_new_allocations=0 final=0' in r['measurement']['stderr']
        identities[str(path.relative_to(root))]=sha(path.read_bytes())
    for suffix,metrics in [('cost',['pending','polling','tokens','lifecycle']),('ui-cost',['update','draw'])]:
        c=reports[suffix];assert c['passed'] and len(c['runs'])==10 and len(c['warmups'])==4
        assert len({r['signature'] for r in c['runs']+c['warmups']})==1
        assert c['boot_adapters']=={'before':'','after':'let _runtime_owner=crate::runtime::initialize().unwrap();'}
        for pair in range(5):assert sorted(r['cohort'] for r in c['runs'] if r['pair']==pair)==['after','before']
        for r in c['runs']+c['warmups']:
            assert r['exit']==0 and r['rss_bytes']>0
            for key in metrics:
                count=5000 if key=='update' else 500 if key=='draw' else 100
                assert len(r[key]['samples_ns'])==count and sum(r[key]['samples_ns'])==r[key]['total_ns']
            if suffix=='cost':
                assert r['signature']==204200 and r['idle']['samples']>=10
                assert r['idle']['threads_min']==r['idle']['threads_max']==(3 if r['cohort']=='before' else 4)
    ui=reports['ui-allocation'];assert ui['passed'] and all(ui[k]['exit']==0 for k in ['sanity','cold','warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert ui['frames']['signature']==reports['ui-cost']['runs'][0]['signature']==6821514814623131185
    for suffix in ['file-quota','workspace']:
        r=reports[suffix];assert r['sanity']['exit']==r['measurement']['exit']==0
        assert all(s['live']<=s['parent_used'] and s['peak']<=s['parent_peak'] for s in r['samples'])
        assert all(r['samples'][-1][k]==0 for k in ['live','parent_used','child_used'])
    assert reports['file-quota']['input']['io_headroom']==9*1024*1024
    assert reports['file-quota']['queue']==dict(admitted=13,queued=11,reason='budget')
    assert reports['file-quota']['type_sizes']['budget']==368
    for key in ['local_picker','exit_view','retry','paths','lookup','review']:assert reports['workspace'][key]['paint_allocations']==reports['workspace'][key]['paint_reallocations']==0
    entry=reports['preview-entry'];assert entry['source_unchanged'] and entry['helper_build']['exit']==entry['parent_build']['exit']==0
    assert len(entry['cases'])==1 and entry['cases'][0]['passed']
    archive=folder/'baselines/m4-task-runtime-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline=json.load(tar.extractfile('source-identity.json'));members={m.name:sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert baseline['source_sha256']==json.loads((folder/'m4-root-budget-audit.json').read_text())['source_sha256']
    for suffix in ['cost','ui-cost']:
        assert reports[suffix]['baseline_source_sha256']==baseline['source_sha256']
        assert reports[suffix]['baseline_manifests_sha256']==baseline['manifests_sha256']==manifests
    for name,digest in baseline['source_sha256'].items()|manifests.items():assert members[name]==digest
    assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(p.relative_to(root)):sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    assert archive.stat().st_size==679149 and sha(archive.read_bytes())=='225cd5c10a42c6b8a74512b0ef8f38f3c34228d32098f476a51f936fa5e77868'
    changes=sorted(n for n in source.keys()|baseline['source_sha256'].keys() if source.get(n)!=baseline['source_sha256'].get(n))
    assert changes==['src/main.rs','src/resources.rs','src/resources/budget.rs','src/runtime.rs','src/runtime_tests.rs','src/tasks.rs','src/workspace/files.rs']
    old_path=folder/'m4-task-runtime-cost-before-wake-coalescing.json';old=json.loads(old_path.read_text())
    old_impl=folder/'baselines/m4-task-runtime-before-wake.rs';old_tool=folder/'baselines/check_task_runtime-before-capture-limit.py'
    assert old['passed'] and old['tool_sha256']==sha(old_tool.read_bytes())
    assert old['source_sha256']['src/runtime.rs']==sha(old_impl.read_bytes())
    assert {k:v for k,v in old['source_sha256'].items() if k!='src/runtime.rs'}=={k:v for k,v in source.items() if k!='src/runtime.rs'}
    assert old_tool.read_text().replace("len(result['samples'])==8","len(result['samples'])==9")== (root/'tools/check_task_runtime.py').read_text()
    result=dict(passed=True,goal_complete=False,source_sha256=source,manifests_sha256=manifests,changed_sources=changes,reports_sha256=identities,
                scope='LatestTask managed worker and singleton supervisor, preadmitted requested stack/heap model,64 fixed records and native join-owned fee, TLS/failure/refusal/final lifetime, current UI/SFTP regression. Not all application threads, all generic payloads/config/platform globals, full global shutdown/RSS/OS/actual stacks/native GUI/other-platform/long-run completion.',
                audit_tool_sha256=sha(Path(__file__).read_bytes()),baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),
                prior_candidate=dict(report=str(old_path.relative_to(root)),sha256=sha(old_path.read_bytes()),implementation_sha256=sha(old_impl.read_bytes()),tool_sha256=sha(old_tool.read_bytes())),
                unresolved=dict(previous_draw_allocations='Two requests in earlier SFTP all-thread paint interval remain unattributed; current repeat zero does not prove a fix.',workspace_root_remaining=reports['workspace']['samples'][-1].get('root_total'),other_threads='PTY/config watcher/layout and independent SFTP supervisor are not managed by this registry.'),
                cost_summary=reports['cost']['summary'],ui_cost_summary=reports['ui-cost']['summary'])
    args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(passed=True,reports=len(identities),goal_complete=False)))


if __name__=='__main__':main()
