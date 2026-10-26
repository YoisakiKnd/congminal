#!/usr/bin/env python3
"""同DTO/execute的同步控制和真实任务池，生成回环环境、两次预热及五组交替。"""
import argparse
import getpass
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time

from check_auth_helper import environment
from check_sftp_executor import run
from check_ssh_transport import sampler

CASE = 'sftp::service::cost::identical_file_jobs_compare_direct_execution_with_bounded_pool'
PHASES = ['starting', 'idle_before', 'upload', 'download', 'idle_after', 'cleanup']


def distribution(values):
    return dict(values=values,median=statistics.median(values),min=min(values),max=max(values))


def summarize(result):
    result['paired_percent'], result['paired_absolute_ns'], result['resources'] = {}, {}, {}
    for field in ['start_ns','upload_ns','download_ns','cleanup_ns','total_ns']:
        differences, ratios = [], []
        for pair in range(5):
            rows = {r['mode']:r for r in result['runs'] if r['pair'] == pair}
            differences.append(rows['pool']['cost'][field]-rows['direct']['cost'][field])
            ratios.append(100*(rows['pool']['cost'][field]/rows['direct']['cost'][field]-1))
        result['paired_absolute_ns'][field] = distribution(differences)
        # 极小启动/回收控制的百分比不作为采用结论；保留原始绝对值。
        if field in ['upload_ns','download_ns','total_ns']:
            result['paired_percent'][field] = distribution(ratios)
    fields = ['cpu_observed_ns','cpu_percent_single_core','rss_sum_peak','rss_sum_median','root_threads_median','processes_max']
    for phase in ['idle_before','upload','download','idle_after']:
        result['resources'][phase] = {}
        for part in ['client','server']:
            result['resources'][phase][part] = {}
            for field in fields:
                rows = [r for r in result['runs'] if not r['warmup']]
                cell = {mode:distribution([r['phases'][phase][part][field] for r in rows if r['mode']==mode]) for mode in ['direct','pool']}
                delta = []
                for pair in range(5):
                    paired = {r['mode']:r for r in rows if r['pair']==pair}
                    delta.append(paired['pool']['phases'][phase][part][field]-paired['direct']['phases'][phase][part][field])
                cell['paired_delta'] = distribution(delta)
                result['resources'][phase][part][field] = cell


def metrics(samples, phase, field, root_pid):
    rows = [s for s in samples if s['phase'] == phase and root_pid in s[field]]
    if phase.startswith('idle') and rows:
        low, high = rows[0]['ns']+50_000_000, rows[-1]['ns']-50_000_000
        rows = [s for s in rows if low <= s['ns'] <= high]
    if len(rows) < 3:
        return dict(samples=len(rows), available=False)
    pids = set().union(*(set(s[field]) for s in rows))
    deltas = {}
    for pid in pids:
        values = [s[field][pid]['cpu_ns'] for s in rows if pid in s[field]]
        deltas[str(pid)] = max(values)-min(values)
    duration = rows[-1]['ns']-rows[0]['ns']
    rss = [sum(p['rss'] for p in s[field].values()) for s in rows]
    threads = [s[field][root_pid]['threads'] for s in rows]
    return dict(available=True, samples=len(rows), span_ns=duration,
                cpu_observed_deltas_ns=deltas, cpu_observed_ns=sum(deltas.values()),
                cpu_percent_single_core=100*sum(deltas.values())/duration,
                rss_sum_peak=max(rss), rss_sum_median=statistics.median(rss),
                root_threads_median=statistics.median(threads), root_threads_min=min(threads), root_threads_max=max(threads),
                processes_max=max(len(s[field]) for s in rows))


