#!/usr/bin/env python3
"""LatestTask core and paid cancellation admission, publication and retirement checks."""
import argparse
import hashlib
import json
from pathlib import Path
from check_font_discovery import sources
from check_sftp_executor import run


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    result = dict(scope='LatestTask shared core/paid token admission, atomic publication, owner retirement, version exhaustion and worker failure; default/preview runtime, three targets static Clippy, Rust1.88 tasks and helper runtime, fmt/release. Cross checks are not native runtime; thread stack/runtime/TLS/default tokens/generic heap payloads and global shutdown remain unproven.',
                  source_sha256=sources(root),
                  manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']},
                  tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), checks=[])
    commands = [
        ['rustc', '-Vv'],
        ['cargo', 'test', '--offline'],
        ['cargo', 'test', '--offline', '--features', 'sftp-preview'],
        ['cargo', 'clippy', '--offline', '--all-targets', '--features', 'sftp-preview,sftp-libssh2,directory-helper', '--', '-D', 'warnings'],
        ['cargo', 'clippy', '--offline', '--all-targets', '--features', 'sftp-preview', '--target', 'x86_64-unknown-linux-gnu', '--', '-D', 'warnings'],
        ['cargo', 'clippy', '--offline', '--all-targets', '--features', 'sftp-preview', '--target', 'x86_64-pc-windows-gnu', '--', '-D', 'warnings'],
        ['rustup', 'run', '1.88.0', 'cargo', 'test', '--offline', 'tasks::', '--', '--test-threads=1'],
        ['rustup', 'run', '1.88.0', 'cargo', 'test', '--offline', '--features', 'ssh-auth-helper', '--bin', 'congminal-ssh-helper', '--', '--test-threads=1'],
        ['cargo', 'fmt', '--all', '--', '--check'],
        ['cargo', 'build', '--offline', '--release', '--bin', 'congminal'],
    ]
    try:
        for command in commands:
            check = run(command, root, 240)
            result['checks'].append(check)
            print(json.dumps(dict(command=command, exit=check['exit'])), flush=True)
            if check['exit']:
                raise RuntimeError(check['stdout'][-8000:]+check['stderr'][-8000:])
        assert sources(root) == result['source_sha256']
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
