#!/usr/bin/env python3
"""隔离回环 SSH/SFTP：真实 Rust 原生会话、主机验证、流式操作与故障清理。"""
import argparse
import getpass
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

from check_ssh_transport import sampler

SCENARIOS = ['positive', 'resolved', 'paths', 'host-alias', 'hashed', 'encrypted', 'bad-passphrase', 'unknown', 'changed', 'revoked', 'auth', 'huge', 'name', 'stall', 'close', 'timeout']
PEER = r'''
import struct,sys
r=sys.stdin.buffer;w=sys.stdout.buffer;mode=sys.argv[1]
def exact(n):
 b=b''
 while len(b)<n:
  part=r.read(n-len(b))
  if not part:raise EOFError()
  b+=part
 return b
def packet():
 n=struct.unpack('>I',exact(4))[0]
 if n>65536:raise ValueError(n)
 return exact(n)
def send(body):w.write(struct.pack('>I',len(body))+body);w.flush()
try:
 assert packet()==b'\1\0\0\0\3'
 send(b'\2\0\0\0\3')
 p=packet();assert p[0]==11
 send(b'\x66'+p[1:5]+b'\0\0\0\1H')
 p=packet();assert p[0]==12
 if mode=='huge':w.write(struct.pack('>I',1<<30)+b'\x68'+p[1:5]);w.flush()
 elif mode=='name':send(b'\x68'+p[1:5]+struct.pack('>IIII',1,1<<30,0,0))
 elif mode=='close':sys.exit(0)
 r.read()
except EOFError:pass
'''


def build(root, test=True):
    command = (['cargo', 'test', '--offline', '--release', '--features', 'sftp-libssh2', '--no-run', '--message-format=json'] if test else ['cargo', 'build', '--offline', '--release', '--message-format=json'])
    result = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=180)
    if result.returncode:
        raise RuntimeError('SFTP test build failed: ' + result.stderr[-8000:])
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get('reason') == 'compiler-artifact' and bool(message.get('profile', {}).get('test')) == test and message.get('target', {}).get('name') == 'congminal' and message.get('executable'):
            return message['executable']
    raise RuntimeError('No native SFTP test executable')

def check_path_oracle(command, root):
    process = subprocess.Popen(command, cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        process.stdin.write(b'\0\0\0\5\1\0\0\0\3')
        process.stdin.flush()
        prefix = b''
        deadline = time.monotonic() + 5
        # VERSION 响应到达前保留输入端；提前 EOF 会让服务端丢弃尚未刷出的回复。
        while len(prefix) < 9:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                break
            data = os.read(process.stdout.fileno(), 9 - len(prefix))
            if not data:
                break
            prefix += data
        process.stdin.close()
        process.stdin = None
        rest, error = process.communicate(timeout=5)
        return {'exit': process.returncode, 'sftp_v3': len(prefix) == 9 and prefix[4:9] == b'\2\0\0\0\3', 'response_bytes': len(prefix) + len(rest), 'prefix_hex': prefix.hex(), 'stderr': error.decode('utf-8', errors='replace')[:8192]}
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=3)


