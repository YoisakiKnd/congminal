#!/usr/bin/env python3
"""Rust 父状态/私有端点/helper/OpenSSH 组合互操作，仅生成隔离回环凭据。"""
import argparse
import getpass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time

from check_auth_helper import environment
from check_ssh_transport import sampler

SOAK_CASE = 'sftp::service::soak_tests::isolated_repeated_service_lifecycle_keeps_credit_and_handles_bounded'
ENTRY_CASE = 'workspace::remote_files::entry::tests::isolated_preview_entry_lists_uploads_downloads_and_joins'
CASE = 'sftp::openssh::authenticated::tests::isolated_authenticated_subsystem_uses_worker_front_and_closes_stage_after_version'
SERVICE_CASE = 'sftp::service::tests::isolated_executor_authenticates_lists_and_joins_before_releasing_parent_credit'
TRANSFER_CASE = 'sftp::service::tests::isolated_executor_transfers_files_with_publication_reports_and_confirmed_shutdown'
WORKSPACE_TRANSFER_CASE = 'workspace::remote_files::transfer_tests::isolated_workspace_file_task_survives_directory_refresh_and_background_switch'
WORKSPACE_CASE = 'workspace::remote_files::real_connection::isolated_workspace_routes_authentication_directory_navigation_and_confirmed_exit'

