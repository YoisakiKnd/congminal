#!/usr/bin/env python3
"""临时主线程可执行文件：真实 NSTextInputContext/client 生命周期和 Rust 分配。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import signal
import subprocess
import tempfile


def run(command, cwd, timeout):
    process = subprocess.Popen(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, encoding='utf-8', errors='replace', start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate(timeout=10)
        return {'command': command, 'exit': process.returncode, 'stdout': stdout, 'stderr': stderr, 'timed_out': True}
    return {'command': command, 'exit': process.returncode, 'stdout': stdout, 'stderr': stderr}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--bridge', action='store_true', help='actual view callbacks/manager with explicitly supplied focus state; not system foreground verification')
    parser.add_argument('--framework-focus', action='store_true', help='explicitly simulate framework key-window notifications; does not prove system foreground activation')
    parser.add_argument('--window-trace', action='store_true', help='diagnostic numeric key dispatch without character text')
    parser.add_argument('--window', action='store_true', help='exercise own NSWindow, posted NSEvents, focus rotation and wake scheduling')
    parser.add_argument('--editor', action='store_true', help='route real Objective-C callbacks through common input to the actual editor')
    parser.add_argument('--trace-allocations', action='store_true', help='diagnostic backtraces for active 64-byte allocations; timings are not comparable')
    parser.add_argument('--toolchain', help='already installed Rust toolchain; never installs')
    args = parser.parse_args()
    if sum([args.window,args.editor,args.bridge])>1: parser.error('choose --window, --bridge or --editor')
    root = Path(__file__).resolve().parent.parent
    if platform.system() != 'Darwin':
        raise SystemExit('requires native macOS; cross compilation is not runtime verification')
    fixtures = [root / 'tools/fixtures' / name for name in ['auth_allocation_meter.rs', 'ime_bridge_probe.rs' if args.bridge else 'ime_window_probe.rs' if args.window else 'ime_editor_probe.rs' if args.editor else 'ime_native_probe.rs']]
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    result = {
        'scope': 'Temporary main-thread native macOS process with actual NSTextInputContext subclass strongly owning an immutable-client proxy. External retain survives UI owner retirement; direct Objective-C callbacks on retired/active clients, extracted output credit, autorelease drain and release-before-free scrubbing. Framework, both shared Budget mutex contexts, thread and clock initialization are outside Rust meter; first context timing and 20 warm construct/activate/deactivate/drop rounds are microdiagnostics. No window, real IME keystrokes, selected-source changes, GUI presentation, full C/ObjC/OS allocation/RSS or end-to-end authentication claim.',
        'platform': platform.platform(),
        'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))},
        'fixtures': {str(p.relative_to(root)): {'sha256': hashlib.sha256(p.read_bytes()).hexdigest(), 'source': p.read_text()} for p in fixtures},
        'tool_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'manifests_sha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in ['Cargo.toml', 'Cargo.lock']},
    }
    if args.bridge:
        result['scope'] = 'Actual NSWindow/view callbacks -> protected bridge -> Controller/Panel/Editor/Backend with EXPLICITLY SUPPLIED focus: composition/coalesced wake, scoped menu paste, focus recreation and stale-paste refusal, masked software render before submission, blocked waiting, confirmation Enter=no and explicit reviewed yes, stale decision and old-client refusal, ordinary context restoration and cleanup. Focus is model-driven because system foreground activation is not available; no system-focus, physical IME or GUI latency claim. Own test window only, actual Backend/Front/Editor, no input-source changes.'
    if args.window:
        result['scope'] = 'Temporary native macOS main-thread own NSWindow and synthetic posted NSEvents: ordinary named key, protected SelectAll, direct async public-text callback, wake scheduling, real focus loss/recovery with fresh editor view, retained retired-client callback and protected Cancel. Actual Backend/Front/Editor, no SSH/helper, no system input-source changes, no physical IME composition or presentation-latency claim. Creates and closes only its own two test windows; records ledger cleanup, not full C/OS/RSS.'
    if args.editor:
        result['scope'] = 'Temporary macOS main-thread native Cocoa callback -> common protected input -> actual Backend/Front/Editor. Composition and maximum commit borrow output without additional String allocation; 200 warm direct-callback/pop/consume events check one decoded String allocation per event, not GUI input latency; recreated-view and closed-challenge rejection, five release-before-free event scrub checks, actual context/external-retain/autorelease cleanup. Auth module is enabled and unused visual modules disabled only in temporary harness; no changes to backend/editor behavior. Excludes C/ObjC/OS/RSS, stacks and preinitialized shared budget/thread/clock/framework state. No window, actual IME keystrokes, SSH/helper, viewport review or input-to-presentation claim.'
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-ime-native-') as temporary:
            copy = Path(temporary)
            for name in ['src', 'assets']:
                shutil.copytree(root / name, copy / name)
            for name in ['Cargo.toml', 'Cargo.lock']:
                shutil.copy2(root / name, copy / name)
            meter = fixtures[0].read_text()
            if args.trace_allocations:
                meter = meter.replace('if !pointer.is_null() { record(layout.size(), 0, false); }',
                    'if !pointer.is_null() { record(layout.size(), 0, false); if layout.size()==64 && STATE.try_with(|s|s.get().active).unwrap_or(false) { trace_allocation(); } }')
                meter += '''\nfn trace_allocation(){unsafe extern "C"{fn backtrace(buffer:*mut *mut core::ffi::c_void,size:i32)->i32;fn backtrace_symbols_fd(buffer:*const *mut core::ffi::c_void,size:i32,fd:i32);}let mut stack=[core::ptr::null_mut();48];unsafe{let size=backtrace(stack.as_mut_ptr(),48);backtrace_symbols_fd(stack.as_ptr(),size,2);}}\n'''
                result['diagnostic_meter'] = meter
            (copy / 'src/auth_allocation_meter.rs').write_text(meter)
            main_file = copy / 'src/main.rs'
            main_source = main_file.read_text().replace('fn main() {', 'fn app_main_before_native_probe() {', 1)
            main_file.write_text(main_source + '\nmod auth_allocation_meter;\nfn main(){platform::run_native_ime_probe();}\n')
            mac = copy / 'src/platform/macos.rs'
            source = mac.read_text()
            assert 'mod protected_ime;' in source
            mac.write_text(source.replace('#[cfg(test)]\nmod protected_ime;', 'mod protected_ime;').replace('mod protected_ime;', 'pub(super) mod protected_ime;', 1))
            ime = copy / 'src/platform/macos/protected_ime.rs'
            if args.window_trace:
                diagnostic = mac.read_text().replace('let flags: usize = msg![event, "modifierFlags"; usize];',
                    'let flags: usize = msg![event, "modifierFlags"; usize]; eprintln!("IME_WINDOW_KEY flags={} code={} managed={}",flags,msg![event,"keyCode";u16],protected_window::managed(this));',1)
                mac.write_text(diagnostic)
                bridge=copy/'src/platform/macos/protected_window.rs'
                bridge.write_text(bridge.read_text().replace('let handle = match lookup(view) {','eprintln!("IME_WINDOW_ROUTE plain={} blocked={} active={}",matches!(lookup(view),Lookup::Plain),matches!(lookup(view),Lookup::Blocked),matches!(lookup(view),Lookup::Active(_))); let handle = match lookup(view) {',1))
                native=copy/'src/platform/macos/protected_ime.rs'
                native.write_text(native.read_text().replace('fn enqueue_command(client: Id, command: InputCommand) {','fn enqueue_command(client: Id, command: InputCommand) { eprintln!("IME_WINDOW_COMMAND active={} rejected={} used={}",unsafe {owner(client)}.state.borrow().active,unsafe {owner(client)}.state.borrow().rejected,unsafe {owner(client)}.state.borrow().used);',1).replace('fn pop(client: Id) -> Option<Input> {','fn pop(client: Id) -> Option<Input> { eprintln!("IME_WINDOW_POP active={} used={}",unsafe {owner(client)}.state.borrow().active,unsafe {owner(client)}.state.borrow().used);',1))
            target = mac if args.window or args.bridge else ime
            fixture=fixtures[1].read_text()
            if args.framework_focus:
                fixture=fixture.replace('0=>{\n                    if msg!', '0=>{\n                    msg![self.window,"becomeKeyWindow";()];\n                    if msg!',1)
                fixture=fixture.replace('msg![self.aux,"makeKeyAndOrderFront:",NIL=>Id;()];assert_eq!', 'msg![self.window,"resignKeyWindow";()];msg![self.aux,"becomeKeyWindow";()];assert_eq!',1)
                fixture=fixture.replace('msg![self.window,"makeKeyAndOrderFront:",NIL=>Id;()];msg![self.aux,"orderOut:",NIL=>Id;()];', 'msg![self.aux,"resignKeyWindow";()];msg![self.window,"becomeKeyWindow";()];msg![self.aux,"orderOut:",NIL=>Id;()];',1)
                result['scope'] += ' Framework key-window state is explicitly simulated via becomeKeyWindow/resignKeyWindow; system foreground activation is NOT verified.'
            if args.window_trace:
                fixture=fixture.replace('0=>{\n                    if msg!', '0=>{\n                    eprintln!("IME_WINDOW_STATE visible={} can_key={} key={} active={} running={}",msg![self.window,"isVisible";i8],msg![self.window,"canBecomeKeyWindow";i8],msg![self.window,"isKeyWindow";i8],msg![msg![class(c"NSApplication"),"sharedApplication";Id],"isActive";i8],msg![msg![class(c"NSApplication"),"sharedApplication";Id],"isRunning";i8]);\n                    if msg!',1)
            target.write_text(target.read_text() + '\n' + fixture)
            if args.editor or args.window or args.bridge:
                ssh = copy / 'src/ssh.rs'
                source = ssh.read_text()
                assert '#[cfg(test)]\npub(crate) mod auth;' in source
                ssh.write_text(source.replace('#[cfg(test)]\npub(crate) mod auth;', 'pub(crate) mod auth;', 1))
                auth = copy / 'src/ssh/auth.rs'
                source = auth.read_text()
                for name in ([] if args.bridge else ['auth_view.rs', 'auth_viewport.rs', 'auth_controller.rs']):
                    source = source.replace('#[path = "' + name + '"]', '#[cfg(any())]\n#[path = "' + name + '"]', 1)
                auth.write_text(source)
                if args.bridge:
                    render = copy / 'src/render.rs'
                    source = render.read_text()
                    for declaration in ['    pub(crate) fn auth_text(', '    pub(crate) fn auth_caret(']:
                        assert '    #[cfg(test)]\n' + declaration in source
                        source = source.replace('    #[cfg(test)]\n' + declaration, declaration, 1)
                    source = source.replace('#[cfg(test)]\nfn clipped_bounds(', 'fn clipped_bounds(', 1)
                    source = source.replace('#[cfg(test)]\nfn blend_auth_glyph(', 'fn blend_auth_glyph(', 1)
                    render.write_text(source)
            mod = copy / 'src/platform/mod.rs'
            entry = 'native_bridge_probe' if args.bridge else 'native_window_probe' if args.window else 'protected_ime::native_editor_probe' if args.editor else 'protected_ime::native_lifetime_probe'
            mod.write_text(mod.read_text() + '\npub(crate) fn run_native_ime_probe(){macos::' + entry + '();}\n')
            result['rustc'] = run(prefix + ['rustc', '-Vv'], copy, 15)
            result['build'] = run(prefix + ['cargo', 'build', '--offline', '--release', '--bin', 'congminal'], copy, 180)
            if result['build']['exit']:
                raise RuntimeError(result['build']['stderr'][-16000:])
            executable = copy / 'target/release/congminal'
            if args.window:
                contents = copy / 'CongminalInputProbe.app/Contents'
                (contents / 'MacOS').mkdir(parents=True)
                with (contents / 'Info.plist').open('wb') as output:
                    plistlib.dump({'CFBundleIdentifier':'org.congminal.input-probe.'+hashlib.sha256(str(copy).encode()).hexdigest()[:12],'CFBundleName':'Congminal Input Probe',
                                   'CFBundleExecutable':'congminal','CFBundlePackageType':'APPL','NSPrincipalClass':'NSApplication',
                                   'CFBundleVersion':'1','LSUIElement':False},output)
                shutil.copy2(executable,contents / 'MacOS/congminal')
                executable=contents / 'MacOS/congminal'
            if args.window:
                out_file, err_file, pid_file = copy/'native.stdout',copy/'native.stderr',copy/'native.pid'
                result['runtime'] = run(['open','-n','-W','--stdout',str(out_file),'--stderr',str(err_file),
                                         '--env','CONGMINAL_IME_PROBE_PID='+str(pid_file),str(contents.parent)], copy, 30)
                if result['runtime'].get('timed_out') and pid_file.exists():
                    pid=int(pid_file.read_text())
                    identity=subprocess.run(['ps','-p',str(pid),'-o','comm='],capture_output=True,text=True,timeout=5)
                    if identity.returncode==0 and Path(identity.stdout.strip()).resolve()==executable.resolve():
                        os.kill(pid,signal.SIGTERM)
                result['runtime']['native_stdout'] = out_file.read_text(errors='replace') if out_file.exists() else ''
                result['runtime']['native_stderr'] = err_file.read_text(errors='replace') if err_file.exists() else ''
                result['runtime']['stdout'] += result['runtime']['native_stdout']
                result['runtime']['stderr'] += result['runtime']['native_stderr']
            else:
                result['runtime'] = run([str(executable)], copy, 60)
            log = result['runtime']['stderr']
            if args.bridge:
                match = re.search(r'IME_BRIDGE plain_keys=(\d+) scoped_inputs=(\d+) coalesced_wakes=(\d+) late_deliveries=(\d+) stale_pastes=(\d+) default_rejections=(\d+) explicit_agreements=(\d+) blocked_waits=(\d+) blocked_cancels=(\d+) final_used=(\d+)',log)
                if result['runtime']['exit'] or not match:
                    raise RuntimeError(log[-16000:])
                result['system_focus_verified'] = False
                result['bridge'] = dict(zip(['plain_keys','scoped_inputs','coalesced_wakes','late_deliveries','stale_pastes','default_rejections','explicit_agreements','blocked_waits','blocked_cancels','final_used'],map(int,match.groups())))
                print(json.dumps(result['bridge']),flush=True)
                return
            if args.window:
                match = re.search(r'IME_WINDOW plain_keys=(\d+) commands=(\d+) commits=(\d+) focus_losses=(\d+) wakes=(\d+) final_used=(\d+) elapsed_ms=(\d+)',log)
                if result['runtime']['exit'] or not match:
                    raise RuntimeError(log[-16000:])
                result['system_focus_verified'] = not args.framework_focus
                result['window'] = dict(zip(['plain_keys','commands','commits','focus_losses','wakes','final_used','elapsed_ms'],map(int,match.groups())))
                print(json.dumps(result['window']),flush=True)
                return
            keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
            result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(
                r'IME_SAMPLE step=(\w+) live=(-?\d+) peak=(-?\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
            summary = re.search(r'IME_NATIVE init_us=(\d+) warm_rounds=(\d+) p50_us=(\d+) p95_us=(\d+) max_us=(\d+) scrub_checks=(\d+) scrub_failures=(\d+)', log)
            raw = re.search(r'IME_WARM_SAMPLES us=([\d,]+)', log)
            if summary:
                result['native'] = dict(zip(['init_us', 'warm_rounds', 'p50_us', 'p95_us', 'max_us', 'scrub_checks', 'scrub_failures'], map(int, summary.groups())))
                result['native']['sorted_samples_us'] = list(map(int, raw[1].strip(',').split(','))) if raw else []
            editor = re.search(r'IME_EDITOR consume_allocations=(\d+) consume_reallocations=(\d+) scrub_checks=(\d+) scrub_failures=(\d+)', log)
            if editor:
                result['editor'] = dict(zip(['consume_allocations','consume_reallocations','scrub_checks','scrub_failures'],map(int,editor.groups())))
                warm = re.search(r'IME_EDITOR_WARM events=(\d+) allocations=(\d+) reallocations=(\d+) p50_ns=(\d+) p95_ns=(\d+) p99_ns=(\d+) max_ns=(\d+)',log)
                raw_warm = re.search(r'IME_EDITOR_WARM_SAMPLES ns=([\d,]+)',log)
                if not warm or not raw_warm:
                    raise RuntimeError(log[-16000:])
                result['editor']['warm'] = dict(zip(['events','allocations','reallocations','p50_ns','p95_ns','p99_ns','max_ns'],map(int,warm.groups())))
                result['editor']['warm']['sorted_samples_ns'] = list(map(int,raw_warm[1].strip(',').split(',')))
            if result['runtime']['exit'] or len(result['samples']) != (6 if args.editor else 7) or not (editor if args.editor else summary):
                raise RuntimeError(log[-16000:])
            metric = 'editor' if args.editor else 'native'
            print(json.dumps({'samples': result['samples'], metric: result[metric]}), flush=True)
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
