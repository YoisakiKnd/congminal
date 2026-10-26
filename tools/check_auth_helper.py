#!/usr/bin/env python3
"""私有 Unix IPC、独立 helper 与真实 OpenSSH；仅生成密钥和隔离回环 sshd。"""
import argparse
import getpass
import hashlib
import hmac
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time

from check_ssh_transport import sampler

def exact(stream, count):
    result = b''
    while len(result) < count:
        part = stream.recv(count - len(result))
        if not part:
            raise EOFError('incomplete private frame')
        result += part
    return result

class Broker:
    def __init__(self, folder, responses, split=False):
        self.folder = folder
        folder.mkdir(mode=0o700)
        self.nonce = os.urandom(32)
        self.ticket = folder / 'ticket'
        descriptor = os.open(self.ticket, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb') as file:
            file.write(b'CGAT\2\0\0\0' + self.nonce)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(folder / 's'))
        self.listener.listen(2)
        self.listener.settimeout(0.05)
        self.responses = responses
        self.split = split
        self.stop = threading.Event()
        self.records, self.errors = [], []
        self.thread = threading.Thread(target=self.run)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            try:
                stream, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            try:
                with stream:
                    stream.settimeout(5)
                    header = exact(stream, 44)
                    count = struct.unpack('>I', header[40:44])[0]
                    if header[:5] != b'CGAP\1' or header[5] > 3 or header[6:8] != b'\0\0' or not hmac.compare_digest(header[8:40], self.nonce) or not 0 < count <= 16384:
                        raise ValueError('private request rejected')
                    prompt = exact(stream, count)
                    if stream.recv(1) != b'':
                        raise ValueError('request did not close its write side')
                    index = len(self.records)
                    self.records.append({'kind': header[5], 'prompt_bytes': count, 'contains_fingerprint': b'SHA256:' in prompt})
                    if index >= 16:
                        raise ValueError('too many fixture interactions')
                    response = self.responses[min(index, len(self.responses) - 1)]
                    # 便于观察临时 helper 存活；不纳入认证响应性能结论。
                    time.sleep(0.03)
                    if response is False:
                        continue
                    if response is None:
                        wire = b'CGAR\1\0\0\0\0\0\0\0'
                    elif response == 'invalid':
                        wire = b'CGAR\2\1\0\0\0\0\0\0'
                    else:
                        wire = b'CGAR\1\1\0\0' + struct.pack('>H', len(response)) + b'\0\0' + response
                    if self.split:
                        for byte in wire:
                            stream.sendall(bytes([byte]))
                    else:
                        stream.sendall(wire)
            except (OSError, EOFError, ValueError) as error:
                self.errors.append(type(error).__name__)

    def close(self):
        self.stop.set()
        self.listener.close()
        self.thread.join(timeout=6)
        if self.thread.is_alive():
            raise RuntimeError('fixture IPC worker did not stop')

def environment():
    return {'PATH': os.defpath, 'LANG': 'C'}

def run_direct(helper, folder, name, response, expected_exit, expected_output, hint=None, split=True):
    broker = Broker(folder / name, [response], split=split)
    env = environment()
    env['CONGMINAL_ASKPASS_TICKET'] = str(broker.ticket)
    if hint is not None:
        env['SSH_ASKPASS_PROMPT'] = hint
    try:
        done = subprocess.run([helper, '--launch'], env=env, capture_output=True, timeout=8)
        record = {'name': name, 'exit': done.returncode, 'output_bytes': len(done.stdout), 'output_matches': done.stdout == expected_output, 'stderr_empty': not done.stderr, 'requests': broker.records, 'fixture_errors': broker.errors}
        record['passed'] = done.returncode == expected_exit and done.stdout == expected_output and not done.stderr and not broker.errors
        expected_kind = 1 if hint == 'confirm' else 2 if hint == 'none' else 3
        record['expected_kind'] = expected_kind
        record['passed'] &= bool(broker.records) and all(r['kind'] == expected_kind for r in broker.records)
        return record
    finally:
        broker.close()

