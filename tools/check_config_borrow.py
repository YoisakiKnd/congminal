#!/usr/bin/env python3
"""Removed Config/Command copies: exact Session hook and verified font-assembly adapters."""
import argparse,hashlib,json,re,shutil,tempfile,statistics
from pathlib import Path
from check_font_discovery import sources,build
from check_sftp_executor import run,identity


def project(source,p):
 for n in ['src','assets']:shutil.copytree(source/n,p/n)
 for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(source/n,p/n)


def main():
 p=argparse.ArgumentParser(description=__doc__);m=p.add_mutually_exclusive_group(required=True);m.add_argument('--allocation',action='store_true');m.add_argument('--cost',action='store_true');p.add_argument('--baseline',type=Path,required=True);p.add_argument('--toolchain');p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent
 fixture=root/'tools/fixtures/config_borrow_cases.rs';cost_fixture=root/'tools/fixtures/config_borrow_cost_cases.rs';meter=root/'tools/fixtures/executor_allocation_meter.rs';result=dict(source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},baseline_source_sha256=sources(args.baseline),baseline_manifests_sha256={n:hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={str(f.relative_to(root)):identity(f) for f in ([fixture,meter] if args.allocation else [fixture,cost_fixture])},scope='Before/current temporary projects; identical32KiB environment+26x8KiB custom send-text fields. Allocation exact hook surrounds actual Session Command construction before terminal/native startup; source is verified before inserting start/stop. Isolated font producer copies exact original/current poll_fonts request assembly via recorded adapters; selected FontConfig signature equal. No Fonts loading/cancellation during that interval; actual pipeline checked by full font integration. Real PTY exec validates environment after Config drop. Cost System allocator:2warmups/cohort+5alternating pairs,1000 font request create/drop intervals and10 real Workspace add/READY/close cycles. time-lp entire process CPU/RSS includes harness/children/setup/teardown and original32KiB environment,not GUI latency or all allocator/config/parser/native launch ownership/RSS bound,other-platform/long-run acceptance. No added dependency/runtime process/thread.',adapters={'before':dict(request_type='Config',request_body='let mut config=app.config.clone();config.font=app.effective_font();config',font_body='&request.font'),'after':dict(request_type='FontConfig',request_body='app.effective_font()',font_body='request')},builds={},attempts=[])
 try:
  with tempfile.TemporaryDirectory(prefix='cg-config-borrow-') as temporary:
   builds={};prefix=['rustup','run',args.toolchain] if args.toolchain else [];result['rustc']=run(prefix+['rustc','-Vv'],root,15)
   for cohort,source in [('before',args.baseline),('after',root)]:
    q=Path(temporary)/cohort;q.mkdir();project(source,q);text=(q/'src/main.rs').read_text()
    assert ('let mut config = self.config.clone();' in text and 'try_submit(config)' in text) if cohort=='before' else ('let font = self.effective_font();' in text and 'try_submit(font)' in text)
    case=fixture.read_text();a=result['adapters'][cohort]
    for token,k in [('REQUEST_TYPE','request_type'),('REQUEST_BODY','request_body'),('FONT_BODY','font_body')]:case=case.replace('/* '+token+' */',a[k])
    if args.allocation:
     shutil.copy2(meter,q/'src/executor_allocation_meter.rs');text+='\n#[cfg(test)] mod executor_allocation_meter;\n'
     case=case.replace('// FONT_EPOCH_START','executor_allocation_meter::start();').replace('// FONT_EPOCH_STOP','let counts=executor_allocation_meter::stop();').replace('// FONT_EPOCH_RELEASED','assert_eq!(executor_allocation_meter::peek().live,0);eprintln!("BORROW_FONT_ALLOC allocations={} reallocations={} live={} peak={} released=0",counts.allocations,counts.reallocations,counts.live,counts.peak);').replace('// COMMAND_EPOCH_RELEASED','assert_eq!(executor_allocation_meter::peek().live,0);')
     f=q/'src/session.rs';s=f.read_text();needle='let command = crate::pty::'+('Command {' if cohort=='before' else 'CommandRef {');assert s.count(needle)==1
     s=s.replace(needle,'let capture=crate::config_borrow_probe::COMMAND_EPOCH.load(std::sync::atomic::Ordering::SeqCst);if capture{crate::executor_allocation_meter::start();}\n        '+needle)
     needle='        // 网格准入先于任何子进程创建';assert needle in s;s=s.replace(needle,'        if capture{let c=crate::executor_allocation_meter::stop();eprintln!("BORROW_COMMAND_ALLOC allocations={} reallocations={} live={} peak={} request_bytes={}",c.allocations,c.reallocations,c.live,c.peak,std::mem::size_of_val(&command));}\n'+needle);f.write_text(s)
    if args.cost:
     file=q/'src/workspace.rs';file.write_text(file.read_text()+'\n'+cost_fixture.read_text())
    (q/'src/main.rs').write_text(text+'\n'+case);binary,result['builds'][cohort]=build(q,prefix);builds[cohort]=(q,binary)
   if args.allocation:
    result['cases']={}
    for cohort in ['before','after']:
     result['cases'][cohort]={}
     for name in ['measured_font_request_assembly','measured_actual_session_command_assembly']:
      q,b=builds[cohort];r=run([str(b),'config_borrow_probe::'+name,'--exact','--nocapture','--test-threads=1'],q,60);result['cases'][cohort][name]=r;assert r['exit']==0,r['stdout']+r['stderr']
     logs=''.join(r['stderr'] for r in result['cases'][cohort].values());font=re.search(r'BORROW_FONT_ALLOC allocations=(\d+) reallocations=(\d+) live=(\d+) peak=(\d+) released=0',logs);command=re.search(r'BORROW_COMMAND_ALLOC allocations=(\d+) reallocations=(\d+) live=(\d+) peak=(\d+) request_bytes=(\d+)',logs);sig=re.search(r'BORROW_FONT signature=(\d+) request_bytes=(\d+) final=0 workers=0',logs);assert font and command and sig and 'BORROW_SESSION env=32768 value_length=1024 final=0 workers=0' in logs
     result.setdefault('summary',{})[cohort]=dict(font=dict(zip(['allocations','reallocations','live','peak'],map(int,font.groups()))),command=dict(zip(['allocations','reallocations','live','peak','request_bytes'],map(int,command.groups()))),font_signature=int(sig[1]),font_request_bytes=int(sig[2]))
    a,b=result['summary']['before'],result['summary']['after'];assert a['font_signature']==b['font_signature'] and a['font']['live']>200000 and b['font']['live']<1024 and a['command']['live']>32768 and all(b['command'][k]==0 for k in ['allocations','reallocations','live','peak']);print(json.dumps(result['summary']),flush=True)
   else:
    def trial(cohort):
     q,b=builds[cohort];r=run(['/usr/bin/time','-lp',str(b),'workspace::config_borrow_cost_probe::same_request_and_workspace_lifecycle','--exact','--ignored','--nocapture','--test-threads=1'],q,90);r['cohort']=cohort;result['attempts'].append(r);log=r['stdout']+r['stderr'];assert r['exit']==0,log[-12000:];m=re.search(r'BORROW_COST signature=(\d+) request=(\[[^\n]+\]) add=(\[[^\n]+\]) ready=(\[[^\n]+\]) close=(\[[^\n]+\]) final=0 workers=0',log);assert m,log[-12000:];r['signature']=int(m[1])
     for k,raw in zip(['request','add','ready','close'],m.groups()[1:]):v=json.loads(raw);assert len(v)==(1000 if k=='request' else 10);r[k]=dict(samples_ns=v,total_ns=sum(v))
     for k,pattern in [('rss_bytes',r'^\s*(\d+)\s+maximum resident set size$'),('user_seconds',r'^user\s+([\d.]+)$'),('system_seconds',r'^sys\s+([\d.]+)$')]:m=re.search(pattern,log,re.M);assert m;r[k]=int(m[1]) if k=='rss_bytes' else float(m[1])
     return r
    result['warmups']=[trial(c) for _ in range(2) for c in ['before','after']];result['runs']=[]
    for pair in range(5):
     for c in (['before','after'] if pair%2==0 else ['after','before']):
      r=trial(c);r['pair']=pair;result['runs'].append(r);print(json.dumps(dict(cohort=c,pair=pair,signature=r['signature'])),flush=True)
    assert len({r['signature'] for r in result['runs']+result['warmups']})==1;result['summary']={}
    for k in ['request','add','ready','close','rss','cpu']:
     values=[]
     for pair in range(5):
      a,b=[next(r for r in result['runs'] if r['cohort']==c and r['pair']==pair) for c in ['before','after']];values.append(b['rss_bytes']-a['rss_bytes'] if k=='rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if k=='cpu' else 100*(b[k]['total_ns']/a[k]['total_ns']-1))
     result['summary'][k]=dict(median=statistics.median(values),minimum=min(values),maximum=max(values),pairs=values)
   assert sources(root)==result['source_sha256'];result['passed']=True
 except Exception as error:result['error']=str(error);raise
 finally:args.output.write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
