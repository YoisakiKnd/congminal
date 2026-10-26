#!/usr/bin/env python3
"""相同加密密钥/父端/SSH/SFTP，五组交替测固定假答案与有界 IPC 的增量成本。"""
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
import tempfile
import threading
import time

from check_auth_helper import environment
from check_sftp_adapter import identity, phase_metrics
from check_ssh_transport import sampler

TEST = 'sftp::openssh::authenticated::benchmark::identical_encrypted_authentication_and_data_workload_measures_incremental_ipc_cost'

def trial(binary, helper, root, folder, ssh, mode, wait_ms, sample):
    env = environment()
    env.update(CONGMINAL_BENCH_AUTH_ROOT=str(folder), CONGMINAL_BENCH_AUTH_HELPER=str(helper), CONGMINAL_BENCH_AUTH_MODE=mode, CONGMINAL_BENCH_AUTH_WAIT_MS=str(wait_ms), CONGMINAL_BENCH_SSH=ssh)
    started = time.monotonic_ns()
    process = subprocess.Popen([binary, TEST, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
    state, lines, events = ['startup'], [], []
    lock = threading.Lock()
    def output():
        length = 0
        for line in process.stdout:
            with lock:
                if length < 65536:
                    lines.append(line)
                    length += len(line)
                found = re.search(r'AUTH_COST_PHASE phase=(\w+)', line)
                if found:
                    state[0] = found[1]
                    events.append({'ns': time.monotonic_ns() - started, 'phase': state[0]})
    reader = threading.Thread(target=output)
    reader.start()
    samples = []
    try:
        while process.poll() is None:
            elapsed = time.monotonic_ns() - started
            if elapsed > 25_000_000_000: raise TimeoutError('authentication cost trial exceeded deadline')
            with lock: phase = state[0]
            samples.append({'ns': elapsed, 'phase': phase, 'tree': sample(process.pid)})
            time.sleep(0.005)
        reader.join(timeout=2)
        log = ''.join(lines)
        found = re.search(r'AUTH_COST_RESULT mode=(\w+) wait_ms=(\d+) bytes=(\d+) ready_us=(\d+) listing_us=(\d+) upload_us=(\d+) download_us=(\d+) cleanup_us=(\d+) auth_peak=(\d+) peak=(\d+) final=0', log)
        record = {'mode': mode, 'wait_ms': wait_ms, 'exit': process.returncode, 'log': log, 'events': events, 'samples': samples}
        record['passed'] = process.returncode == 0 and found is not None and '1 passed' in log
        if found:
            record['passed'] &= found[1] == mode and int(found[2]) == wait_ms
            record.update(dict(zip(['bytes', 'ready_us', 'listing_us', 'upload_us', 'download_us', 'cleanup_us', 'auth_peak', 'managed_peak'], map(int, found.groups()[2:]))))
            for direction in ['upload', 'download']:
                record[direction + '_mib_s'] = record['bytes'] / 1048576 / (record[direction + '_us'] / 1e6)
        record['phases'] = {phase: phase_metrics(samples, phase, process.pid) for phase in ['idle_before', 'listing', 'upload', 'download', 'idle_after']}
        # 相同三进程拓扑中间的稳定区间，去掉 helper 启动/退出各 100ms。
        overlapping = [s for s in samples if s['phase'] == 'connecting' and len(s['tree']) >= 3]
        if wait_ms and overlapping:
            low, high = overlapping[0]['ns'] + 100_000_000, overlapping[-1]['ns'] - 100_000_000
            waiting = [dict(s, phase='credential_wait') for s in overlapping if low <= s['ns'] <= high]
            record['waiting'] = phase_metrics(waiting, 'credential_wait', process.pid)
        record['peak_tree_rss'] = max((sum(p['rss'] for p in s['tree'].values()) for s in samples), default=0)
        record['peak_processes'] = max((len(s['tree']) for s in samples), default=0)
        record['peak_tree_threads'] = max((sum(p['threads'] for p in s['tree'].values()) for s in samples), default=0)
        seen = {pid for s in samples for pid in s['tree']} - {process.pid}
        time.sleep(0.05)
        record['observed_children_still_live'] = [pid for pid in seen if sample(pid)]
        record['passed'] &= not record['observed_children_still_live']
        if wait_ms: record['passed'] &= bool(record.get('waiting') and record['waiting']['sample_wall_ns'] >= 500_000_000)
        return record
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=3)
        reader.join(timeout=2)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rounds', type=int, default=5)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    ssh, cc = shutil.which('ssh'), shutil.which('cc')
    if not 5 <= args.rounds <= 20: parser.error('rounds must be 5..20')
    if os.name != 'posix' or not ssh or not cc or not Path('/usr/sbin/sshd').is_file(): parser.error('requires POSIX OpenSSH and a C fixture compiler')
    root = Path(__file__).resolve().parent.parent
    fixture = root / 'tools/fixtures/fixed_askpass.c'
    result = {'scope': 'Incremental bounded IPC/helper cost against a fixed PUBLIC fake-answer C fixture, not a usable product or secure UI baseline. Both use the same real Rust parent state/private endpoint/serviced stream, test frontend polling, generated encrypted key, OpenSSH/config/server, SFTP codec, 501 entries and 32MiB bidirectional verification, compression/control reuse disabled. Both launchers exec SSH. Two warmups per wait mode then at least five alternating pairs for immediate and 1s answers. Stable wait samples trim 100ms from each end of the observed 3-process overlap. Includes test harness and simulated frontend; excludes GUI/IME/Windows/weak networks/long idle/full-heap proof. Short-lived process peaks may be missed. No user SSH files, agent or credentials.', 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'tool_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'fixture': {'sha256': hashlib.sha256(fixture.read_bytes()).hexdigest(), 'source': fixture.read_text()}, 'platform': platform.platform(), 'machine': platform.machine(), 'logical_cpus': os.cpu_count(), 'versions': [identity(['rustc', '-Vv']), identity([ssh, '-V']), identity([cc, '--version'])], 'warmups': [], 'runs': []}
    server = None
    try:
        build = subprocess.run(['cargo', 'build', '--offline', '--release', '--features', 'ssh-auth-helper', '--bin', 'congminal-ssh-helper'], cwd=root, capture_output=True, text=True, timeout=180)
        result['helper_build'] = {'exit': build.returncode, 'stdout': build.stdout, 'stderr': build.stderr}
        if build.returncode: raise RuntimeError(build.stderr[-12000:])
        build = subprocess.run(['cargo', 'test', '--offline', '--release', '--no-run', '--message-format=json'], cwd=root, capture_output=True, text=True, timeout=180)
        result['parent_build'] = {'exit': build.returncode, 'stderr': build.stderr}
        if build.returncode: raise RuntimeError(build.stderr[-12000:])
        artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
        binary = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target', {}).get('name') == 'congminal' and a.get('executable'))
        helper = root / 'target/release/congminal-ssh-helper'
        sample, result['sampler'] = sampler()
        with tempfile.TemporaryDirectory(prefix='cgc-', dir='/tmp') as temporary:
            folder = Path(temporary).resolve()
            os.chmod(folder, 0o700)
            fixed = folder / 'fixed-askpass'
            command = [cc, '-O2', '-std=c11', '-Wall', '-Wextra', '-Werror', str(fixture), '-o', str(fixed)]
            built = subprocess.run(command, capture_output=True, text=True, timeout=30)
            result['fixture_build'] = {'command': command, 'exit': built.returncode, 'stdout': built.stdout, 'stderr': built.stderr}
            if built.returncode: raise RuntimeError(built.stderr)
            result['executables'] = {name: {'bytes': p.stat().st_size, 'sha256': hashlib.sha256(p.read_bytes()).hexdigest()} for name, p in [('fixed_fixture', fixed), ('helper', helper), ('test_parent', Path(binary))]}
            (folder / 'list').mkdir()
            for index in range(500): (folder / 'list' / f'file-{index:04}').touch()
            (folder / 'list/folder').mkdir()
            for name, phrase in [('host', ''), ('client', 'fixture only')]: subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', phrase, '-f', str(folder / name)], check=True)
            (folder / 'authorized_keys').write_bytes((folder / 'client.pub').read_bytes())
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0)); port = probe.getsockname()[1]
            public = (folder / 'host.pub').read_text().split()
            (folder / 'known').write_text(f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
            (folder / 'client.config').write_text(f'Host fixture\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile "{folder}/client"\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile "{folder}/known"\n GlobalKnownHostsFile none\n StrictHostKeyChecking yes\n NumberOfPasswordPrompts 1\n Compression no\n ControlMaster no\n ControlPath none\n LogLevel ERROR\n')
            (folder / 'sshd.config').write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {folder}/host\nAuthorizedKeysFile {folder}/authorized_keys\nPidFile {folder}/pid\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY no\nSubsystem sftp internal-sftp\nLogLevel ERROR\n')
            with (folder / 'server.log').open('w') as log:
                server = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(folder / 'sshd.config')], stdout=log, stderr=log, start_new_session=True)
                for _ in range(100):
                    if server.poll() is not None: raise RuntimeError('isolated sshd exited')
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=0.05): break
                    except OSError: time.sleep(0.01)
                else: raise RuntimeError('isolated sshd not ready')
                for wait_ms in [0, 1000]:
                    for index in range(-1, args.rounds):
                        order = ['fixed', 'bounded'] if index % 2 == 0 else ['bounded', 'fixed']
                        for mode in order:
                            record = trial(binary, fixed if mode == 'fixed' else helper, root, folder, ssh, mode, wait_ms, sample)
                            record['round'] = index
                            result['warmups' if index == -1 else 'runs'].append(record)
                            print(json.dumps({k: record.get(k) for k in ['mode', 'wait_ms', 'round', 'passed', 'ready_us']}), flush=True)
                            if not record['passed']: raise RuntimeError(record['log'][-12000:])
            result['summary'] = {}
            for wait_ms in [0, 1000]:
                for mode in ['fixed', 'bounded']:
                    records = [r for r in result['runs'] if r['mode'] == mode and r['wait_ms'] == wait_ms]
                    result['summary'][f'{mode}_{wait_ms}'] = {name: {'median': statistics.median(r[name] for r in records), 'min': min(r[name] for r in records), 'max': max(r[name] for r in records)} for name in ['ready_us', 'listing_us', 'upload_mib_s', 'download_mib_s', 'cleanup_us', 'auth_peak', 'managed_peak', 'peak_tree_rss', 'peak_processes', 'peak_tree_threads']}
            result['server_log'] = (folder / 'server.log').read_text()[-65536:]
    finally:
        if server and server.poll() is None:
            os.killpg(server.pid, signal.SIGTERM)
            try: server.wait(timeout=3)
            except subprocess.TimeoutExpired: os.killpg(server.pid, signal.SIGKILL); server.wait(timeout=3)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')

if __name__ == '__main__': main()
