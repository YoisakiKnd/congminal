#!/usr/bin/env python3
"""同服务器/协议/数据，交替测量阻塞基线与有界 OpenSSH 管道；不访问用户 SSH 文件。"""
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

from check_ssh_transport import sampler

TEST = 'sftp::openssh::benchmark::identical_protocol_workload_compares_blocking_and_bounded_pipes'

def identity(command):
    done = subprocess.run(command, capture_output=True, text=True, timeout=10)
    return {'command': command, 'exit': done.returncode, 'stdout': done.stdout, 'stderr': done.stderr}

def phase_metrics(samples, phase, root_pid):
    selected = [s for s in samples if s['phase'] == phase]
    if not selected:
        return None
    first, last = selected[0], selected[-1]
    wall = last['ns'] - first['ns']
    common = first['tree'].keys() & last['tree'].keys()
    cpu = sum(max(0, last['tree'][p]['cpu_ns'] - first['tree'][p]['cpu_ns']) for p in common)
    return {'samples': len(selected), 'sample_wall_ns': wall,
            'common_process_cpu_ns': cpu,
            'single_core_cpu_percent': cpu / wall * 100 if wall else None,
            'peak_root_rss': max(s['tree'].get(root_pid, {}).get('rss', 0) for s in selected),
            'peak_children_rss': max(sum(v['rss'] for p, v in s['tree'].items() if p != root_pid) for s in selected),
            'peak_tree_rss': max(sum(v['rss'] for v in s['tree'].values()) for s in selected),
            'peak_root_threads': max(s['tree'].get(root_pid, {}).get('threads', 0) for s in selected),
            'peak_tree_threads': max(sum(v['threads'] for v in s['tree'].values()) for s in selected),
            'peak_tree_processes': max(len(s['tree']) for s in selected)}