def run_case(executable, application, root, folder, scenario, sample, backend='native'):
    probe = socket.socket()
    probe.bind(('127.0.0.1', 0))
    port = probe.getsockname()[1]
    probe.close()
    target_name = 'localhost' if scenario in ['resolved', 'paths', 'host-alias'] else '127.0.0.1'
    for filename, keyname in [('known', 'host.pub'), ('changed', 'changed-host.pub')]:
        key = (folder / keyname).read_text().split()
        lookup = 'none' if scenario == 'host-alias' else f'[{target_name}]:{port}'
        (folder / filename).write_text(f'{lookup} {key[0]} {key[1]}\n')
    if backend == 'openssh' and scenario == 'changed':
        shutil.copyfile(folder / 'changed', folder / 'known')
    if backend == 'openssh' and scenario == 'unknown':
        (folder / 'known').write_bytes(b'')
    if backend == 'openssh' and scenario == 'revoked':
        (folder / 'known').write_text('@revoked ' + (folder / 'known').read_text())
    if scenario == 'hashed':
        subprocess.run(['ssh-keygen', '-q', '-H', '-f', str(folder / 'known')], capture_output=True, check=True)
    subsystem = 'internal-sftp' if scenario in ['positive', 'resolved', 'paths', 'host-alias', 'hashed', 'agent', 'encrypted', 'bad-passphrase', 'unknown', 'changed', 'revoked', 'auth'] else f'{shlex.quote(sys.executable)} {shlex.quote(str(folder / "peer.py"))} {scenario}'
    config = f'''Port {port}
ListenAddress 127.0.0.1
HostKey {folder}/host
PidFile {folder}/pid
AuthorizedKeysFile {folder}/authorized_keys
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowUsers {getpass.getuser()}
AllowTcpForwarding no
X11Forwarding no
PermitTTY no
Subsystem sftp {subsystem}
LogLevel DEBUG3
'''
    (folder / 'config').write_text(config)
    subprocess.run(['/usr/sbin/sshd', '-t', '-f', str(folder / 'config')], check=True)
    log_path = folder / f'{scenario}.server.log'
    process = None
    agent = None
    with log_path.open('w') as log:
        server = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(folder / 'config')], stdout=log, stderr=log, start_new_session=True)
        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise RuntimeError(log_path.read_text()[-4000:])
                try:
                    with socket.create_connection(('127.0.0.1', port), timeout=0.05):
                        break
                except OSError:
                    time.sleep(0.01)
            else:
                raise RuntimeError('isolated sshd not ready')
            environment = os.environ.copy()
            environment.update(CONGMINAL_TEST_APP=application, CONGMINAL_TEST_SFTP_NAME=target_name, CONGMINAL_TEST_SFTP_ROOT=str(folder), CONGMINAL_TEST_SFTP_USER=getpass.getuser(), CONGMINAL_TEST_SFTP_PORT=str(port), CONGMINAL_TEST_SFTP_SCENARIO=scenario)
            if backend == 'openssh':
                # 第二个带空格的 known_hosts 路径实际持有记录；-G 的扁平输出无法恢复这种边界。
                host_records = folder / 'known records with spaces'
                shutil.copyfile(folder / 'known', host_records)
                include = folder / 'stream include'
                identity = 'wrong' if scenario == 'auth' else 'client'
                if scenario in ['paths', 'host-alias']:
                    identity = 'tokens-%h_%n_%r_%p_%k_%%'
                include.write_text(f'Host bounded-path-alias\n  HostName {target_name}\n  Port {port}\n  User {getpass.getuser()}\n  IdentityFile "{folder / identity}"\n  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile "{folder / "missing records"}" "{host_records}"\n  GlobalKnownHostsFile none\n  StrictHostKeyChecking yes\n  LogLevel ERROR\nMatch originalhost bounded-path-alias\n  Compression yes\n')
                if scenario == 'agent':
                    socket_path = folder / 'agent.sock'
                    agent = subprocess.Popen(['ssh-agent', '-D', '-a', str(socket_path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
                    for _ in range(100):
                        if socket_path.exists():
                            break
                        if agent.poll() is not None:
                            raise RuntimeError('isolated SSH agent exited before ready')
                        time.sleep(0.01)
                    else:
                        raise RuntimeError('isolated SSH agent not ready')
                    environment['SSH_AUTH_SOCK'] = str(socket_path)
                    subprocess.run(['ssh-add', str(folder / 'client')], env=environment, capture_output=True, check=True, timeout=5)
                    include.write_text(include.read_text().replace(f'IdentityFile "{folder / identity}"', 'IdentityFile none').replace('IdentitiesOnly yes', 'IdentitiesOnly no').replace('IdentityAgent none', f'IdentityAgent "{socket_path}"'))
                if scenario == 'host-alias':
                    with include.open('a') as stream:
                        stream.write('  HostKeyAlias none\n')
                (folder / 'stream config').write_text(f'Include "{include}"\n')
            path_oracle = None
            if scenario in ['paths', 'host-alias']:
                # 由 OpenSSH 实际展开，与应用路径展开使用同一生成密钥；不执行远端 shell。
                key_alias = 'none' if scenario == 'host-alias' else 'bounded-path-alias'
                expected = folder / f'tokens-{target_name}_bounded-path-alias_{getpass.getuser()}_{port}_{key_alias}_%'
                shutil.copyfile(folder / 'client', expected)
                os.chmod(expected, 0o600)
                oracle_config = folder / 'path oracle'
                oracle_config.write_text(f'Host bounded-path-alias\n  HostName {target_name}\n  Port {port}\n  User {getpass.getuser()}\n  IdentityFile "{folder}/tokens-%h_%n_%r_%p_%k_%%"\n  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile "{folder / "known"}"\n  GlobalKnownHostsFile none\n  StrictHostKeyChecking yes\n  BatchMode yes\n  LogLevel ERROR\n')
                if scenario == 'host-alias':
                    with oracle_config.open('a') as stream:
                        stream.write('  HostKeyAlias none\n')
                oracle_result = check_path_oracle(['/usr/bin/ssh', '-T', '-F', str(oracle_config), '-s', '--', 'bounded-path-alias', 'sftp'], root)
                if oracle_result['exit'] or not oracle_result['sftp_v3']:
                    return {'scenario': scenario, 'exit': oracle_result['exit'] or 1, 'passed': False, 'log': 'OpenSSH path-token oracle failed: ' + json.dumps(oracle_result), 'path_oracle': oracle_result}
                path_oracle = oracle_result
            started = time.monotonic_ns()
            test = 'sftp::openssh::tests::isolated_subsystem_streams_lists_and_releases_after_faults' if backend == 'openssh' else 'sftp::native::tests::native_isolated_session_validates_hosts_handles_streams_and_cleanup'
            process = subprocess.Popen([executable, test, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            lines = []
            def read_output():
                length = 0
                for line in process.stdout:
                    if length < 65536:
                        lines.append(line)
                        length += len(line)
            reader = threading.Thread(target=read_output)
            reader.start()
            samples = []
            while process.poll() is None:
                elapsed = time.monotonic_ns() - started
                if elapsed > 45_000_000_000:
                    raise TimeoutError('native SFTP test exceeded 45 seconds')
                if sample:
                    samples.append({'elapsed_ns': elapsed, 'client': sample(process.pid), 'server': sample(server.pid)})
                time.sleep(0.005)
            reader.join(timeout=2)
            time.sleep(0.05)
            server_log = log_path.read_text()
            record = {'scenario': scenario, 'client_pid': process.pid, 'exit': process.returncode, 'log': ''.join(lines), 'samples': samples, 'server_log': server_log[:262144]}
            if path_oracle is not None:
                record['path_oracle'] = path_oracle
            if sample:
                seen = {int(pid) for s in samples for pid in s['client']} - {process.pid}
                record['observed_client_children_still_live'] = [pid for pid in seen if sample(pid)]
                record['peak_client_tree_rss'] = max((sum(p['rss'] for p in s['client'].values()) for s in samples), default=0)
            if scenario in ['unknown', 'changed', 'revoked']:
                record['auth_request_seen'] = 'userauth-request for user' in server_log
            marker = 'final=0' if backend == 'openssh' or scenario in ['positive', 'resolved', 'paths', 'host-alias', 'hashed', 'encrypted'] else 'used=0'
            record['passed'] = process.returncode == 0 and marker in record['log'] and not record.get('observed_client_children_still_live') and not record.get('auth_request_seen')
            return record
        finally:
            if process and process.poll() is None:
                process.kill()
                process.wait(timeout=3)
            if agent and agent.poll() is None:
                os.killpg(agent.pid, signal.SIGTERM)
                try:
                    agent.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(agent.pid, signal.SIGKILL)
                    agent.wait(timeout=3)
            if server.poll() is None:
                os.killpg(server.pid, signal.SIGTERM)
                try:
                    server.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(server.pid, signal.SIGKILL)
                    server.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--scenario', choices=['all'] + SCENARIOS, default='all')
    parser.add_argument('--rounds', type=int, default=1)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.rounds <= 100:
        parser.error('--rounds must be 1..100')
    if os.name != 'posix' or not Path('/usr/sbin/sshd').is_file():
        parser.error('isolated fixture requires POSIX OpenSSH sshd; this does not claim Windows runtime validation')
    root = Path(__file__).resolve().parent.parent
    sample, note = sampler()
    result = {'scope': 'Real feature-gated Rust Session/NativePool over isolated SSH, generated keys, strict host records, 32MiB streaming / 8MiB managed test budget; bounded same-program DNS/cache, numeric dual-stack connection, and Unix supervised OpenSSH -G queries with controlled Include/Match/ProxyJump and Match exec cancellation. Test fixture input reservation is included in shared_peak. This POSIX run does not verify Windows capture or native C dependencies/runtime, production user-config integration, GUI, or full heap/RSS.', 'sampler': note, 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'runs': []}
    executable = build(root)
    application = build(root, test=False)
    checked = subprocess.run([executable, 'sftp::connect::tests::numeric_connect_handles_dual_stack_refusal_fallback_and_descriptor_flags', '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, capture_output=True, text=True, timeout=10)
    result['numeric_connect'] = {'exit': checked.returncode, 'log': checked.stdout + checked.stderr}
    if checked.returncode:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')
        raise RuntimeError(result['numeric_connect']['log'])
    with tempfile.TemporaryDirectory(prefix='congminal-sftp-native-') as temporary:
        folder = Path(temporary)
        os.chmod(folder, 0o700)
        for key in ['host', 'changed-host', 'client', 'wrong']:
            subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(folder / key)], check=True)
        subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', 'fixture only', '-f', str(folder / 'encrypted')], check=True)
        (folder / 'authorized_keys').write_bytes((folder / 'client.pub').read_bytes() + (folder / 'encrypted.pub').read_bytes())
        remote = folder / 'remote'
        listing = remote / 'list'
        listing.mkdir(parents=True)
        (listing / 'directory').mkdir()
        (listing / '- space 中文\n.bin').write_bytes(b'binary\0\xff')
        (listing / '.hidden').touch()
        (folder / 'peer.py').write_text(PEER)
        environment = os.environ.copy()
        environment['CONGMINAL_TEST_APP'] = application
        # 受控、无网络配置；包含自建 Match exec 等待/取消用例，不读取用户配置。
        include = folder / 'client include'
        include.write_text(f'Host bounded-alias\n  HostName localhost\n  HostKeyAlias bounded-host\n  IdentityFile "{folder / "first key"}"\n  IdentityFile "{folder / "second key"}"\n  ProxyJump bounded-jump\nMatch originalhost bounded-alias\n  Compression yes\n')
        client_config = folder / 'client config'
        client_config.write_text(f'Include "{include}"\n')
        environment['CONGMINAL_TEST_SSH_CONFIG'] = str(client_config)
        stalled = folder / 'client stalled'
        stalled.write_text('Match exec "sleep 30"\n  Compression yes\n')
        environment['CONGMINAL_TEST_SSH_STALLED_CONFIG'] = str(stalled)
        for label, path in [('BASELINE', 'none'), ('SETTINGS', str(client_config))]:
            expanded = subprocess.run(['/usr/bin/ssh', '-G', '-T', '-F', path, '-p', '2222', '--', 'bounded-alias'], capture_output=True, timeout=5, check=True)
            if len(expanded.stdout) > 128 * 1024:
                raise RuntimeError('controlled SSH fixture exceeded output limit')
            output = folder / label
            output.write_bytes(expanded.stdout)
            environment['CONGMINAL_TEST_SSH_' + label] = str(output)
        ambiguous_config = folder / 'client ambiguous'
        ambiguous_config.write_text(f'Host bounded-alias\n  UserKnownHostsFile "{folder / "one key"}" "{folder / "two key"}"\n  GlobalKnownHostsFile none\n')
        ambiguous = subprocess.run(['/usr/bin/ssh', '-G', '-T', '-F', str(ambiguous_config), '--', 'bounded-alias'], capture_output=True, timeout=5, check=True)
        if len(ambiguous.stdout) > 128 * 1024:
            raise RuntimeError('controlled ambiguous-path fixture exceeded output limit')
        ambiguous_output = folder / 'AMBIGUOUS'
        ambiguous_output.write_bytes(ambiguous.stdout)
        environment['CONGMINAL_TEST_SSH_AMBIGUOUS'] = str(ambiguous_output)
        settings = subprocess.Popen([executable, 'ssh::settings::tests::real_openssh_expansion_preserves_paths_and_flags_unhandled_changes', '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        samples = []
        started = time.monotonic_ns()
        try:
            while settings.poll() is None:
                elapsed = time.monotonic_ns() - started
                if elapsed > 15_000_000_000:
                    raise TimeoutError('controlled settings test exceeded 15 seconds')
                if sample:
                    samples.append({'elapsed_ns': elapsed, 'client': sample(settings.pid)})
                time.sleep(0.005)
            log = settings.communicate(timeout=2)[0]
        finally:
            if settings.poll() is None:
                settings.kill()
                settings.wait(timeout=3)
        record = {'exit': settings.returncode, 'log': log[:65536], 'samples': samples}
        if sample:
            seen = {int(pid) for s in samples for pid in s['client']} - {settings.pid}
            time.sleep(0.05)
            record['observed_children_still_live'] = [pid for pid in seen if sample(pid)]
            record['peak_client_tree_rss'] = max((sum(p['rss'] for p in s['client'].values()) for s in samples), default=0)
        result['ssh_settings'] = record
        if settings.returncode or record.get('observed_children_still_live'):
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2) + '\n')
            raise RuntimeError(result['ssh_settings']['log'])
        resolver = subprocess.run([executable, 'resolve::parent::tests::real_helper_resolution_and_cost_are_bounded_and_release_credit', '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=environment, capture_output=True, text=True, timeout=15)
        result['resolver_cost'] = {'exit': resolver.returncode, 'log': resolver.stdout + resolver.stderr}
        if resolver.returncode:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2) + '\n')
            raise RuntimeError(result['resolver_cost']['log'])

        try:
            for round_number in range(args.rounds):
                for scenario in SCENARIOS if args.scenario == 'all' else [args.scenario]:
                    record = run_case(executable, application, root, folder, scenario, sample)
                    record['round'] = round_number
                    result['runs'].append(record)
                    print(json.dumps({k: record[k] for k in ['scenario', 'round', 'exit', 'passed']}), flush=True)
                    if not record['passed']:
                        raise RuntimeError(record['log'][-8000:])
        finally:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2) + '\n')
    print('SFTP native checks passed')


if __name__ == '__main__':
    main()
