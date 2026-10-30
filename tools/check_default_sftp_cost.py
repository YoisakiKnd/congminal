#!/usr/bin/env python3
"""macOS production binaries: alternating minimal/default local-terminal startup and idle resource comparison."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

from check_ssh_transport import sampler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--minimal', type=Path, required=True)
    parser.add_argument('--default', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    identity = lambda: {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
                        for p in sorted((root/'src').rglob('*.rs'))}
    binaries = {'minimal': args.minimal.resolve(), 'default': args.default.resolve()}
    sample, backend = sampler()
    result = dict(source_sha256=identity(), tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        binaries={name: dict(path=str(p), bytes=p.stat().st_size, sha256=hashlib.sha256(p.read_bytes()).hexdigest())
                  for name,p in binaries.items()}, sampler=backend, runs=[], warmups=[],
        scope='Actual release application binaries, same source: --no-default-features versus default sftp. Two warmups/mode, five alternating pairs. Native Cocoa window with identical fixed Menlo font/config and shell output; shell exits naturally after 5s. Child-ready is observed marker creation after shell spawn, not first presented frame or native input latency. Idle interval 2.0-4.5s; root CPU/RSS/threads and owned tree sampled every 20ms, observer excluded. Concurrent independent SFTP soak is present on the same host; raw samples and ranges retained, no claim of isolated cold startup, full-heap accounting, or all-platform acceptance.')
    try:
        with tempfile.TemporaryDirectory(prefix='cg-default-cost-') as temporary:
            folder = Path(temporary)
            marker = folder/'ready'
            shell = folder/'shell'
            shell.write_text('#!/bin/sh\nprintf "ready" > "' + str(marker) + '"\nprintf "fixed local terminal workload\\r\\n"\nsleep 5\n')
            shell.chmod(0o700)
            config = folder/'congminal.toml'
            config.write_text(f'''[font]
families = ["Menlo"]
size = 14
[window]
columns = 80
rows = 24
[shell]
program = "{shell}"
''')
            for index in range(-2,5):
                for mode in (['minimal','default'] if index%2==0 else ['default','minimal']):
                    if marker.exists(): marker.unlink()
                    environment = dict(os.environ, CONGMINAL_CONFIG=str(config))
                    environment.pop('CONGMINAL_TRACE',None)
                    started = time.monotonic_ns()
                    process = subprocess.Popen([str(binaries[mode])],cwd=folder,env=environment,
                        stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
                    samples=[]
                    ready=None
                    try:
                        while process.poll() is None:
                            elapsed=time.monotonic_ns()-started
                            if ready is None and marker.exists(): ready=elapsed
                            samples.append(dict(ns=elapsed,tree=sample(process.pid)))
                            if elapsed>10_000_000_000: raise TimeoutError('local terminal did not exit with its shell')
                            time.sleep(0.02)
                        error=process.communicate(timeout=2)[1].decode('utf-8','replace')
                        assert process.returncode==0 and ready is not None, error[-4000:]
                        idle=[s for s in samples if 2_000_000_000<=s['ns']<=4_500_000_000 and process.pid in s['tree']]
                        assert len(idle)>50
                        first,last=idle[0],idle[-1]
                        cpu=100*(last['tree'][process.pid]['cpu_ns']-first['tree'][process.pid]['cpu_ns'])/(last['ns']-first['ns'])
                        row=dict(mode=mode,pair=index,child_ready_ns=ready,exit=process.returncode,
                            root_idle_cpu_percent=cpu,root_idle_rss_peak=max(s['tree'][process.pid]['rss'] for s in idle),
                            root_idle_threads_peak=max(s['tree'][process.pid]['threads'] for s in idle),
                            tree_idle_processes_peak=max(len(s['tree']) for s in idle),samples=samples,stderr=error)
                        result['warmups' if index<0 else 'runs'].append(row)
                        print(json.dumps({k:v for k,v in row.items() if k not in ['samples','stderr']}),flush=True)
                    finally:
                        if process.poll() is None:
                            process.terminate();process.wait(timeout=5)
            result['summary']={mode:{key:dict(median=statistics.median(r[key] for r in result['runs'] if r['mode']==mode),
                minimum=min(r[key] for r in result['runs'] if r['mode']==mode), maximum=max(r[key] for r in result['runs'] if r['mode']==mode))
                for key in ['child_ready_ns','root_idle_cpu_percent','root_idle_rss_peak','root_idle_threads_peak','tree_idle_processes_peak']}
                for mode in binaries}
    finally:
        result['source_unchanged']=result['source_sha256']==identity()
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_text(json.dumps(result,indent=2)+'\n')
    assert result['source_unchanged']


if __name__=='__main__': main()