def trial(executable, root, folder, ssh, mode, sample):
    environment = os.environ.copy()
    environment.update(CONGMINAL_BENCH_SFTP_ROOT=str(folder), CONGMINAL_BENCH_SFTP_MODE=mode, CONGMINAL_BENCH_SSH=ssh)
    started = time.monotonic_ns()
    process = subprocess.Popen([executable, TEST, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
    current_phase = ['startup']
    lines, events = [], []
    lock = threading.Lock()
    def output():
        length = 0
        for line in process.stdout:
            with lock:
                if length < 65536:
                    lines.append(line)
                    length += len(line)
                found = re.search(r'SFTP_ADAPTER_PHASE mode=\w+ phase=(\w+)', line)
                if found:
                    current_phase[0] = found.group(1)
                    events.append({'ns': time.monotonic_ns() - started, 'phase': current_phase[0]})
    reader = threading.Thread(target=output)
    reader.start()
    samples = []
    try:
        while process.poll() is None:
            elapsed = time.monotonic_ns() - started
            if elapsed > 20_000_000_000:
                raise TimeoutError('normal-network adapter benchmark exceeded 20 seconds')
            if sample:
                with lock:
                    phase = current_phase[0]
                samples.append({'ns': elapsed, 'phase': phase, 'tree': sample(process.pid)})
            time.sleep(0.005)
        reader.join(timeout=2)
        log = ''.join(lines)
        match = re.search(r'SFTP_ADAPTER_RESULT mode=\w+ bytes=(\d+) ready_us=(\d+) listing_us=(\d+) upload_us=(\d+) download_us=(\d+)', log)
        closed = re.search(r'SFTP_ADAPTER_CLOSED mode=\w+ cleanup_us=(\d+) peak=(\d+) final=0', log)
        result = {'mode': mode, 'exit': process.returncode, 'log': log, 'events': events, 'samples': samples}
        result['passed'] = process.returncode == 0 and match is not None and closed is not None
        if match and closed:
            result.update(dict(zip(['bytes', 'ready_us', 'listing_us', 'upload_us', 'download_us'], map(int, match.groups()))))
            result.update(cleanup_us=int(closed.group(1)), managed_peak=int(closed.group(2)))
            result['upload_mib_s'] = result['bytes'] / (1024 * 1024) / (result['upload_us'] / 1e6)
            result['download_mib_s'] = result['bytes'] / (1024 * 1024) / (result['download_us'] / 1e6)
        if sample:
            time.sleep(0.05)
            seen = {p for s in samples for p in s['tree']} - {process.pid}
            result['observed_children_still_live'] = [p for p in seen if sample(p)]
            result['passed'] &= not result['observed_children_still_live']
            result['phases'] = {phase: phase_metrics(samples, phase, process.pid) for phase in ['idle_before', 'listing', 'upload', 'download', 'idle_after']}
            result['peak_tree_rss'] = max((sum(v['rss'] for v in s['tree'].values()) for s in samples), default=0)
        return result
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
    if not 5 <= args.rounds <= 20:
        parser.error('rounds must be 5..20')
    ssh = shutil.which('ssh')
    if os.name != 'posix' or not ssh or not Path('/usr/sbin/sshd').is_file():
        parser.error('requires POSIX OpenSSH; this benchmark does not prove Windows runtime behavior')
    root = Path(__file__).resolve().parent.parent
    sample, note = sampler()
    result = {'scope': 'Release default-dependency test process; same OpenSSH executable/config, generated keys and sshd, SFTP v3 codec, 501 visible directory entries, 32MiB pattern upload/download verification, 64KiB transfer chunks and sequential ACK. Two warmups then alternating order in five or more pairs, 600ms idle windows. Blocking std pipes are only a healthy-network control and lack product cancellation; Python owns the overall trial timeout. Excludes GUI latency, machine cold-cache guarantees, full-heap admission and Windows runtime. Process samples can miss short-lived peaks.', 'platform': platform.platform(), 'machine': platform.machine(), 'logical_cpus': os.cpu_count(), 'sampler': note, 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'tool_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'versions': [identity(['rustc', '-Vv']), identity([ssh, '-V']), identity(['/usr/sbin/sshd', '-V'])], 'warmups': [], 'runs': []}
    if platform.system() == 'Darwin':
        result['hardware'] = identity(['sysctl', '-n', 'machdep.cpu.brand_string', 'hw.memsize'])
    build = subprocess.run(['cargo', 'test', '--offline', '--release', '--no-run', '--message-format=json'], cwd=root, capture_output=True, text=True, timeout=180)
    executable = None
    diagnostics = []
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if item.get('reason') == 'compiler-message':
            diagnostics.append(item.get('message', {}).get('rendered', ''))
        if item.get('reason') == 'compiler-artifact' and item.get('profile', {}).get('test') and item.get('executable'):
            executable = item['executable']
    result['build'] = {'exit': build.returncode, 'stderr': build.stderr, 'diagnostics': diagnostics}
    server = None
    try:
        if build.returncode or not executable:
            raise RuntimeError(build.stderr + ''.join(diagnostics))
        with tempfile.TemporaryDirectory(prefix='congminal-sftp-adapter-') as temporary:
            folder = Path(temporary)
            os.chmod(folder, 0o700)
            for name in ['host', 'client']:
                subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(folder / name)], check=True)
            (folder / 'authorized_keys').write_bytes((folder / 'client.pub').read_bytes())
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                port = probe.getsockname()[1]
            public = (folder / 'host.pub').read_text().split()
            (folder / 'known records').write_text(f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
            (folder / 'server config').write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {folder}/host\nAuthorizedKeysFile {folder}/authorized_keys\nPidFile {folder}/pid\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY no\nSubsystem sftp internal-sftp\nLogLevel ERROR\n')
            (folder / 'client config').write_text(f'Host bounded-benchmark\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile "{folder / "client"}"\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile "{folder / "known records"}"\n GlobalKnownHostsFile none\n StrictHostKeyChecking yes\n Compression no\n ControlMaster no\n ControlPath none\n LogLevel ERROR\n')
            listing = folder / 'list'
            listing.mkdir()
            (listing / 'directory').mkdir()
            (listing / '.hidden').touch()
            for i in range(500):
                (listing / f'{i:04d} space 中文.txt').touch()
            subprocess.run(['/usr/sbin/sshd', '-t', '-f', str(folder / 'server config')], check=True)
            with (folder / 'server.log').open('w') as log:
                server = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(folder / 'server config')], stdout=log, stderr=log, start_new_session=True)
                for _ in range(100):
                    if server.poll() is not None:
                        raise RuntimeError('isolated sshd exited')
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=0.05):
                            break
                    except OSError:
                        time.sleep(0.01)
                else:
                    raise RuntimeError('isolated sshd not ready')
                for round_number in range(-1, args.rounds):
                    order = ['blocking', 'bounded'] if round_number % 2 == 0 else ['bounded', 'blocking']
                    for mode in order:
                        record = trial(executable, root, folder, ssh, mode, sample)
                        record['round'] = round_number
                        result['warmups' if round_number == -1 else 'runs'].append(record)
                        print(json.dumps({k: record[k] for k in ['mode', 'round', 'exit', 'passed']}, ensure_ascii=False), flush=True)
                        if not record['passed']:
                            raise RuntimeError(record['log'][-8000:])
                # 独立运行，避免额外的同步观察线程影响上面的吞吐/RSS 对照。
                environment = os.environ.copy()
                environment['CONGMINAL_BENCH_SSH'] = ssh
                launch = subprocess.run([executable, 'sftp::openssh::benchmark::synchronized_launch_guard_wait_is_measured_without_network', '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=environment, capture_output=True, text=True, timeout=30)
                log = launch.stdout + launch.stderr
                waits = [{'index': int(i), 'wait_us': int(w), 'hold_upper_us': int(h)} for i, w, h in re.findall(r'SFTP_LAUNCH_SAMPLE index=(\d+) wait_us=(\d+) hold_upper_us=(\d+)', log)]
                spawns = {int(i): int(t) for i, t in re.findall(r'SFTP_SPAWN_SAMPLE index=(\d+) spawn_us=(\d+)', log)}
                result['launch_lock'] = {'exit': launch.returncode, 'log': log, 'samples': waits, 'spawn_us': spawns, 'percentile_method': 'nearest rank on 100 synchronized headless lock samples; hold_upper_us includes observer scheduling and is an upper bound, not exact hold time; not GUI input latency'}
                if launch.returncode or len(waits) != 100 or len(spawns) != 100 or 'SFTP_LAUNCH_DONE samples=100 final=0' not in log:
                    raise RuntimeError(log[-8000:])
                for field in ['wait_us', 'hold_upper_us']:
                    values = sorted(r[field] for r in waits)
                    result['launch_lock'][field] = {'median': statistics.median(values), 'p95': values[94], 'p99': values[98], 'max': values[-1]}
                result['summary'] = {}
                for mode in ['blocking', 'bounded']:
                    records = [r for r in result['runs'] if r['mode'] == mode]
                    result['summary'][mode] = {name: {'median': statistics.median(values), 'min': min(values), 'max': max(values)} for name in ['ready_us', 'listing_us', 'upload_mib_s', 'download_mib_s', 'cleanup_us', 'managed_peak'] if (values := [r[name] for r in records])}
                result['server_log'] = (folder / 'server.log').read_text()[:65536]
    finally:
        if server and server.poll() is None:
            os.killpg(server.pid, signal.SIGTERM)
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(server.pid, signal.SIGKILL)
                server.wait(timeout=3)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')

if __name__ == '__main__':
    main()
