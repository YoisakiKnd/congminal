#!/usr/bin/env python3
"""Disposable Windows CI: official pinned OpenSSH, generated encrypted key, native Service/IPC/file IO."""
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request
import zipfile

URL = 'https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.4.0.0p1-Preview/OpenSSH-Win64.zip'
SHA256 = 'b67ff8becd203502190ec42ac7787c24c2cd3325ed7700485fe7a1c83d2a7957'
CASE = 'sftp::service::native_openssh_tests::native_authenticated_directory_roundtrip_cancel_and_join'


def run(command, **kwargs):
    result = subprocess.run(command, capture_output=True, text=True, timeout=240, **kwargs)
    if result.returncode:
        raise RuntimeError(str(command[0]) + ': ' + (result.stdout + result.stderr)[-12000:])
    return result


def main():
    if os.name != 'nt':
        raise RuntimeError('requires disposable Windows CI runner')
    repository = Path(__file__).resolve().parent.parent
    run(['cargo', 'build', '--locked', '--release', '--bins'], cwd=repository)
    built = run(['cargo', 'test', '--locked', '--release', '--no-run', '--message-format=json'], cwd=repository)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith('{')]
    binary = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact'
                  and a.get('target', {}).get('name') == 'congminal' and a.get('profile', {}).get('test')
                  and a.get('executable'))
    with tempfile.TemporaryDirectory(prefix='cg-native-') as temporary:
        root = Path(temporary)
        archive = root / 'openssh.zip'
        digest = hashlib.sha256()
        length = 0
        with urllib.request.urlopen(URL, timeout=60) as response, archive.open('wb') as output:
            while True:
                block = response.read(65536)
                if not block:
                    break
                length += len(block)
                if length > 64 * 1024 * 1024:
                    raise RuntimeError('OpenSSH fixture exceeds download limit')
                digest.update(block)
                output.write(block)
        if digest.hexdigest() != SHA256:
            raise RuntimeError('official OpenSSH fixture checksum mismatch')
        with zipfile.ZipFile(archive) as bundle:
            for entry in bundle.infolist():
                if not (root / entry.filename).resolve().is_relative_to(root.resolve()):
                    raise RuntimeError('invalid fixture archive path')
            bundle.extractall(root)
        ssh_root = root / 'OpenSSH-Win64'
        keygen = ssh_root / 'ssh-keygen.exe'
        for name, phrase in [('host', ''), ('client', 'fixture only')]:
            run([str(keygen), '-q', '-t', 'ed25519', '-N', phrase, '-f', str(root / name)])
        (root / 'authorized_keys').write_bytes((root / 'client.pub').read_bytes())
        remote = root / 'remote'
        remote.mkdir()
        (remote / 'fixture.txt').write_bytes(b'generated fixture only')
        # Same user only; no user/service/account/firewall changes. Host-key protection also accepts Administrators.
        identity = json.loads(run(['powershell.exe', '-NoProfile', '-Command',
                                  '[Security.Principal.WindowsIdentity]::GetCurrent().User.Value | ConvertTo-Json']).stdout)
        acl_script = root / 'private-file.ps1'
        acl_script.write_text("""param([string]$File, [string]$OwnerSid)
$ErrorActionPreference = 'Stop'
$acl = New-Object System.Security.AccessControl.FileSecurity
$acl.SetOwner((New-Object System.Security.Principal.SecurityIdentifier($OwnerSid)))
$acl.SetAccessRuleProtection($true, $false)
foreach ($sid in @($OwnerSid, 'S-1-5-18', 'S-1-5-32-544')) {
    $identity = New-Object System.Security.Principal.SecurityIdentifier($sid)
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($identity, 'FullControl', 'Allow')
    $acl.AddAccessRule($rule)
}
[System.IO.File]::SetAccessControl($File, $acl)
""")
        for name in ['host', 'client', 'authorized_keys']:
            run(['powershell.exe', '-NoProfile', '-File', str(acl_script), str(root / name), identity])
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            port = probe.getsockname()[1]
        known = root / 'known_hosts'
        known.write_text('')
        (root / 'client.config').write_text(
            f'Host fixture\n HostName 127.0.0.1\n Port {port}\n User {os.environ["USERNAME"]}\n'
            f' IdentityFile "{(root / "client").as_posix()}"\n IdentitiesOnly yes\n IdentityAgent none\n'
            f' UserKnownHostsFile "{known.as_posix()}"\n GlobalKnownHostsFile none\n'
            ' StrictHostKeyChecking ask\n NumberOfPasswordPrompts 1\n ControlMaster no\n LogLevel ERROR\n')
        server_config = root / 'server.config'
        server_config.write_text(
            f'Port {port}\n ListenAddress 127.0.0.1\n HostKey "{(root / "host").as_posix()}"\n'
            f' AuthorizedKeysFile "{(root / "authorized_keys").as_posix()}"\n'
            ' PasswordAuthentication no\n KbdInteractiveAuthentication no\n StrictModes yes\n'
            ' AllowTcpForwarding no\n X11Forwarding no\n PermitTTY no\n'
            f' Subsystem sftp "{(ssh_root / "sftp-server.exe").as_posix()}" -d "{remote.as_posix()}"\n')
        with (root / 'server.log').open('w') as log:
            server = subprocess.Popen([str(ssh_root / 'sshd.exe'), '-D', '-e', '-f', str(server_config)],
                                      stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 15
                while True:
                    if server.poll() is not None:
                        raise RuntimeError('isolated sshd exited: ' + (root / 'server.log').read_text()[-8000:])
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=0.1):
                            break
                    except OSError:
                        if time.monotonic() > deadline:
                            raise RuntimeError('isolated sshd not ready')
                        time.sleep(0.05)
                env = dict(os.environ, CONGMINAL_NATIVE_ROOT=str(root),
                           CONGMINAL_NATIVE_HELPER=str(repository / 'target/release/congminal-ssh-helper.exe'),
                           CONGMINAL_NATIVE_SSH=str(ssh_root / 'ssh.exe'))
                result = subprocess.run([binary, CASE, '--exact', '--ignored', '--nocapture', '--test-threads=1'],
                                        cwd=repository, env=env, capture_output=True, text=True, timeout=120)
                print(result.stdout + result.stderr)
                if result.returncode or 'NATIVE_SFTP_BACKEND' not in result.stderr:
                    print((root / 'server.log').read_text()[-12000:])
                    raise RuntimeError('native Windows SFTP verification failed')
            finally:
                if server.poll() is None:
                    subprocess.run(['taskkill.exe', '/PID', str(server.pid), '/T', '/F'],
                                   capture_output=True, timeout=10, check=False)
                server.wait(timeout=10)
                # The native test confirms client/helper job join before this server fixture is removed.


if __name__ == '__main__':
    main()
