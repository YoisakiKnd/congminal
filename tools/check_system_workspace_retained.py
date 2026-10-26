#!/usr/bin/env python3
"""Diagnose the workspace epoch's retained 48-byte request, in a temporary copy only."""
import argparse,hashlib,json,re,shutil,tempfile
from pathlib import Path
from check_font_discovery import sources,build
from check_sftp_executor import run,identity,WORKSPACE_ALLOCATION

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--output',type=Path,required=True);args=p.parse_args();root=Path(__file__).resolve().parent.parent
 names=['executor_allocation_meter.rs','system_workspace_allocation_cases.rs','retained_allocation_trace.rs']
 result=dict(source_sha256=sources(root),tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),fixtures={n:identity(root/'tools/fixtures'/n) for n in names},scope='Diagnostic all-thread 48-byte live request pointer table with 256 fixed records and 32 native frames, after ordinary workspace wait timeout. Not performance evidence; no production changes. Original final-zero assertion remains; failure is retained.')
 try:
  with tempfile.TemporaryDirectory(prefix='cg-system-retained-') as temporary:
   copy=Path(temporary)
   for n in ['src','assets']:shutil.copytree(root/n,copy/n)
   for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(root/n,copy/n)
   shutil.copy2(root/'tools/fixtures/retained_allocation_trace.rs',copy/'src/retained_allocation_trace.rs')
   mainfile=copy/'src/main.rs';mainfile.write_text(mainfile.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n#[cfg(test)] mod retained_allocation_trace;\n')
   meter=(root/'tools/fixtures/executor_allocation_meter.rs').read_text().replace('pub fn start(){','pub fn start(){crate::retained_allocation_trace::begin();')
   meter=meter.replace('record(layout.size(),epoch,0,0,false);unsafe{base.add(offset)}','record(layout.size(),epoch,0,0,false);crate::retained_allocation_trace::add(unsafe{base.add(offset)},layout.size());unsafe{base.add(offset)}')
   meter=meter.replace('record(0,0,layout.size(),epoch,false);','crate::retained_allocation_trace::remove(pointer);record(0,0,layout.size(),epoch,false);')
   meter=meter.replace('record(length,epoch,layout.size(),old_epoch,true);','crate::retained_allocation_trace::remove(pointer);crate::retained_allocation_trace::add(unsafe{base.add(offset)},length);record(length,epoch,layout.size(),old_epoch,true);')
   (copy/'src/executor_allocation_meter.rs').write_text(meter)
   fixture=(root/'tools/fixtures/system_workspace_allocation_cases.rs').read_text().replace('crate::runtime::diagnostic();','crate::retained_allocation_trace::end();')
   file=copy/'src/workspace/remote_files.rs';file.write_text(file.read_text()+'\n'+fixture)
   binary,result['build']=build(copy,[]);result['measurement']=run([str(binary),WORKSPACE_ALLOCATION,'--exact','--nocapture','--test-threads=1'],copy,30)
   log=result['measurement']['stdout']+result['measurement']['stderr'];base=re.search(r'RETAINED_BASE base=(0x[\da-f]+) overflow=(true|false)',log);assert base and base[2]=='false',log[-6000:]
   result['events']=[]
   for pointer,raw in re.findall(r'RETAINED_EVENT pointer=(0x[\da-f]+) frames=([^\n]+)',log):
    frames=raw.rstrip(',').split(',');symbols=run(['atos','-o',str(binary),'-l',base[1]]+frames,copy,15);assert symbols['exit']==0;result['events'].append(dict(pointer=pointer,frames=frames,symbols=symbols['stdout']))
   print(json.dumps(dict(exit=result['measurement']['exit'],events=result['events']),indent=2),flush=True)
 finally:args.output.write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
