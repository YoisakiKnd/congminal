#!/usr/bin/env python3
"""Keep a generated loopback SFTP fixture and the packaged Cocoa app alive for native UI verification."""
import argparse
import getpass
import json
import os
from pathlib import Path
import shlex
import signal
import socket
import subprocess
import tempfile
import time


def descendants(pid):
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid='], text=True).splitlines()
    edges = [tuple(map(int, row.split())) for row in rows]
    owned = {pid}
    while True:
        found = {child for child, parent in edges if parent in owned}
        if found <= owned:
            return owned
        owned |= found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--state', type=Path, required=True)
    args = parser.parse_args()
    assert os.uname().sysname == 'Darwin'
    root = Path(tempfile.mkdtemp(prefix='cg-ui-', dir='/tmp')).resolve()
    root.chmod(0o700)
    remote = root / 'remote'
    remote.mkdir(mode=0o700)
    (remote / 'hello.txt').write_text('native SFTP fixture\n中文路径与二进制都应保留。\n')
    (remote / 'empty').mkdir(mode=0o700)
    (root / 'source.txt').write_text('native upload fixture\n')
    for name, phrase in [('host', ''), ('client', 'fixture only')]:
        subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', phrase, '-f', str(root / name)], check=True)
    (root / 'authorized_keys').write_bytes((root / 'client.pub').read_bytes())
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        port = probe.getsockname()[1]
    public = (root / 'host.pub').read_text().split()
    (root / 'known_hosts').write_text(f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
    client = root / 'client.config'
    client.write_text(f'Host *\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n'
                      f' IdentityFile {root}/client\n IdentitiesOnly yes\n IdentityAgent none\n'
                      f' UserKnownHostsFile {root}/known_hosts\n GlobalKnownHostsFile none\n'
                      ' StrictHostKeyChecking yes\n ControlMaster no\n LogLevel ERROR\n')
    # All terminal/background paths use the isolated config; no personal agent/config or startup shell is used.
    wrapper = root / 'ssh-fixture'
    wrapper.write_text('#!/bin/sh\nexec /usr/bin/ssh -F ' + shlex.quote(str(client)) + ' "$@"\n')
    wrapper.chmod(0o700)
    server_config = root / 'sshd.config'
    server_config.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {root}/host\n'
                             f'AuthorizedKeysFile {root}/authorized_keys\nPidFile {root}/pid\n'
                             'StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n'
                             f'AllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY no\n'
                             f'ForceCommand internal-sftp -d {remote}\nSubsystem sftp internal-sftp\nLogLevel ERROR\n')
    app = args.app.resolve()
    config = root / 'congminal.toml'
    config.write_text(f'''[window]
columns = 120
rows = 34
[sftp]
ssh-config = '{client}'
[connections.fixture]
kind = 'ssh'
name = '原生 SFTP 验收'
host = '127.0.0.1'
program = '{wrapper}'
''')
    with (root / 'server.log').open('w') as log, (root / 'application.log').open('w') as app_log:
        server = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(server_config)],
                                  stdout=log, stderr=log, start_new_session=True)
        application = None
        try:
            deadline = time.monotonic() + 5
            while True:
                if server.poll() is not None:
                    raise RuntimeError('fixture sshd exited')
                try:
                    with socket.create_connection(('127.0.0.1', port), timeout=0.1):
                        break
                except OSError:
                    assert time.monotonic() < deadline
                    time.sleep(0.02)
            application = subprocess.Popen([str(app / 'Contents/MacOS/congminal')],
                env=dict(os.environ, CONGMINAL_CONFIG=str(config)), stdout=app_log, stderr=app_log)
            args.state.write_text(json.dumps(dict(root=str(root), app=str(app), pid=application.pid, port=port)))
            print(args.state.read_text(), flush=True)
            application.wait(timeout=1200)
            print('NATIVE_APP_EXIT=' + str(application.returncode), flush=True)
        finally:
            # Emergency fixture timeout only: retire our own process trees. Normal UI close uses production join.
            for process in [application, server]:
                if process is not None and process.poll() is None:
                    for pid in sorted(descendants(process.pid), reverse=True):
                        try:
                            os.kill(pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                    process.wait(timeout=10)
            # Retain the disposable fixture and files for byte comparison and evidence; no recursive deletion.


if __name__ == '__main__':
    main()