def run_ssh(helper, ssh, root, folder, port, name, identity, responses, expect_success, unknown=False, sample=None):
    broker = Broker(folder / ('ipc-' + name), responses)
    known = folder / (name + '.known')
    public = (folder / 'host.pub').read_text().split()
    known.write_text('' if unknown else f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
    config = folder / (name + '.config')
    config.write_text(f'Host fixture\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile "{folder / identity}"\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile "{known}"\n GlobalKnownHostsFile none\n StrictHostKeyChecking {"ask" if unknown else "yes"}\n NumberOfPasswordPrompts 2\n LogLevel ERROR\n')
    command = [helper, '--launch', str(broker.ticket), ssh, '-T', '-s', '-oForkAfterAuthentication=no', '-oBatchMode=no', '-F', str(config)]
    command += ['--', 'fixture', 'sftp']
    process = subprocess.Popen(command, cwd=root, env=environment(), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    started = time.monotonic_ns()
    samples, prefix = [], b''
    try:
        process.stdin.write(b'\0\0\0\5\1\0\0\0\3')
        process.stdin.flush()
        while process.poll() is None:
            elapsed = time.monotonic_ns() - started
            if elapsed > 12_000_000_000:
                raise TimeoutError('isolated authentication exceeded deadline')
            if sample:
                samples.append({'ns': elapsed, 'tree': sample(process.pid)})
            if select.select([process.stdout], [], [], 0)[0]:
                part = os.read(process.stdout.fileno(), 9 - len(prefix))
                prefix += part
                if len(prefix) == 9:
                    break
            time.sleep(0.005)
        process.stdin.close()
        process.stdin = None
        rest, stderr = process.communicate(timeout=5)
        success = len(prefix) == 9 and prefix[4:] == b'\2\0\0\0\3'
        record = {'name': name, 'exit': process.returncode, 'sftp_v3': success, 'response_bytes': len(prefix) + len(rest), 'requests': broker.records, 'fixture_errors': broker.errors, 'samples': samples, 'host_record_bytes': known.stat().st_size, 'client_stderr': stderr.decode('utf-8', errors='replace')[-16000:]}
        if sample:
            seen = {p for s in samples for p in s['tree']} - {process.pid}
            time.sleep(0.05)
            record['observed_children_still_live'] = [p for p in seen if sample(p)]
            record['peak_tree_rss'] = max((sum(v['rss'] for v in s['tree'].values()) for s in samples), default=0)
            record['peak_processes'] = max((len(s['tree']) for s in samples), default=0)
        record['passed'] = ((success and process.returncode == 0) if expect_success else (not success and process.returncode != 0)) and not broker.errors and not record.get('observed_children_still_live')
        if unknown:
            record['passed'] &= bool(record['requests']) and record['requests'][0]['contains_fingerprint'] and ((known.stat().st_size > 0) == expect_success)
        return record
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=3)
        broker.close()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if os.name != 'posix' or not Path('/usr/sbin/sshd').is_file():
        parser.error('requires POSIX private Unix IPC and an isolated sshd; Windows remains a separate gate')
    root = Path(__file__).resolve().parent.parent
    result = {'scope': 'Optional standalone helper, generated private tickets and real OpenSSH askpass with encrypted test keys and isolated loopback sshd. Fixture broker supplies test answers; this is not the application frontend/state worker, password/2FA server, GUI, Windows or fair helper performance validation. No real user SSH files or credentials; records omit prompts, capabilities and answers. 30ms fixture response delay makes short-lived helpers observable.', 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'direct': [], 'ssh': []}
    server = None
    build = subprocess.run(['cargo', 'build', '--offline', '--release', '--features', 'ssh-auth-helper', '--bin', 'congminal-ssh-helper'], cwd=root, capture_output=True, text=True, timeout=180)
    result['build'] = {'exit': build.returncode, 'stdout': build.stdout, 'stderr': build.stderr}
    try:
        if build.returncode:
            raise RuntimeError(build.stderr[-8000:])
        helper = str(root / 'target/release/congminal-ssh-helper')
        result['helper_bytes'] = Path(helper).stat().st_size
        sample, result['sampler'] = sampler()
        with tempfile.TemporaryDirectory(prefix='cga-', dir='/tmp') as temporary:
            folder = Path(temporary).resolve()
            os.chmod(folder, 0o700)
            record = run_direct(helper, folder, 'maximum_whole', ('x' * 1020 + '文').encode(), 0, ('x' * 1020 + '文').encode(), split=False)
            result['direct'].append(record)
            print(json.dumps({'direct': 'maximum_whole', 'passed': record['passed']}), flush=True)
            if not record['passed']: raise RuntimeError('whole-frame helper check failed')
            for name, response, code, output, hint in [('maximum', ('x' * 1020 + '文').encode(), 0, ('x' * 1020 + '文').encode(), None), ('empty', b'', 0, b'', None), ('cancel', None, 1, b'', None), ('malformed', 'invalid', 2, b'', None), ('closed', False, 2, b'', None), ('invalid_utf8', b'\xff', 2, b'', None), ('confirmation', b'yes', 0, b'yes', 'confirm'), ('notice', b'', 0, b'', 'none')]:
                record = run_direct(helper, folder, name, response, code, output, hint)
                result['direct'].append(record)
                print(json.dumps({'direct': name, 'passed': record['passed']}), flush=True)
                if not record['passed']:
                    raise RuntimeError('direct helper check failed: ' + name)
            for name, phrase in [('host', ''), ('client', 'fixture only'), ('long', 'x' * 1020 + '文')]:
                subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', phrase, '-f', str(folder / name)], check=True)
                checked = subprocess.run(['ssh-keygen', '-y', '-P', phrase, '-f', str(folder / name)], capture_output=True)
                result.setdefault('fixture_decryption', {})[name] = checked.returncode == 0
                if checked.returncode: raise RuntimeError('generated fixture cannot be decrypted')
            (folder / 'authorized_keys').write_bytes((folder / 'client.pub').read_bytes() + (folder / 'long.pub').read_bytes())
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0)); port = probe.getsockname()[1]
            (folder / 'sshd.config').write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {folder}/host\nAuthorizedKeysFile {folder}/authorized_keys\nPidFile {folder}/pid\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY no\nSubsystem sftp internal-sftp\nLogLevel ERROR\n')
            with (folder / 'server.log').open('w') as log:
                server = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(folder / 'sshd.config')], stdout=log, stderr=log, start_new_session=True)
                for _ in range(100):
                    if server.poll() is not None:
                        raise RuntimeError('isolated sshd exited')
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=0.05): break
                    except OSError: time.sleep(0.01)
                else: raise RuntimeError('sshd not ready')
                for name, identity, responses, ok, unknown in [('passphrase', 'client', [b'fixture only'], True, False), ('maximum', 'long', [('x' * 1020 + '文').encode()], True, False), ('bad_passphrase', 'client', [b'wrong fixture'], False, False), ('cancel', 'client', [None], False, False), ('host_accept', 'client', [b'yes', b'fixture only'], True, True), ('host_decline', 'client', [b'no'], False, True)]:
                    record = run_ssh(helper, shutil.which('ssh'), root, folder, port, name, identity, responses, ok, unknown, sample)
                    result['ssh'].append(record)
                    print(json.dumps({'ssh': name, 'passed': record['passed']}), flush=True)
                    if not record['passed']: raise RuntimeError('SSH helper check failed: ' + name)
    finally:
        if server and server.poll() is None:
            os.killpg(server.pid, signal.SIGTERM)
            try: server.wait(timeout=3)
            except subprocess.TimeoutExpired: os.killpg(server.pid, signal.SIGKILL); server.wait(timeout=3)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')

if __name__ == '__main__': main()
