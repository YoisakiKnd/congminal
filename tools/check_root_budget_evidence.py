#!/usr/bin/env python3
"""Audit root-budget metadata evidence and retain the unresolved prior draw observation."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations'
    source={str(p.relative_to(root)):sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))}
    manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']}
    reports={};identities={}
    for suffix in ['allocation','allocation-msrv','cost','build','file-quota','ui-allocation','ui-cost','preview-entry','directory-helper']:
        path=folder/f'm4-root-budget-{suffix}.json';r=json.loads(path.read_text())
        assert r['source_sha256']==source and r.get('manifests_sha256',r.get('manifest_sha256'))==manifests,suffix
        tool=('check_root_budget_build.py' if suffix=='build' else 'check_sftp_executor.py' if suffix=='file-quota' else 'check_ui_state.py' if suffix.startswith('ui-') else 'check_auth_parent.py' if suffix=='preview-entry' else 'check_local_directory.py' if suffix=='directory-helper' else 'check_root_budget.py')
        assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes()),suffix
        for key in ['fixtures','allocation_fixtures']:
            for name,item in r.get(key,{}).items():
                data=(root/name).read_bytes();assert item['sha256']==sha(data) and item['source']==data.decode('utf-8')
        reports[suffix]=r;identities[str(path.relative_to(root))]=sha(path.read_bytes())
    for suffix in ['allocation','allocation-msrv']:
        r=reports[suffix];assert r['passed'] and r['measurement']['exit']==0
        assert r['layout']==dict(budget=352,stats=304,self_fee=480)
        assert [s['step'] for s in r['samples']]==['root','payload','child','child_full','root_owner_dropped']
        assert [s['live'] for s in r['samples']]==[432,496,864,4960,4960]
        assert [s['used'] for s in r['samples']]==[480,544,5056,5056,5056]
        assert all(s['live']<=s['used'] and s['peak']<=s['budget_peak'] and s['reallocations']==0 for s in r['samples'])
        assert 'ROOT_FINAL refused_new_allocations=0 final=0' in r['measurement']['stderr']
    for suffix,metrics in [('cost',[('roots',100),('leases',100),('snapshots',100)]),('ui-cost',[('update',5000),('draw',500)])]:
        r=reports[suffix];assert r['passed'] and len(r['runs'])==10 and len(r['warmups'])==4
        assert len({x['signature'] for x in r['runs']+r['warmups']})==1
        for pair in range(5):assert sorted(x['cohort'] for x in r['runs'] if x['pair']==pair)==['after','before']
        for x in r['runs']+r['warmups']:
            assert x['exit']==0 and x['rss_bytes']>0
            for key,count in metrics:assert len(x[key]['samples_ns'])==count and sum(x[key]['samples_ns'])==x[key]['total_ns']
    assert {x['signature'] for x in reports['cost']['runs']}=={320000}
    assert reports['cost']['adapters']=={'before':'fn root(limit:usize)->Arc<Budget>{Budget::new(limit,0)}','after':'fn root(limit:usize)->Arc<Budget>{Budget::root(limit,0).unwrap()}'}
    b=reports['build'];assert b['passed'] and len(b['checks'])==10 and all(x['exit']==0 for x in b['checks'])
    for index,counts in {1:[585],2:[588,20],6:[11],7:[20]}.items():
        assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',b['checks'][index]['stdout'])]==counts
    file=reports['file-quota'];assert file['sanity']['exit']==file['measurement']['exit']==0
    assert file['input']['io_headroom']==9*1024*1024 and file['queue']==dict(admitted=13,queued=11,reason='budget')
    assert file['type_sizes']['budget']==352
    assert all(s['live']<=s['parent_used'] and s['peak']<=s['parent_peak'] for s in file['samples'])
    assert all(file['samples'][-1][k]==0 for k in ['live','parent_used','child_used'])
    ui=reports['ui-allocation'];assert ui['passed'] and all(ui[k]['exit']==0 for k in ['sanity','cold','warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert ui['frames']['signature']==reports['ui-cost']['runs'][0]['signature']==6821514814623131185
    entry=reports['preview-entry'];assert entry['source_unchanged'] and entry['helper_build']['exit']==entry['parent_build']['exit']==0
    assert len(entry['cases'])==1 and all(c['passed'] for c in entry['cases'])
    directory=reports['directory-helper'];assert directory['passed'] and not directory['adopted']
    assert all(directory[k]['exit']==0 for k in ['helper_build','minimal_build','test_build','unit','actual_helper'])
    assert len(directory['same_process_pairs'])==7 and directory['helper_kind']=='minimal shared-reader prototype'
    archive=folder/'baselines/m4-root-budget-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline=json.load(tar.extractfile('source-identity.json'))
        members={m.name:sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert baseline['source_sha256']==json.loads((folder/'m4-task-resources-audit.json').read_text())['source_sha256']
    for suffix in ['cost','ui-cost']:
        assert reports[suffix]['baseline_source_sha256']==baseline['source_sha256']
        assert reports[suffix]['baseline_manifests_sha256']==baseline['manifests_sha256']==manifests
    for name,digest in baseline['source_sha256'].items()|manifests.items():assert members[name]==digest
    assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(p.relative_to(root)):sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    assert archive.stat().st_size==678487 and sha(archive.read_bytes())=='0c66dd0ab6f1eddb2bd39fc2b9f1002866425084558deb5b9c97d8f693dcb58f'
    changes=sorted(n for n in source.keys()|baseline['source_sha256'].keys() if source.get(n)!=baseline['source_sha256'].get(n))
    assert changes==['src/local_directory/isolated.rs','src/main.rs','src/resources/budget.rs','src/ssh/helper_main.rs']
    trace_path=folder/'m4-task-draw-allocation-trace.json';trace=json.loads(trace_path.read_text())
    assert trace['source_sha256']==baseline['source_sha256'] and trace['passed'] and trace['calibration']['exit']==0
    assert trace['tool_sha256']==sha((root/'tools/check_workspace_draw_allocations.py').read_bytes())
    for name,item in trace['fixtures'].items():assert item==dict(source=(root/name).read_text(),sha256=sha((root/name).read_bytes()))
    assert trace['paints']==20000 and len(trace['runs'])==10
    assert all(r['exit']==0 and r['events']==[] and r['path_allocations']==r['path_reallocations']==0 for r in trace['runs'])
    result=dict(passed=True,goal_complete=False,source_sha256=source,manifests_sha256=manifests,changed_sources=changes,
                scope='Production independent root Budget/Arc/first Mutex metadata self-admission, anchored child avoids double charge; current full build, cold allocations, cost and adjacent UI/SFTP entry. Not thread runtime/other locks/config/all metadata/native/long-run completion. Prior two draw allocations remain unattributed; ten diagnostic expanded runs failed to reproduce, not a source attribution or proof of permanent zero.',
                reports_sha256=identities,audit_tool_sha256=sha(Path(__file__).read_bytes()),
                baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),
                prior_source_diagnostic=dict(path=str(trace_path.relative_to(root)),sha256=sha(trace_path.read_bytes()),resolved=False),
                cost_summary=reports['cost']['summary'],ui_cost_summary=reports['ui-cost']['summary'])
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(dict(passed=True,reports=len(reports),goal_complete=False)))


if __name__=='__main__':main()
