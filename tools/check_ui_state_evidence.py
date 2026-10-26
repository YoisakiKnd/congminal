#!/usr/bin/env python3
"""核对UI高亮/提示栏阶段的源码、分配、像素、成本和基线；不完成整个计划。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


def sha(data):return hashlib.sha256(data).hexdigest()


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations'
    source={str(p.relative_to(root)):sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))};manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']};reports={};identities={}
    for suffix in ['allocation','allocation-msrv','cost','build','file-quota','workspace']:
        path=folder/f'm4-ui-state-{suffix}.json';r=json.loads(path.read_text());assert r['source_sha256']==source and r['manifests_sha256']==manifests
        if 'tool_sha256' in r:
            tool='check_sftp_executor.py' if suffix in ['workspace','file-quota'] else 'check_ui_state.py';assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes())
        for key in ['fixtures','allocation_fixtures']:
            for name,item in r.get(key,{}).items():
                data=(root/name).read_bytes();assert item['sha256']==sha(data) and item['source']==data.decode('utf-8')
        reports[suffix]=r;identities[str(path.relative_to(root))]=sha(path.read_bytes())
    for suffix in ['allocation','allocation-msrv']:
        r=reports[suffix];assert r['passed'] and all(r[key]['exit']==0 for key in ['sanity','cold','warm'])
        s=r['sample'];assert 0<s['live']<=s['used'] and s['peak']<=s['budget_peak']
        assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in r['warm']['stderr']
        assert 'denied_new_allocations=0 final=0' in r['cold']['stderr']
        assert len(r['frames']['update']['samples_ns'])==5000 and len(r['frames']['draw']['samples_ns'])==500
    c=reports['cost'];assert c['passed'] and len(c['runs'])==10 and len(c['warmups'])==4
    assert len({r['signature'] for r in c['runs']+c['warmups']})==1
    for r in c['runs']+c['warmups']:
        assert r['exit']==0 and r['rss_bytes']>0 and len(r['update']['samples_ns'])==5000 and len(r['draw']['samples_ns'])==500
    assert reports['allocation']['frames']['signature']==reports['allocation-msrv']['frames']['signature']==c['runs'][0]['signature']
    b=reports['build'];assert b['passed'] and len(b['checks'])==9 and all(r['exit']==0 for r in b['checks'])
    for i,counts in {1:[550],2:[553,18],6:[22]}.items():assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',b['checks'][i]['stdout'])]==counts
    for suffix in ['workspace','file-quota']:
        r=reports[suffix];assert r['sanity']['exit']==r['measurement']['exit']==0
        assert all(s['live']<=s['parent_used'] for s in r['samples']);assert all(r['samples'][-1][k]==0 for k in ['live','parent_used','child_used'])
    assert reports['file-quota']['input']['io_headroom']==9*1024*1024 and reports['file-quota']['queue']['admitted']==13
    for key in ['local_picker','exit_view','retry','paths','lookup','review']:assert reports['workspace'][key]['paint_allocations']==reports['workspace'][key]['paint_reallocations']==0
    archive=folder/'baselines/m4-ui-state-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline=json.load(tar.extractfile('source-identity.json'));members={m.name:sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert baseline['source_sha256']==c['baseline_source_sha256']==json.loads((folder/'m4-font-map-audit.json').read_text())['source_sha256']
    assert baseline['manifests_sha256']==c['baseline_manifests_sha256']==manifests
    for name,digest in baseline['source_sha256'].items()|manifests.items():assert members[name]==digest
    assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(p.relative_to(root)):sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes=sorted(n for n in source.keys()|baseline['source_sha256'].keys() if source.get(n)!=baseline['source_sha256'].get(n))
    assert changes==['src/main.rs','src/notifications.rs','src/render.rs','src/resources/budget.rs','src/search.rs','src/shell.rs','src/ui.rs','src/ui_state.rs']
    failed=json.loads((folder/'m4-ui-state-allocation-before-pty-wait.json').read_text());assert failed['warm']['exit']!=0 and 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in failed['warm']['stderr']
    historical={}
    old=json.loads((folder/'m4-ui-state-cost-before-single-pass.json').read_text());assert old['passed'] and old['summary']['update']['median']>20
    assert [n for n in source if source[n]!=old['source_sha256'].get(n)]==['src/main.rs','src/search.rs','src/ui_state.rs']
    for suffix in ['allocation','allocation-msrv','cost','build','file-quota','workspace']:
        path=folder/f'm4-ui-state-{suffix}-before-single-pass.json';r=json.loads(path.read_text());assert r['source_sha256']==old['source_sha256'];historical[str(path.relative_to(root))]=sha(path.read_bytes())
    assert sha((folder/'baselines/ui-state-before-single-pass.rs').read_bytes())==old['source_sha256']['src/ui_state.rs']
    result=dict(passed=True,goal_complete=False,scope='Per-view UI decorations and bar buffers plus actual warm update/software draw. Search engine/query/preedit ownership, notices/shell/config storage, task/root metadata, actual native input/presentation/clipboard/platform/long-run remain outside completion.',source_sha256=source,manifests_sha256=manifests,changed_sources=changes,reports_sha256=identities,historical_reports_sha256=historical,audit_tool_sha256=sha(Path(__file__).read_bytes()),baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),cost_summary=c['summary'])
    args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(passed=True,reports=len(reports),goal_complete=False)))


if __name__=='__main__':main()
