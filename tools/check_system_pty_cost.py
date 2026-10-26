#!/usr/bin/env python3
"""Same-load real local PTY construction,512KiB pipe/VT and close API costs; no GUI latency claim."""
import argparse,hashlib,json,re,shutil,statistics,tempfile
from pathlib import Path
from check_font_discovery import build,sources
from check_sftp_executor import run,identity


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--baseline',type=Path,required=True);p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent
    fixture=root/'tools/fixtures/system_pty_cost_cases.rs'
    result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},baseline_source_sha256=sources(args.baseline),baseline_manifests_sha256={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={str(fixture.relative_to(root)):identity(fixture)},scope='Identical System fixture in frozen/current source copies. Shared supervisor preinitialized both; one session warmup,10 real /bin/sh -> stty raw/-echo -> exec cat sessions/trial,512KiB ASCII plus marker, actual Session.poll VT parsing/no scrollback and final screen hash. Constructor API,constructor-to-ready,bulk-write-to-visible-marker,close API; pool settlement outside intervals and not equal native-ack guarantee in old implementation. Current test-only separate-session-case gate is included, not production overhead.2warmups/cohort then5 alternating pairs; time-lp entire CPU/RSS includes setup,hash,pool/native settlement and children. stty/cat are fixture dependencies only. Not cold disk/native window/IME/full app input latency,actual stack/OS/RSS cap,other-platform/long-run acceptance.')
    try:
        with tempfile.TemporaryDirectory(prefix='cg-system-pty-cost-') as temporary:
            builds={};result['builds']={}
            for c,source in [('before',args.baseline),('after',root)]:
                q=Path(temporary)/c;q.mkdir()
                for n in ['src','assets']:shutil.copytree(source/n,q/n)
                for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(source/n,q/n)
                file=q/'src/session.rs';file.write_text(file.read_text()+'\n'+fixture.read_text());binary,result['builds'][c]=build(q,[]);builds[c]=(q,binary)
            def trial(c):
                q,binary=builds[c];r=run(['/usr/bin/time','-lp',str(binary),'session::system_cost_probe::same_pty_lifecycle_and_bulk','--exact','--ignored','--nocapture','--test-threads=1'],q,90);log=r['stdout']+r['stderr']
                m=re.search(r'PTY_COST signature=(\d+) spawn=(\[[^\n]+\]) ready=(\[[^\n]+\]) bulk=(\[[^\n]+\]) close=(\[[^\n]+\]) bytes=(\d+) final=0',log);assert r['exit']==0 and m,log[-12000:]
                r.update(cohort=c,signature=int(m[1]),bytes=int(m[6]));assert r['bytes']==524301
                for key,raw in zip(['spawn','ready','bulk','close'],m.groups()[1:5]):values=json.loads(raw);assert len(values)==10;r[key]=dict(samples_ns=values,total_ns=sum(values))
                for key,pattern in [('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$'),('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$')]:v=re.search(pattern,log,re.M);assert v;r[key]=int(v[1]) if key=='rss_bytes' else float(v[1])
                return r
            result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
            for pair in range(5):
                for c in (['before','after'] if pair%2==0 else ['after','before']):
                    r=trial(c);r['pair']=pair;result['runs'].append(r);print(json.dumps(dict(cohort=c,pair=pair,bulk_ns=r['bulk']['total_ns'])),flush=True)
            assert len({r['signature'] for r in result['runs']+result['warmups']})==1;result['summary']={}
            for key in ['spawn','ready','bulk','close','rss','cpu']:
                values=[]
                for pair in range(5):
                    a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']];values.append(b['rss_bytes']-a['rss_bytes'] if key=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if key=='cpu' else 100*(b[key]['total_ns']/a[key]['total_ns']-1))
                result['summary'][key]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)
            assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