def trial(root, binary, helper, folder, mode, sample, server_pid):
    env = environment()
    env.update(CONGMINAL_AUTH_ROOT=str(folder), CONGMINAL_AUTH_HELPER=str(helper),
               CONGMINAL_AUTH_CONFIG=str(folder/'cost.config'), CONGMINAL_AUTH_SCENARIO=mode)
    started = time.monotonic_ns()
    process = subprocess.Popen([binary, CASE, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding='utf-8', errors='replace', start_new_session=True)
    lock = threading.Lock()
    state, events, lines = ['preparing'], [], []
    overflow = threading.Event()
    def read():
        count = 0
        for line in process.stdout:
            with lock:
                count += len(line)
                if count <= 256*1024:
                    lines.append(line)
                else:
                    overflow.set()
                match = re.search(r'SFTP_SERVICE_PHASE phase=(\w+)', line)
                if match:
                    state[0] = match[1]
                    events.append(dict(ns=time.monotonic_ns()-started, phase=match[1]))
    reader = threading.Thread(target=read)
    reader.start()
    samples = []
    timed_out = False
    try:
        while process.poll() is None:
            elapsed = time.monotonic_ns()-started
            with lock:
                phase = state[0]
            samples.append(dict(ns=elapsed, phase=phase, tree=sample(process.pid), server_tree=sample(server_pid)))
            if elapsed > 30_000_000_000 or overflow.is_set():
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                break
            time.sleep(.005)
        process.wait(timeout=5)
        reader.join(timeout=5)
        if reader.is_alive():
            raise RuntimeError('cost output reader did not finish')
        log = ''.join(lines)
        found = re.search(r'SFTP_SERVICE_COST mode=(direct|pool) bytes=(\d+) requests=(\d+) start_ns=(\d+) upload_ns=(\d+) download_ns=(\d+) cleanup_ns=(\d+) total_ns=(\d+) peak=(\d+) final=0', log)
        observed = set().union(*(set(s['tree']) for s in samples)) if samples else set()
        still_live = []
        for pid in observed-{process.pid}:
            try:
                os.kill(pid, 0)
                still_live.append(pid)
            except ProcessLookupError:
                pass
        record = dict(mode=mode, pid=process.pid, server_pid=server_pid, exit=process.returncode, timed_out=timed_out,
                      log=log, events=events, samples=samples, observed_children_still_live=still_live)
        record['passed'] = not process.returncode and not timed_out and found is not None and not still_live and not overflow.is_set()
        if found:
            record['cost'] = dict(zip(['bytes','requests','start_ns','upload_ns','download_ns','cleanup_ns','total_ns','managed_peak'],map(int,found.groups()[1:])))
            c = record['cost']
            record['passed'] &= found[1] == mode and c['bytes'] == 33554432 and c['requests'] == 2 and c['managed_peak'] <= 16777216
            record['passed'] &= c['total_ns'] == sum(c[key] for key in ['start_ns','upload_ns','download_ns','cleanup_ns'])
        record['passed'] &= [e['phase'] for e in events] == PHASES+['finished'] and '1 passed' in log
        record['phases'] = {p:dict(client=metrics(samples,p,'tree',process.pid), server=metrics(samples,p,'server_tree',server_pid)) for p in PHASES}
        record['passed'] &= all(record['phases'][p]['client']['available'] and record['phases'][p]['server']['available'] for p in ['idle_before','upload','download','idle_after'])
        return record
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
        reader.join(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != 'darwin' or not Path('/usr/sbin/sshd').is_file():
        parser.error('this CPU/RSS sampler requires native macOS and an isolated local sshd')
    root = Path(__file__).resolve().parent.parent
    def sources():
        return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root/'src').rglob('*.rs'))}
    result = dict(scope='Actual System allocator, identical shared prepare_file/capture/execute upload and download file pipeline. Each trial generates and prepares 32MiB sources before measured phases; same explicit profile, encrypted key, immediate simulated bounded frontend, helper/private IPC/OpenSSH and known-host checks, source/mtime/type guards, no-replace atomic destination, explicit cleanup and final tracked budget zero. Direct control runs synchronously on harness test thread, no queue/workers; pool uses actual Service with two workers and one reaper, same 16MiB child/input ceiling/9MiB policy. Direct lacks queue/backpressure/cancel/GUI contract and is not a deployable UI alternative. Start API, two full file job lifecycles (including connection/auth and common frontend thread), pool joined cleanup measured; 300ms idle before/after excluded from active total. Byte verification after phases. Two warmups each, five alternating pairs. Client and separate owned server sampled every 5ms, up to64pids/tree. CPU sums per-observed-pid min/max deltas: lower bound missing short-lived/edge CPU, not complete child lifetime CPU; RSS is sampled sums, not whole-process memory hard cap or allocator accounting. No native window/input presentation, real remote latency, hostile/fault equivalence or 30-minute idle claim.',
                  platform=platform.platform(),source_sha256=sources(),tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  helper_tools_sha256={n:hashlib.sha256((root/'tools'/n).read_bytes()).hexdigest() for n in ['check_auth_helper.py','check_sftp_executor.py','check_ssh_transport.py']},
                  manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},runs=[])
    server = None
    try:
        result['rustc'] = run(['rustc','-Vv'],root,15)
        result['ssh'] = run([shutil.which('ssh'),'-V'],root,15)
        result['helper_build'] = run(['cargo','build','--offline','--release','--features','ssh-auth-helper','--bin','congminal-ssh-helper'],root,180)
        if result['helper_build']['exit']:
            raise RuntimeError(result['helper_build']['stderr'])
        result['build'] = run(['cargo','test','--offline','--release','--no-run','--message-format=json'],root,180)
        if result['build']['exit']:
            raise RuntimeError(result['build']['stdout'][-12000:]+result['build']['stderr'])
        artifacts = [json.loads(line) for line in result['build']['stdout'].splitlines() if line.startswith('{')]
        binary = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target',{}).get('name') == 'congminal' and a.get('executable'))
        helper = root/'target/release/congminal-ssh-helper'
        result['executables'] = {str(p.relative_to(root)):dict(bytes=p.stat().st_size,sha256=hashlib.sha256(p.read_bytes()).hexdigest()) for p in [Path(binary),helper]}
        sample,result['sampler'] = sampler()
        with tempfile.TemporaryDirectory(prefix='cgsc-',dir='/tmp') as temporary:
            folder = Path(temporary).resolve()
            os.chmod(folder,0o700)
            for name,phrase in [('host',''),('client','fixture only')]:
                subprocess.run(['ssh-keygen','-q','-t','ed25519','-N',phrase,'-f',str(folder/name)],check=True)
            (folder/'authorized_keys').write_bytes((folder/'client.pub').read_bytes())
            with socket.socket() as probe:
                probe.bind(('127.0.0.1',0))
                port = probe.getsockname()[1]
            public = (folder/'host.pub').read_text().split()
            (folder/'cost.known').write_text(f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
            (folder/'cost.config').write_text(f'Host fixture\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile "{folder}/client"\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile "{folder}/cost.known"\n GlobalKnownHostsFile none\n StrictHostKeyChecking yes\n NumberOfPasswordPrompts 2\n LogLevel ERROR\n')
            (folder/'sshd.config').write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {folder}/host\nAuthorizedKeysFile {folder}/authorized_keys\nPidFile {folder}/pid\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY no\nSubsystem sftp internal-sftp\nLogLevel VERBOSE\n')
            expanded = subprocess.run(['/usr/sbin/sshd','-T','-f',str(folder/'sshd.config')],capture_output=True,text=True,timeout=10)
            if expanded.returncode:
                raise RuntimeError(expanded.stderr)
            if any(line.startswith('persourcepenalties ') for line in expanded.stdout.splitlines()):
                with (folder/'sshd.config').open('a') as out:
                    out.write('PerSourcePenalties no\n')
            with (folder/'server.log').open('w') as log:
                server = subprocess.Popen(['/usr/sbin/sshd','-D','-e','-f',str(folder/'sshd.config')],stdout=log,stderr=log,start_new_session=True)
                for _ in range(100):
                    if server.poll() is not None:
                        raise RuntimeError('owned sshd exited')
                    try:
                        with socket.create_connection(('127.0.0.1',port),timeout=.05):
                            break
                    except OSError:
                        time.sleep(.01)
                else:
                    raise RuntimeError('owned sshd not ready')
                for index in range(7):
                    for mode in (['direct','pool'] if index % 2 == 0 else ['pool','direct']):
                        record = trial(root,binary,helper,folder,mode,sample,server.pid)
                        record.update(warmup=index<2,pair=None if index<2 else index-2)
                        result['runs'].append(record)
                        result['server_log'] = (folder/'server.log').read_text()[-65536:]
                        print(json.dumps(dict(mode=mode,warmup=record['warmup'],passed=record['passed'],cost=record.get('cost'))),flush=True)
                        if not record['passed']:
                            raise RuntimeError(record['log'][-12000:])
        summarize(result)
        print(json.dumps(result['paired_percent']),flush=True)
    finally:
        if server and server.poll() is None:
            os.killpg(server.pid,signal.SIGTERM)
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(server.pid,signal.SIGKILL)
                server.wait(timeout=3)
        result['source_unchanged'] = sources() == result['source_sha256']
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_text(json.dumps(result,indent=2)+'\n')
    if not result['source_unchanged']:
        raise RuntimeError('source changed during cost evaluation')


if __name__ == '__main__':
    main()
