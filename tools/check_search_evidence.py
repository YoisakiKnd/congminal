#!/usr/bin/env python3
"""核对搜索资源阶段的来源、实际分配、语义对照与相邻组件回归。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


def sha(data):return hashlib.sha256(data).hexdigest()


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    root=Path(__file__).resolve().parent.parent;folder=root/'docs/evaluations';source={str(p.relative_to(root)):sha(p.read_bytes()) for p in sorted((root/'src').rglob('*.rs'))};manifests={n:sha((root/n).read_bytes()) for n in ['Cargo.toml','Cargo.lock']};reports={};identities={}
    for suffix in ['allocation','allocation-msrv','cost','build','file-quota','workspace','ui-allocation']:
        path=folder/f'm4-search-resources-{suffix}.json';r=json.loads(path.read_text());assert r['source_sha256']==source and r['manifests_sha256']==manifests
        if 'tool_sha256' in r:
            tool='check_sftp_executor.py' if suffix in ['file-quota','workspace'] else 'check_ui_state.py' if suffix=='ui-allocation' else 'check_search_resources.py';assert r['tool_sha256']==sha((root/'tools'/tool).read_bytes())
        for key in ['fixtures','allocation_fixtures']:
            for name,item in r.get(key,{}).items():
                data=(root/name).read_bytes();assert item['sha256']==sha(data) and item['source']==data.decode('utf-8')
        reports[suffix]=r;identities[str(path.relative_to(root))]=sha(path.read_bytes())
    for suffix in ['allocation','allocation-msrv']:
        r=reports[suffix];assert r['passed'] and r['sanity']['exit']==r['measurement']['exit']==0
        s=r['sample'];assert 0<s['live']<=s['used'] and s['peak']<=s['budget_peak']
        assert [s['chars'] for s in r['patterns']]==[1,2,3,5,17,513,1024]
        assert all(0<s['live']<=s['used'] and s['peak']<=s['budget_peak'] for s in r['patterns'])
        assert 'SEARCH_WARM allocations=0 reallocations=0 live=0 peak=0 closed=2048 denied_new_allocations=0 final=0' in r['measurement']['stderr']
    c=reports['cost'];assert c['passed'] and len(c['runs'])==10 and len(c['warmups'])==4 and len({r['signature'] for r in c['runs']+c['warmups']})==1
    for r in c['runs']+c['warmups']:
        assert r['exit']==0 and r['rss_bytes']>0
        for key,n in [('edits',1000),('scans',100),('navigation',100),('checkpoints',1000)]:assert len(r[key]['samples_ns'])==n
    b=reports['build'];assert b['passed'] and len(b['checks'])==9 and all(r['exit']==0 for r in b['checks'])
    for index,counts in {1:[553],2:[556,18],6:[8]}.items():assert [int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',b['checks'][index]['stdout'])]==counts
    for suffix in ['workspace','file-quota']:
        r=reports[suffix];assert r['sanity']['exit']==r['measurement']['exit']==0
        assert all(s['live']<=s['parent_used'] for s in r['samples']);assert all(r['samples'][-1][key]==0 for key in ['live','parent_used','child_used'])
    assert reports['file-quota']['input']['io_headroom']==9*1024*1024 and reports['file-quota']['queue']['admitted']==13
    ui=reports['ui-allocation'];assert ui['passed'] and all(ui[key]['exit']==0 for key in ['sanity','cold','warm'])
    assert 'UI_WARM allocations=0 reallocations=0 live=0 peak=0' in ui['warm']['stderr']
    assert len(ui['frames']['update']['samples_ns'])==5000 and len(ui['frames']['draw']['samples_ns'])==500
    assert ui['frames']['signature']==json.loads((folder/'m4-ui-state-cost.json').read_text())['runs'][0]['signature']
    archive=folder/'baselines/m4-search-resources-before.tar.gz'
    with tarfile.open(archive) as tar:
        baseline=json.load(tar.extractfile('source-identity.json'));members={m.name:sha(tar.extractfile(m).read()) for m in tar if m.isfile()}
    assert baseline['source_sha256']==c['baseline_source_sha256']==json.loads((folder/'m4-ui-state-audit.json').read_text())['source_sha256'];assert baseline['manifests_sha256']==c['baseline_manifests_sha256']==manifests
    for name,digest in baseline['source_sha256'].items()|manifests.items():assert members[name]==digest
    assert {n:v for n,v in members.items() if n.startswith('assets/')}=={str(p.relative_to(root)):sha(p.read_bytes()) for p in (root/'assets').rglob('*') if p.is_file()}
    changes=sorted(n for n in source.keys()|baseline['source_sha256'].keys() if source.get(n)!=baseline['source_sha256'].get(n));assert changes==['src/main.rs','src/resources/budget.rs','src/search.rs','src/ui_state.rs','src/workspace.rs']
    result=dict(passed=True,goal_complete=False,scope='Search owned query/preedit/KMP/recent/saved/match buffers, tested history semantics, adjacent warmed UI/SFTP regressions only. Input events/clipboard, root/context/fixed metadata, notices/shell/links/config/tasks/other UI, native latency/platform/long-run outside completion.',source_sha256=source,manifests_sha256=manifests,changed_sources=changes,reports_sha256=identities,audit_tool_sha256=sha(Path(__file__).read_bytes()),baseline=dict(path=str(archive.relative_to(root)),bytes=archive.stat().st_size,sha256=sha(archive.read_bytes())),cost_summary=c['summary'])
    args.output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(passed=True,reports=len(reports),goal_complete=False)))


if __name__=='__main__':main()
