#!/usr/bin/env python3
"""隔离 OpenSSH 独立流适配：默认无库依赖测试程序，生成密钥及受控配置。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

from check_sftp_transport import PEER, run_case
from check_ssh_transport import sampler

SCENARIOS = ['positive', 'resolved', 'paths', 'host-alias', 'hashed', 'agent', 'unknown', 'changed', 'revoked', 'auth', 'huge', 'name', 'stall', 'close', 'timeout']

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--scenario', choices=['all'] + SCENARIOS, default='all')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if os.name != 'posix' or not Path('/usr/sbin/sshd').is_file():
        parser.error('requires isolated POSIX sshd; Windows runtime is a separate gate')
    root = Path(__file__).resolve().parent.parent
    result = {'scope': 'Default-dependency test binary and real OpenSSH subsystem over generated loopback sshd. Functional 32MiB streaming, quoted known_hosts list, Include/Match, aliases and faulty peers. This is not a balanced throughput benchmark, production UI, interactive authentication, full-heap admission or Windows runtime validation.', 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'runs': []}
    build = subprocess.run(['cargo', 'test', '--offline', '--release', '--no-run', '--message-format=json'], cwd=root, capture_output=True, text=True, timeout=180)
    result['build'] = {'exit': build.returncode, 'stderr': build.stderr, 'diagnostics': []}
    executable = None
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if item.get('reason') == 'compiler-message':
            result['build']['diagnostics'].append(item.get('message', {}).get('rendered', ''))
        if item.get('reason') == 'compiler-artifact' and item.get('target', {}).get('name') == 'congminal' and item.get('profile', {}).get('test') and item.get('executable'):
            executable = item['executable']
    try:
        if build.returncode or not executable:
            raise RuntimeError(build.stderr + ''.join(result['build']['diagnostics']))
        sample, result['sampler'] = sampler()
        with tempfile.TemporaryDirectory(prefix='congminal-sftp-stream-') as temporary:
            folder = Path(temporary)
            os.chmod(folder, 0o700)
            for name in ['host', 'changed-host', 'client', 'wrong']:
                subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(folder / name)], check=True)
            (folder / 'authorized_keys').write_bytes((folder / 'client.pub').read_bytes())
            listing = folder / 'remote/list'
            listing.mkdir(parents=True)
            (listing / 'directory').mkdir()
            (listing / '- space 中文\n.bin').write_bytes(b'binary\0\xff')
            (listing / '.hidden').touch()
            (folder / 'peer.py').write_text(PEER)
            for scenario in SCENARIOS if args.scenario == 'all' else [args.scenario]:
                record = run_case(executable, '', root, folder, scenario, sample, backend='openssh')
                result['runs'].append(record)
                print(json.dumps({k: record[k] for k in ['scenario', 'exit', 'passed']}), flush=True)
                if not record['passed']:
                    raise RuntimeError(record['log'][-8000:])
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')

if __name__ == '__main__':
    main()
