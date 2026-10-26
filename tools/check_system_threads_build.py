#!/usr/bin/env python3
"""Managed PTY/config/layout threads, reserved startup and cancellation handles."""
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
    result = dict(scope='PTY reader/writer/waiter, configuration watcher and layout store now share fixed managed runtime; pre-reserved startup, native IO cancellation ownership and creator context. Default/preview full local runtime,3target static Clippy,MSRV tasks/runtime/PTY/watcher/layout,isolated unknown join,fmt/release. Not full GUI/data-plane/OS/RSS/all-platform/long-run acceptance',
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
        ['cargo','test','--offline','runtime::tests::unconfirmed_join_keeps_fee_and_fails_closed_in_an_isolated_process','--','--exact','--ignored','--test-threads=1'],
        ['rustup','run','1.88.0','cargo','test','--offline','runtime::tests','--','--test-threads=1'],
        ['rustup','run','1.88.0','cargo','test','--offline','runtime::tests::unconfirmed_join_keeps_fee_and_fails_closed_in_an_isolated_process','--','--exact','--ignored','--test-threads=1'],
        ['rustup','run','1.88.0','cargo','test','--offline','pty::','--','--test-threads=1'],
        ['rustup','run','1.88.0','cargo','test','--offline','configuration_monitor_admission_and_cancel_follow_creator_budget','--','--test-threads=1'],
        ['rustup','run','1.88.0','cargo','test','--offline','store_runtime_admits_before_read_and_keeps_fee_until_worker_exit','--','--test-threads=1'],
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
