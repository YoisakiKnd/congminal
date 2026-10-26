#!/usr/bin/env python3
"""在临时源码副本注入 Rust 分配探针；不改实际工作区或默认程序分配器。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import tempfile

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--toolchain', help='optional already installed Rust toolchain; never installs one')
    parser.add_argument('--ipc', action='store_true', help='measure Unix worker endpoint with two simultaneous displayed prompts')
    parser.add_argument('--editor', action='store_true', help='measure secret editor and verify initialized event storage before deallocation')
    parser.add_argument('--controller', action='store_true', help='measure modal controller lifecycle, independently retained paste and scoped input/software drawing')
    parser.add_argument('--view', action='store_true', help='measure prompt index overlap and warm masked software drawing')
    args = parser.parse_args()
    if sum([args.ipc,args.editor,args.view,args.controller])>1: parser.error('choose one of --ipc, --editor, --view or --controller')
    root = Path(__file__).resolve().parent.parent
    fixtures = [root / 'tools/fixtures/auth_allocation_meter.rs', root / ('tools/fixtures/auth_controller_allocation_cases.rs' if args.controller else 'tools/fixtures/auth_view_allocation_cases.rs' if args.view else 'tools/fixtures/auth_editor_allocation_cases.rs' if args.editor else 'tools/fixtures/auth_ipc_allocation_cases.rs' if args.ipc else 'tools/fixtures/auth_allocation_cases.rs')]
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    version = subprocess.run(prefix + ['rustc', '-Vv'], capture_output=True, text=True, encoding='utf-8', errors='replace', timeout=15)
    result = {'scope': 'Exact isolated test in a temporary source copy. Current-thread Rust GlobalAlloc requested bytes, conservative old/new realloc overlap; excludes allocator overhead, C/OS internal allocations, thread stacks, RSS and preinitialized shared budget contexts. Same-argument 16KiB escaped prompt, request/decoded/display overlap, 1023-byte UI/decoded answers; 10000 near-limit UTF-8 edit pairs. Separate real Condvar timeout covers lazy wait allocation. No SSH/helper/UI/network is invoked.', 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'fixtures': {str(p.relative_to(root)): {'sha256': hashlib.sha256(p.read_bytes()).hexdigest(), 'source': p.read_text(encoding='utf-8')} for p in fixtures}}
    result.update(platform=platform.platform(), machine=platform.machine(), rustc={'exit': version.returncode, 'stdout': version.stdout, 'stderr': version.stderr}, tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    if args.ipc:
        result['scope'] = 'Temporary-source current-thread GlobalAlloc requests for private Unix ticket/socket worker: partial 16KiB input frame, two simultaneous 64KiB escaped displays, 1023-byte queued/sent/decoded answer and cleanup. 10000 waiting ticks allocation check is a microdiagnostic, not idle CPU or GUI latency. Excludes allocator/C/OS internals, RSS, thread stacks and preinitialized shared budget/launch-lock contexts; no SSH/helper process or GUI.'
    if args.editor:
        result['scope'] = 'Temporary-source current-thread GlobalAlloc requests for editor with 16KiB raw/64KiB escaped prompt, two fixed 1023-byte secret buffers, admitted event strings, UTF-8 editing, stale/invalid events and submit/drop. Registered initialized event/answer bytes inspected BEFORE allocator deallocation, never after free. 10000 scalar edit pairs allocation microdiagnostic. Native platform admission, event routing, GUI/IME, allocator/C/OS internals, RSS, stacks and preinitialized shared budget context are excluded.'
    if args.view:
        result['scope'] = 'Temporary-source current-thread GlobalAlloc requests for maximum escaped prompt, editor, cooperative row index, old/new overlap and coalesced resize. Separate 200 warm edit/sync/software-render events with preinitialized canvas/font caches; nearest-rank percentiles, not native input/presentation latency. Excludes allocator/C/OS, RSS, stacks, native IME/clipboard/events, cold glyphs and preinitialized shared budget contexts. No SSH/helper/network.'
    if args.controller:
        result['scope'] = 'Temporary-source actual Controller/Panel/Editor/Backend: maximum escaped prompt plus captured connection identity from a preinitialized immutable maximum-field profile, old/new index overlap, scoped 1023-byte independently retained clipboard across blur/recreated view, stale paste refusal, maximum input and submitted answer; three release-before-free checks. Separate 200 warm scoped input plus software drawing events, 100 decoded one-byte Strings and 100 payload-free deletion commands. Prewarmed renderer/font/canvas and shared Budget/thread/clock contexts excluded. Current-thread Rust allocation requests only, not C/OS/RSS/stacks, native Cocoa/window/IME or presentation latency; no SSH/helper/network.'
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-auth-allocation-') as temporary:
            copy = Path(temporary)
            for name in ['src', 'assets']:
                shutil.copytree(root / name, copy / name)
            for name in ['Cargo.toml', 'Cargo.lock']:
                shutil.copy2(root / name, copy / name)
            shutil.copy2(fixtures[0], copy / 'src/auth_allocation_meter.rs')
            main_file = copy / 'src/main.rs'
            main_file.write_text(main_file.read_text(encoding='utf-8') + '\n#[cfg(test)] mod auth_allocation_meter;\n', encoding='utf-8')
            auth_file = copy / 'src/ssh/auth.rs'
            auth_file.write_text(auth_file.read_text(encoding='utf-8') + '\n' + fixtures[1].read_text(encoding='utf-8'), encoding='utf-8')
            case = 'ssh::auth::allocation_controller_probe::measured_controller_lifecycle_and_warm_events_are_bounded' if args.controller else 'ssh::auth::allocation_view_probe::measured_viewport_overlap_and_warm_masked_render_are_bounded' if args.view else 'ssh::auth::allocation_editor_probe::measured_editor_allocations_and_event_scrubbing_are_bounded' if args.editor else 'ssh::auth::allocation_ipc_probe::measured_private_ipc_allocations_and_idle_ticks_are_bounded' if args.ipc else 'ssh::auth::allocation_probe::measured_authentication_allocations_are_funded_and_edits_do_not_allocate'
            command = prefix + ['cargo', 'test', '--offline', '--release', case, '--', '--exact', '--nocapture', '--test-threads=1']
            process = subprocess.Popen(command, cwd=copy, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding='utf-8', errors='replace', start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=180)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate(timeout=10)
                result['timed_out'] = True
            log = stdout + stderr
            result.update(command=command, exit=process.returncode, log=log)
            keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
            result['samples'] = [dict(label=m[0], step=m[1], **dict(zip(keys, map(int, m[2:])))) for m in re.findall(r'AUTH_ALLOC_SAMPLE label=(\w+) step=(\w+) live=(-?\d+) peak=(-?\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
            edits = re.search(r'AUTH_ALLOC_EDITS pairs=(\d+) allocations=(\d+) reallocations=(\d+) elapsed_us=(\d+)', log)
            if edits:
                result['edits'] = dict(zip(['pairs', 'allocations', 'reallocations', 'elapsed_us'], map(int, edits.groups())))
            idle = re.search(r'AUTH_IPC_IDLE ticks=(\d+) allocations=(\d+) reallocations=(\d+) elapsed_us=(\d+)', log)
            if idle:
                result['idle_ticks'] = dict(zip(['ticks', 'allocations', 'reallocations', 'elapsed_us'], map(int, idle.groups())))
            editor = re.search(r'AUTH_EDITOR_EVENTS pairs=(\d+) allocations=(\d+) reallocations=(\d+) elapsed_us=(\d+) scrub_checks=(\d+) scrub_failures=(\d+)', log)
            if editor:
                result['editor_events'] = dict(zip(['pairs', 'allocations', 'reallocations', 'elapsed_us', 'scrub_checks', 'scrub_failures'], map(int, editor.groups())))
            work = re.search(r'AUTH_VIEW_WORK slices=(\d+) max_chars=(\d+) layout_allocations=(\d+) resize_allocations=(\d+)',log)
            render = re.search(r'AUTH_VIEW_RENDER frames=(\d+) allocations=(\d+) reallocations=(\d+) p50_us=(\d+) p95_us=(\d+) p99_us=(\d+) max_us=(\d+)',log)
            if work and render:
                result['view_work'] = dict(zip(['slices','max_chars','layout_allocations','resize_allocations'],map(int,work.groups())))
                result['view_render'] = dict(zip(['frames','allocations','reallocations','p50_us','p95_us','p99_us','max_us'],map(int,render.groups())))
                raw = re.search(r'AUTH_VIEW_RENDER_SAMPLES us=([\d,]+)',log)
                result['view_render']['sorted_samples_us'] = list(map(int,raw[1].strip(',').split(','))) if raw else []
            controller = re.search(r'AUTH_CONTROLLER_EVENTS events=(\d+) allocations=(\d+) reallocations=(\d+) p50_us=(\d+) p95_us=(\d+) p99_us=(\d+) max_us=(\d+)',log)
            clear = re.search(r'AUTH_CONTROLLER_CLEAR scrub_checks=(\d+) scrub_failures=(\d+)',log)
            controller_input = re.search(r'AUTH_CONTROLLER_INPUT p50_ns=(\d+) p95_ns=(\d+) p99_ns=(\d+) max_ns=(\d+)',log)
            if controller and clear and controller_input:
                result['controller_events'] = dict(zip(['events','allocations','reallocations','p50_us','p95_us','p99_us','max_us'],map(int,controller.groups())))
                result['controller_events'].update(dict(zip(['scrub_checks','scrub_failures'],map(int,clear.groups()))))
                result['controller_input'] = dict(zip(['p50_ns','p95_ns','p99_ns','max_ns'],map(int,controller_input.groups())))
                for metric,pattern in [('controller_events',r'AUTH_CONTROLLER_EVENT_SAMPLES us=([\d,]+)'),('controller_input',r'AUTH_CONTROLLER_INPUT_SAMPLES ns=([\d,]+)')]:
                    raw = re.search(pattern,log)
                    result[metric]['sorted_samples'] = list(map(int,raw[1].strip(',').split(','))) if raw else []
            if process.returncode or len(result['samples']) != (10 if args.controller else 7 if args.editor or args.view else 8 if args.ipc else 10) or not ((controller and clear and controller_input) if args.controller else (work and render) if args.view else editor if args.editor else idle if args.ipc else edits):
                raise RuntimeError(log[-16000:])
            metric = 'controller_events' if args.controller else 'view_render' if args.view else 'editor_events' if args.editor else 'idle_ticks' if args.ipc else 'edits'
            print(json.dumps({'samples': result['samples'], metric: result[metric]}), flush=True)
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')

if __name__ == '__main__':
    main()
