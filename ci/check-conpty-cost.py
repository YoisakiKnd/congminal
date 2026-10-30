#!/usr/bin/env python3
"""Fresh-parent system/bundled ConPTY A/B. Windows OS counters; no production dependencies."""
import argparse,ctypes as c,json,os,re,statistics,subprocess,time
from ctypes import wintypes as w
from pathlib import Path
assert os.name=='nt'
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--metadata',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--pairs',type=int,default=5);args=p.parse_args()
exe=None
for line in args.metadata.read_text(encoding='utf-8').splitlines():
 try:item=json.loads(line)
 except ValueError:continue
 if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='congminal' and item.get('executable'):exe=item['executable']
assert exe,'No main test executable'
k=c.WinDLL('kernel32',use_last_error=True);ps=c.WinDLL('psapi',use_last_error=True)
class Entry(c.Structure):
 _fields_=[('size',w.DWORD),('usage',w.DWORD),('pid',w.DWORD),('heap',c.c_size_t),('module',w.DWORD),('threads',w.DWORD),('parent',w.DWORD),('priority',w.LONG),('flags',w.DWORD),('name',w.WCHAR*260)]
class Memory(c.Structure):
 _fields_=[('size',w.DWORD),('faults',w.DWORD)]+[(n,c.c_size_t) for n in ['peak','working','peak_paged','paged','peak_nonpaged','nonpaged','pagefile','peak_pagefile','private']]
k.CreateToolhelp32Snapshot.argtypes=[w.DWORD,w.DWORD];k.CreateToolhelp32Snapshot.restype=w.HANDLE
k.Process32FirstW.argtypes=[w.HANDLE,c.POINTER(Entry)];k.Process32NextW.argtypes=[w.HANDLE,c.POINTER(Entry)]
k.CloseHandle.argtypes=[w.HANDLE];k.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD];k.OpenProcess.restype=w.HANDLE
k.GetProcessTimes.argtypes=[w.HANDLE]+[c.POINTER(w.FILETIME)]*4
ps.GetProcessMemoryInfo.argtypes=[w.HANDLE,c.POINTER(Memory),w.DWORD]
def snapshot(root):
 h=k.CreateToolhelp32Snapshot(2,0);rows=[]
 if h==c.c_void_p(-1).value:return []
 try:
  entry=Entry();entry.size=c.sizeof(entry);more=k.Process32FirstW(h,c.byref(entry))
  while more:
   rows.append((entry.pid,entry.parent,entry.threads,entry.name));more=k.Process32NextW(h,c.byref(entry))
 finally:k.CloseHandle(h)
 owned={root}
 for _ in range(16):
  new=owned|{pid for pid,parent,_,_ in rows if parent in owned}
  if new==owned:break
  owned=new
 result=[]
 for pid,parent,threads,name in rows:
  if pid not in owned:continue
  h=k.OpenProcess(0x1000|0x10,False,pid)
  if not h:continue
  try:
   mem=Memory();mem.size=c.sizeof(mem);a,b,kt,ut=[w.FILETIME() for _ in range(4)]
   if ps.GetProcessMemoryInfo(h,c.byref(mem),c.sizeof(mem)) and k.GetProcessTimes(h,c.byref(a),c.byref(b),c.byref(kt),c.byref(ut)):
    ticks=((kt.dwHighDateTime<<32)|kt.dwLowDateTime)+((ut.dwHighDateTime<<32)|ut.dwLowDateTime)
    result.append(dict(pid=pid,parent=parent,name=name,threads=threads,working=mem.working,private=mem.private,cpu_ticks=ticks))
  finally:k.CloseHandle(h)
 return result
records=[]
for pair in range(args.pairs):
 for mode in (['system','bundled'] if pair%2==0 else ['bundled','system']):
  started=time.monotonic();env=dict(os.environ,CONGMINAL_COST_MODE=mode)
  child=subprocess.Popen([exe,'--exact','graphics::native_tests::conpty_cost_parent','--ignored','--nocapture'],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
  samples=[]
  while child.poll() is None:
   samples.append(dict(seconds=time.monotonic()-started,processes=snapshot(child.pid)))
   if time.monotonic()-started>15:child.kill();raise RuntimeError('ConPTY cost fixture timeout')
   time.sleep(.05)
  log=child.communicate()[0].decode('utf-8',errors='replace');assert child.returncode==0,log
  match=re.search(r'CONPTY_COST mode='+mode+r' ready_us=(\d+)',log);assert match,log
  records.append(dict(pair=pair,mode=mode,ready_us=int(match[1]),samples=samples,log=log))
summary={}
for mode in ['system','bundled']:
 group=[v for v in records if v['mode']==mode]
 def peak(v,key):return max((sum(p[key] for p in s['processes']) for s in v['samples']),default=0)
 summary[mode]=dict(ready_us_median=statistics.median(v['ready_us'] for v in group),tree_working_peak_median=statistics.median(peak(v,'working') for v in group),tree_private_peak_median=statistics.median(peak(v,'private') for v in group),tree_threads_peak=max(peak(v,'threads') for v in group),processes_peak=max(len(s['processes']) for v in group for s in v['samples']),process_names=sorted({p['name'] for v in group for s in v['samples'] for p in s['processes']}))
args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(dict(scope='Windows native PTY fresh parent and observed descendants, generated ready/2s idle child, 5 alternating pairs. OS working/private counters are separate from Rust budgets; sampling may miss short-lived processes. No terminal input-latency claim.',summary=summary,records=records),indent=2),encoding='utf-8');print(json.dumps(summary))