def run_case(binary, helper, root, folder, port, name, sample, case=CASE, auth_preview=None, server_pid=None):
    known = folder / (name + '.known')
    public = (folder / 'host.pub').read_text().split()
    unknown = name in ['host_accept', 'host_decline']
    known.write_text('' if unknown else f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
    identity = 'long' if name == 'maximum' else 'client'
    config = folder / (name + '.config')
    config.write_text(f'Host fixture\n HostName 127.0.0.1\n Port {port}\n User {getpass.getuser()}\n IdentityFile "{folder / identity}"\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile "{known}"\n GlobalKnownHostsFile none\n StrictHostKeyChecking {"ask" if unknown else "yes"}\n NumberOfPasswordPrompts 2\n LogLevel ERROR\n')
    env = environment()
    env.update(CONGMINAL_AUTH_ROOT=str(folder), CONGMINAL_AUTH_CONFIG=str(config), CONGMINAL_AUTH_HELPER=str(helper), CONGMINAL_AUTH_SCENARIO=name)
    if auth_preview is not None and case == WORKSPACE_CASE and name == 'passphrase':
        env['CONGMINAL_SFTP_WORKSPACE_AUTH_PREVIEW'] = str(auth_preview.resolve())
    process = subprocess.Popen([binary, case, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    started = time.monotonic_ns()
    samples = []
    timed_out = False
    try:
        while process.poll() is None:
            row = {'ns': time.monotonic_ns() - started, 'tree': sample(process.pid)}
            if server_pid is not None: row['server_tree'] = sample(server_pid)
            samples.append(row)
            if time.monotonic_ns() - started > (1850_000_000_000 if case == SOAK_CASE else 25_000_000_000):
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                break
            time.sleep(1 if case == SOAK_CASE else 0.005)
        stdout, stderr = process.communicate(timeout=5)
        log = (stdout + stderr).decode('utf-8', 'replace')
        tag = 'SFTP_ENTRY_RESULT' if case == ENTRY_CASE else 'SFTP_WORKSPACE_RESULT' if case in [WORKSPACE_CASE,WORKSPACE_TRANSFER_CASE] else 'SFTP_SERVICE_RESULT' if case in [SERVICE_CASE, SOAK_CASE] else 'SFTP_TRANSFER_RESULT' if case == TRANSFER_CASE else 'AUTH_PARENT_RESULT'
        expected_cleanup = 'failed_expected' if case == WORKSPACE_CASE and name == 'cleanup_failure' else 'ok'
        match = re.search(tag + r' scenario=(\w+) requests=(\d+) peak=(\d+) final=0 cleanup=' + expected_cleanup + (r' max_tick_ns=(\d+)' if case in [WORKSPACE_CASE,WORKSPACE_TRANSFER_CASE] else ''), log)
        observed = set().union(*(set(s['tree']) for s in samples)) if samples else set()
        still_live = []
        for pid in observed - {str(process.pid)}:
            try: os.kill(int(pid), 0); still_live.append(int(pid))
            except ProcessLookupError: pass
        record = {'name': name, 'exit': process.returncode, 'timed_out': timed_out, 'log': log, 'samples': samples, 'observed_children_still_live': still_live, 'host_record_bytes': known.stat().st_size, 'elapsed_ns':time.monotonic_ns()-started}
        if match:
            record.update(requests=int(match[2]), managed_peak=int(match[3]))
            if case in [WORKSPACE_CASE,WORKSPACE_TRANSFER_CASE]: record.update(expected_cleanup=expected_cleanup, max_tick_ns=int(match[4]))
        record['passed'] = process.returncode == 0 and not timed_out and match is not None and match[1] == name and '1 passed' in log and not still_live
        if case == WORKSPACE_CASE and name == 'cleanup_failure':
            disposition = 'SFTP_EXIT_DISPOSITION cleanup=Failed acknowledged=1 joined=1 preserved=1' in log
            record['passed'] &= disposition
            record['exit_disposition'] = dict(acknowledged=disposition, joined=disposition, residual_preserved=disposition)
        if case == WORKSPACE_TRANSFER_CASE and (not name.startswith('download_lookup') or name=='download_lookup'):
            notice=re.search(r'SFTP_NOTICE_RESULT scenario=(\w+) count=1 result=(\w+)',log)
            expected_notice='Failed' if 'refuse' in name or 'retry' in name or name=='upload_stale_prepare' else 'Cancelled' if name in ['background_cancel','upload_cancel_before_auth'] else 'Published'
            record['passed'] &= notice is not None and notice[1]==name and notice[2]==expected_notice
            if notice:record['notice']=dict(count=1,result=notice[2])
        if case == WORKSPACE_TRANSFER_CASE and 'retry' in name:
            retry = re.search(r'SFTP_RETRY_RESULT scenario=(\w+) requests=(\d+) version=fresh old_record=retained target=verified', log)
            expected = 2 if name.startswith('download') else 1
            record['passed'] &= retry is not None and retry[1] == name and int(retry[2]) == expected
            if retry: record['retry'] = dict(requests=int(retry[2]), version='fresh', old_record='retained', target='verified')
        if case == WORKSPACE_TRANSFER_CASE and name.startswith('download_lookup'):
            lookup = re.search(r'SFTP_LOOKUP_RESULT scenario=(\w+) requests=(\d+) failure=(None|Some\(\w+\))',log)
            expected = {'download_lookup':'None','download_lookup_form_cancel':'Some(Cancelled)','download_lookup_stale':'Some(Changed)','download_lookup_symlink':'Some(Invalid)'}
            record['passed'] &= lookup is not None and lookup[1]==name and int(lookup[2])==(0 if name=='download_lookup_form_cancel' else 1) and (lookup[3]==expected[name] if name in expected else lookup[3]!='None')
            if lookup: record['lookup'] = dict(requests=int(lookup[2]),failure=lookup[3])
            if name=='download_lookup_form_cancel':
                retired=re.search(r'mode=(queued_retired|running_result)',log)
                record['passed'] &= retired is not None
                if retired: record['lookup']['cancel_mode']=retired[1]
            if name!='download_lookup':
                record['max_tick_ns']=None
                record['max_tick_scope']='not measured: lookup failure/cancel exits before the file loop'
        if case == TRANSFER_CASE:
            record['states'] = [dict(index=int(m[0]), bytes=int(m[1]), publication=m[2], end=m[3], cleanup=m[4], failure=m[5]) for m in re.findall(r'SFTP_TRANSFER_STATE index=(\d+) bytes=(\d+) publication=(.*?) end=Some\((\w+)\) cleanup=(\w+) failure=([^\n]+)', log)]
            success = name in ['upload', 'download', 'upload_replace', 'download_replace', 'concurrent', 'pressure']
            expected_end = 'Completed' if success else 'Cancelled' if name.endswith('cancel') or name == 'shutdown' else 'Failed'
            record['passed'] &= len(record['states']) == (2 if name in ['concurrent','pressure'] else 1) and all(s['end'] == expected_end and s['cleanup'] == 'Confirmed' and (not success or s['bytes'] == 33554432) for s in record['states'])
        if case == TRANSFER_CASE and name == 'pressure':
            pressure = re.search(r'SFTP_IO_PRESSURE admitted=(\d+) input=(\d+) input_limit=(\d+) io_headroom=(\d+)',log)
            if pressure: record['pressure'] = dict(zip(['admitted','input','input_limit','io_headroom'], map(int,pressure.groups())))
            record['passed'] &= pressure is not None and int(pressure[1])<64 and int(pressure[2])<=int(pressure[3]) and int(pressure[4])==9*1024*1024
        if name == 'host_accept': record['passed'] &= known.stat().st_size > 0
        if name == 'host_decline': record['passed'] &= known.stat().st_size == 0
        return record
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=3)

def lookup_cost_summary(cases):
    import statistics
    def tree_cost(record,key):
        rows=[s.get(key,{}) for s in record['samples']]
        pids=set().union(*(set(row) for row in rows))
        cpu=0
        for pid in pids:
            values=[row[pid]['cpu_ns'] for row in rows if pid in row]
            cpu+=max(values)-min(values)
        return dict(cpu_ns_observed_lower_bound=cpu,rss_peak_observed=max((sum(p['rss'] for p in row.values()) for row in rows),default=0),processes_peak_observed=max(map(len,rows),default=0))
    for record in cases:
        record['resources']={key:tree_cost(record,key) for key in ['tree','server_tree']}
    pairs=[]
    for index in range(5):
        pair={c['name']:c for c in cases if c['pair']==index}
        base=pair['download_background'];query=pair['download_lookup']
        row=dict(index=index,elapsed_delta_ns=query['elapsed_ns']-base['elapsed_ns'],elapsed_delta_percent=100*(query['elapsed_ns']/base['elapsed_ns']-1))
        for key in ['tree','server_tree']:
            for metric in ['cpu_ns_observed_lower_bound','rss_peak_observed']:
                row[key+'_'+metric+'_delta']=query['resources'][key][metric]-base['resources'][key][metric]
        pairs.append(row)
    keys=[k for k in pairs[0] if k!='index']
    return dict(pairs=pairs,paired={k:dict(median=statistics.median(p[k] for p in pairs),minimum=min(p[k] for p in pairs),maximum=max(p[k] for p in pairs)) for k in keys})

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--auth-preview', type=Path, help='workspace mode: save fixture masked software framebuffer as PPM')
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--preview-entry', action='store_true', help='actual opt-in preview entry, software auth and bounded upload/download; not native window')
    mode.add_argument('--service', action='store_true', help='exercise typed executor request through real auth/SFTP directory and joined shutdown')
    mode.add_argument('--workspace', action='store_true', help='exercise Workspace-owned service, protected input/software render, remote directory and close gate')
    mode.add_argument('--workspace-transfer', action='store_true', help='exercise fixed Workspace file records, actual deferred auth/controller parking, directory cancellation, background upload/download and cancel')
    mode.add_argument('--lookup-cost', action='store_true', help='two warmups/mode and five alternating pairs: cached attributes vs fallback LSTAT for the same 32MiB download workflow')
    mode.add_argument('--transfer', action='store_true', help='exercise actual typed upload/download jobs, source guards, atomic commit, cancellation and concurrent directions')
    mode.add_argument('--soak', action='store_true', help='30-minute repeated real Service/auth/directory/join lifecycle in one parent, OS tree sampled each second')
    args = parser.parse_args()
    if args.lookup_cost: args.workspace_transfer = True
    if args.auth_preview is not None and not args.workspace: parser.error('--auth-preview requires --workspace')
    if os.name != 'posix' or not Path('/usr/sbin/sshd').is_file():
        parser.error('requires Unix private IPC and an isolated local sshd')
    root = Path(__file__).resolve().parent.parent
    result = {'scope': 'Real Rust Front/Backend and bounded Unix IPC, optional helper and OpenSSH, generated encrypted keys with isolated loopback sshd. A simulated frontend thread supplies fixture answers; this is not GUI/password/2FA/Windows or fair performance validation. Positive cases list and verify 1MiB bidirectional data after VERSION; authentication stage is already closed. Waiting case delays answer 1.5s with a 1s network timeout. No user SSH files/agent/credentials. Samples include the Rust test harness and simulated frontend, not production idle costs.', 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'cases': []}
    if args.service:
        result['scope'] = 'Real typed Service directory request -> fixed executor -> deferred Front publication -> private IPC/helper/OpenSSH -> SFTP listing -> explicit transport/endpoint cleanup -> worker/reaper join and held-result parent-credit release. Generated encrypted keys and private loopback sshd only; fixture main thread supplies bounded answers. Eight auth/cancel/host-key/wait cases, 1.5s answer wait with 1s network timeout. No Workspace/GUI, native IME, transfer jobs, Windows, same-workload performance or idle-cost claim.'
    if args.workspace:
        result['scope'] = 'Real Workspace uniquely owns Service and 16MiB child, routes protected input/controller/software canvas -> private IPC/helper/OpenSSH/SFTP directories, source/profile invalidation, raw-byte navigation, explicit close gate and worker/reaper join. Existing isolated local cat PTY carries an SSH profile reference to verify no auth/name fallback; this is NOT a live SSH terminal pane, native window/focus/IME/clipboard or platform GUI test. 12 cases, including 1023-byte answer, rejection/cancellation, 1.5s visible wait (Workspace network timeout remains 10s), three independent navigation requests, close during auth, profile replacement, and expected endpoint-cleanup failure that preserves residual files, requires full warning review and a visible explicit acknowledgement after confirmed worker join; Enter/Esc/close never acknowledge. final=0 means SFTP parent credit, not all application allocations; process sampling includes harness/PTY. max_tick_ns is a single-run test diagnostic without presentation/native event costs, not GUI latency/performance acceptance. No transfer jobs, Windows, 30-minute idle or fair baseline claim.'
    if args.transfer:
        result['scope'] = 'Real Service Action/Output uploads/downloads through the same fixed 2-worker/64-wait/16MiB pool, deferred Front/helper/private IPC/OpenSSH to generated loopback sshd. 32MiB constant-buffer files exceed pool quota. Fifteen cases: both directions, existing target refusal and explicitly-approved primitive replacement, early streaming cancel, same-size source modification/remote mtime guard, stale remote selection, bad auth, streaming shutdown, concurrent upload+download, maximum queued metadata held under real dual-direction IO. Simulated frontend supplies fixture passphrase; mutations/cancel injected only after observing early streaming, not a frozen deterministic network schedule. Actual publication/retained partial bytes and clean joined shutdown/independent status ownership checked. No Workspace transfer GUI/real SSH terminal pane/native IME, other-platform runtime, whole-process allocation/RSS cap, fair throughput comparison or long-idle acceptance. Process samples include harness/client/helper; server is separate owned fixture. No user SSH files or credentials.'
    if args.workspace_transfer:
        result['scope'] = 'Sixteen real Workspace loopback cases with generated encrypted keys and private IPC/helper/OpenSSH/SFTP. Actual directory -> selected raw name/Version -> protected bounded path fields -> upload source inspection or download version lookup -> full software review/explicit overwrite -> 32MiB file transfer. Fixed file records, background authentication parking/fresh-view input rejection, streaming directory refresh/hide, pane switching, visible task cancel, source replacement refusal, explicit cleanup/join, byte verification and retained Status credit lifetime. Sidebar hidden after launch to isolate file-auth switching from automatic directory loads. Five lookup cases inject missing type/time into cached real entries; real LSTAT covers success, auth cancel, direct form cancel (either queued retirement without completion or running cancelled result), known-size mismatch and actual symlink refusal. No native file chooser/window/IME/clipboard, live SSH terminal pane, GUI latency, full-process memory cap, long-run or other-platform acceptance. Initial directory and lookup challenge counts asserted separately; requests in final marker counts file prompts (or lookup prompts on lookup-failure exits); max_tick_ns unavailable for early lookup exits. No user SSH files or credentials.'
    if args.lookup_cost:
        result['scope'] = 'Incremental whole-process workflow cost: same release Workspace fixture/server/generated encrypted key/32MiB content/download verification, software review and background switching; baseline has complete cached directory attributes, fallback removes cached type/time and adds real independent authenticated LSTAT. Input schemas and scenario-folder name lengths differ; not a same-input backend substitution or a 5% adoption gate. Two warmups/mode then five alternating pairs, all raw trials retained. Elapsed spans after Popen through exit/communicate, includes test harness, source-file preparation, existing local cat PTYs, UI/auth/IO/verification/shutdown and observer polling. Client and owned server trees sampled separately; observed CPU lower bounds miss PID edges/short children, summed RSS may double-count shared pages/miss peaks. Observer CPU excluded; no native GUI/input latency, isolated query latency, production idle, weak-network, long-run or other-platform acceptance.'
    if args.preview_entry:
        result['scope'] = 'Opt-in sftp-preview build: Config parsing, visible entry paint/click, deferred singleton Service, actual private helper/OpenSSH authentication, canonical directory, visible local picker/original source bytes, source preparation/full review, upload/download content and joined zero SFTP parent credit. Generated fixture keys/config only; software renderer and supplied focus on local cat PTY, not native window/IME, live SSH terminal or fair performance/whole-process memory/long-run acceptance.'
    result['case'] = SOAK_CASE if args.soak else ENTRY_CASE if args.preview_entry else WORKSPACE_TRANSFER_CASE if args.workspace_transfer else WORKSPACE_CASE if args.workspace else SERVICE_CASE if args.service else TRANSFER_CASE if args.transfer else CASE
    if args.soak:
        result['scope'] = 'One parent process, at least 1800s of repeated real Service creation, encrypted-key private IPC authentication, directory read, confirmed shutdown/join and zero parent SFTP credit per cycle. Generated loopback sshd and credentials, immutable profile precreated. Owned client/helper/ssh OS tree sampled once per second; short-lived children/peaks can be missed. Not GUI, latency or all-platform soak validation.'
    result['tool_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    result['manifest_sha256'] = {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in ['Cargo.toml', 'Cargo.lock']}
    result['versions'] = {}
    for name, command in [('rustc', ['rustc', '-Vv']), ('ssh', [shutil.which('ssh'), '-V'])]:
        version = subprocess.run(command, capture_output=True, text=True, timeout=10)
        result['versions'][name] = {'exit': version.returncode, 'stdout': version.stdout, 'stderr': version.stderr}
    server = None
    try:
        built = subprocess.run(['cargo', 'build', '--offline', '--release', '--features', 'ssh-auth-helper', '--bin', 'congminal-ssh-helper'], cwd=root, capture_output=True, text=True, timeout=180)
        result['helper_build'] = {'exit': built.returncode, 'stdout': built.stdout, 'stderr': built.stderr}
        if built.returncode: raise RuntimeError(built.stderr[-12000:])
        built = subprocess.run(['cargo', 'test', '--offline', '--release', '--no-run', '--message-format=json'] + (['--features','sftp-preview'] if args.preview_entry else []), cwd=root, capture_output=True, text=True, timeout=180)
        result['parent_build'] = {'exit': built.returncode, 'stderr': built.stderr}
        artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith('{')]
        result['parent_build']['diagnostics'] = [a['message'].get('rendered', '') for a in artifacts if a.get('reason') == 'compiler-message']
        if built.returncode: raise RuntimeError((''.join(result['parent_build']['diagnostics']) + built.stderr)[-12000:])
        binary = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target', {}).get('name') == 'congminal' and a.get('executable'))
        helper = root / 'target/release/congminal-ssh-helper'
        result['executables'] = {str(p.relative_to(root)): {'bytes': p.stat().st_size, 'sha256': hashlib.sha256(p.read_bytes()).hexdigest()} for p in [Path(binary), helper]}
        sample, result['sampler'] = sampler()
        with tempfile.TemporaryDirectory(prefix='cgp-', dir='/tmp') as temporary:
            folder = Path(temporary).resolve()
            os.chmod(folder, 0o700)
            (folder / 'remote').mkdir(mode=0o700)
            (folder / 'remote/fixture.txt').write_bytes(b'generated fixture only')
            for name, phrase in [('host', ''), ('client', 'fixture only'), ('long', 'x' * 1020 + '文')]:
                subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', phrase, '-f', str(folder / name)], check=True)
            (folder / 'authorized_keys').write_bytes((folder / 'client.pub').read_bytes() + (folder / 'long.pub').read_bytes())
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                port = probe.getsockname()[1]
            (folder / 'sshd.config').write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {folder}/host\nAuthorizedKeysFile {folder}/authorized_keys\nPidFile {folder}/pid\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {getpass.getuser()}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTTY no\nSubsystem sftp internal-sftp\nLogLevel VERBOSE\n')
            if args.preview_entry:
                conf=folder/'sshd.config'
                conf.write_text(conf.read_text().replace('Subsystem sftp internal-sftp\n',f'Subsystem sftp internal-sftp -d {folder}/remote\n'))
            # 密集的预期失败不得污染后续独立场景；只改本工具拥有的 sshd。
            expanded = subprocess.run(['/usr/sbin/sshd', '-T', '-f', str(folder / 'sshd.config')], capture_output=True, text=True, timeout=10)
            if expanded.returncode: raise RuntimeError('isolated server configuration rejected: ' + expanded.stderr)
            penalty_supported = any(line.startswith('persourcepenalties ') for line in expanded.stdout.splitlines())
            if penalty_supported:
                with (folder / 'sshd.config').open('a') as config_file:
                    config_file.write('PerSourcePenalties no\n')
            result['fixture_policy'] = {'per_source_penalties_supported': penalty_supported, 'disabled_only_in_own_fixture': penalty_supported, 'reason': 'Expected authentication failures must not penalize later independent cases; application/client policy unchanged.'}
            with (folder / 'server.log').open('w') as log:
                server = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(folder / 'sshd.config')], stdout=log, stderr=log, start_new_session=True)
                for _ in range(100):
                    if server.poll() is not None: raise RuntimeError('isolated sshd exited')
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=0.05): break
                    except OSError: time.sleep(0.01)
                else: raise RuntimeError('isolated sshd not ready')
                names = ['passphrase', 'maximum', 'bad_passphrase', 'cancel', 'task_cancel', 'host_accept', 'host_decline', 'wait']
                if args.workspace: names += ['navigation', 'close', 'profile_change', 'cleanup_failure']
                if args.preview_entry: names = ['entry']
                if args.soak: names = ['soak']
                if args.transfer:
                    names = ['upload', 'download', 'upload_existing', 'upload_replace', 'download_existing', 'download_replace', 'upload_cancel', 'download_cancel', 'upload_changed', 'download_changed', 'download_stale', 'bad_auth', 'shutdown', 'concurrent', 'pressure']
                if args.workspace_transfer: names = ['upload_background','download_background','background_cancel','upload_replace_review','download_replace_review','upload_refuse_review','download_refuse_review','upload_stale_prepare','upload_cancel_before_auth','upload_retry_review','download_retry_review','download_lookup','download_lookup_cancel','download_lookup_form_cancel','download_lookup_stale','download_lookup_symlink']
                if args.lookup_cost:
                    names=['download_background','download_lookup','download_lookup','download_background']
                    for pair in range(5):names += ['download_background','download_lookup'] if pair%2==0 else ['download_lookup','download_background']
                for index,name in enumerate(names):
                    case_folder=folder
                    if args.lookup_cost:
                        case_folder=folder/('t'+str(index));case_folder.mkdir(mode=0o700)
                        for key in ['host.pub','client']:shutil.copy2(folder/key,case_folder/key)
                    record = run_case(binary, helper, root, case_folder, port, name, sample, result['case'], args.auth_preview,server.pid if args.lookup_cost else None)
                    if args.lookup_cost:record.update(warmup=index<4,pair=None if index<4 else (index-4)//2)
                    result['cases'].append(record)
                    result['server_log'] = (folder / 'server.log').read_text()[-65536:]
                    print(json.dumps({k: record.get(k) for k in ['name', 'passed', 'managed_peak', 'requests']}), flush=True)
                    if not record['passed']: raise RuntimeError(record['log'][-12000:])
                if args.lookup_cost:result['cost_summary']=lookup_cost_summary(result['cases'])
    finally:
        if server and server.poll() is None:
            os.killpg(server.pid, signal.SIGTERM)
            try: server.wait(timeout=3)
            except subprocess.TimeoutExpired: os.killpg(server.pid, signal.SIGKILL); server.wait(timeout=3)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        result['source_unchanged'] = result['source_sha256'] == {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}
        args.output.write_text(json.dumps(result, indent=2) + '\n')
    if not result['source_unchanged']: raise RuntimeError('source changed during verification')

if __name__ == '__main__': main()
