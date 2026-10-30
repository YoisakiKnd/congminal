#!/usr/bin/env python3
"""Disposable Linux CI only: loopback password authentication with a temporary local account and isolated sshd."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

CASE = 'sftp::service::native_openssh_tests::native_authenticated_directory_roundtrip_cancel_and_join'


def run(command, **kwargs):
    result = subprocess.run(command, capture_output=True, text=True, timeout=240, **kwargs)
    if result.returncode:
        raise RuntimeError(str(command[0]) + ': ' + (result.stdout + result.stderr)[-10000:])
    return result


def main():
    assert os.name == 'posix' and Path('/usr/sbin/sshd').is_file()
    assert os.environ.get('GITHUB_ACTIONS') == 'true', 'temporary account fixture is restricted to disposable CI'
    repository = Path(__file__).resolve().parent.parent
    run(['cargo', 'build', '--locked', '--release', '--bins'], cwd=repository)
    built = run(['cargo', 'test', '--locked', '--release', '--no-run', '--message-format=json'], cwd=repository)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith('{')]
    binary = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact'
                  and a.get('target', {}).get('name') == 'congminal' and a.get('profile', {}).get('test')
                  and a.get('executable'))
    user = 'cg' + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix='cg-pwd-', dir='/tmp') as temporary:
        root = Path(temporary)
        # Allow traversal to the isolated server user's own remote directory; no public read or write.
        root.chmod(0o711)
        remote = root / 'remote'
        remote.mkdir(mode=0o700)
        (remote / 'fixture.txt').write_bytes(b'generated fixture only')
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(root / 'host')])
        created = False
        server = None
        try:
            run(['sudo', 'useradd', '-M', '-U', '-d', str(remote), '-s', '/bin/sh', user])
            created = True
            # Deliberately public fixture password, accepted only by this generated local-only sshd/account.
            run(['sudo', 'chpasswd'], input=user + ':fixture only\n')
            run(['sudo', 'chown', '-R', user + ':' + user, str(remote)])
            run(['sudo', 'mkdir', '-p', '/run/sshd'])
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                port = probe.getsockname()[1]
            (root / 'known_hosts').write_text('')
            (root / 'client.config').write_text(
                f'Host fixture\n HostName 127.0.0.1\n Port {port}\n User {user}\n'
                f' UserKnownHostsFile {root}/known_hosts\n GlobalKnownHostsFile none\n'
                ' IdentityAgent none\n IdentityFile none\n PubkeyAuthentication no\n'
                ' PreferredAuthentications password\n StrictHostKeyChecking ask\n NumberOfPasswordPrompts 1\n'
                ' ControlMaster no\n LogLevel ERROR\n')
            server_config = root / 'server.config'
            server_config.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/host\n'
                f'PidFile {root}/pid\nAllowUsers {user}\nPasswordAuthentication yes\nPubkeyAuthentication no\n'
                'KbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin no\nPermitTTY no\n'
                'AllowTcpForwarding no\nX11Forwarding no\n'
                f'ForceCommand internal-sftp -d {remote}\nSubsystem sftp internal-sftp\nLogLevel VERBOSE\n')
            with (root / 'server.log').open('w') as log:
                server = subprocess.Popen(['sudo', '/usr/sbin/sshd', '-D', '-e', '-f', str(server_config)],
                                          stdout=log, stderr=log, start_new_session=True)
                deadline = time.monotonic() + 10
                while True:
                    if server.poll() is not None:
                        raise RuntimeError('isolated sshd exited: ' + (root/'server.log').read_text()[-8000:])
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=0.1):
                            break
                    except OSError:
                        assert time.monotonic() < deadline
                        time.sleep(0.05)
                env = dict(os.environ, CONGMINAL_NATIVE_ROOT=str(root), CONGMINAL_NATIVE_SSH='/usr/bin/ssh',
                    CONGMINAL_NATIVE_HELPER=str(repository/'target/release/congminal-ssh-helper'))
                result = subprocess.run([binary, CASE, '--exact', '--ignored', '--nocapture', '--test-threads=1'],
                    cwd=repository, env=env, capture_output=True, text=True, timeout=120)
                print(result.stdout + result.stderr)
                if result.returncode or 'NATIVE_SFTP_BACKEND platform=linux' not in result.stderr:
                    print((root/'server.log').read_text()[-12000:])
                    raise RuntimeError('native password SFTP verification failed')
        finally:
            if server is not None:
                if server.poll() is None:
                    subprocess.run(['sudo', 'kill', '--', '-' + str(server.pid)], capture_output=True, timeout=10)
                server.wait(timeout=10)
            if created:
                run(['sudo', 'chown', '-R', '--no-dereference', str(os.getuid()) + ':' + str(os.getgid()), str(root)])
                run(['sudo', 'userdel', user])


if __name__ == '__main__':
    main()
