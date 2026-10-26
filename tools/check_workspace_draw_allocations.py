#!/usr/bin/env python3
"""Attribute all-thread Rust requests during the path-form direct paint interval on macOS."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
from check_font_discovery import sources, build
from check_sftp_executor import identity, run, WORKSPACE_ALLOCATION


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rounds', type=int, default=10)
    parser.add_argument('--paints', type=int, default=20000)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    assert sys.platform == 'darwin' and args.rounds > 0 and args.paints > 0
    root = Path(__file__).resolve().parent.parent
    fixture_names = ['executor_allocation_meter.rs', 'sftp_workspace_allocation_cases.rs', 'allocation_trace.rs']
    result = dict(source_sha256=sources(root), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']},
                  tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  fixtures={f'tools/fixtures/{n}': identity(root/'tools/fixtures'/n) for n in fixture_names},
                  paints=args.paints,
                  scope='Diagnostic-only temporary System-wrapper allocation meter. Fixed64x32 native backtrace storage; no allocator logging heap. Trace requested number of path-form direct paints (default20000 rather than original200), tag pthread_self and symbolize with atos after each process exits. Native backtrace is diagnostic overhead, not performance/stack/RSS evidence. All thread counts retained. Full fixture assertions remain except path allocation equality is recorded instead to permit reporting. Per-round private cwd prevents reuse of fixture directory. No production edit or dependency.', runs=[])
    try:
        with tempfile.TemporaryDirectory(prefix='cg-draw-allocation-trace-') as temporary:
            p = Path(temporary)
            for name in ['src', 'assets']:
                shutil.copytree(root/name, p/name)
            for name in ['Cargo.toml', 'Cargo.lock']:
                shutil.copy2(root/name, p/name)
            main_file = p/'src/main.rs'
            main_file.write_text(main_file.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n#[cfg(test)] mod allocation_trace;\n')
            meter = (root/'tools/fixtures/executor_allocation_meter.rs').read_text()
            meter = meter.replace('let current=EPOCH.load', 'if new>0 {crate::allocation_trace::record(new);}let current=EPOCH.load')
            (p/'src/executor_allocation_meter.rs').write_text(meter)
            shutil.copy2(root/'tools/fixtures/allocation_trace.rs', p/'src/allocation_trace.rs')
            fixture = (root/'tools/fixtures/sftp_workspace_allocation_cases.rs').read_text()
            fixture = fixture.replace('let before_paths=meter::peek();', 'crate::allocation_trace::begin();let before_paths=meter::peek();')
            fixture = fixture.replace('for _ in 0..200{assert!(app.transfer_paths_paint());}', f'for _ in 0..{args.paints}{{assert!(app.transfer_paths_paint());}}')
            fixture = fixture.replace('let after_paths=meter::peek();assert_eq!(before_paths.allocations,after_paths.allocations);assert_eq!(before_paths.reallocations,after_paths.reallocations);', 'let after_paths=meter::peek();crate::allocation_trace::end();')
            fixture = fixture.replace('wait(||', 'eprintln!("TRACE_WAIT line={}",line!());wait(||')
            file = p/'src/workspace/remote_files.rs'
            file.write_text(file.read_text()+'\n'+fixture)
            binary, result['build'] = build(p, [])
            result['rustc'] = run(['rustc', '-Vv'], p, 15)
            result['calibration'] = run([str(binary), 'allocation_trace::tests::', '--nocapture', '--test-threads=1'], p, 15)
            assert result['calibration']['exit'] == 0, result['calibration']
            os.environ['RUST_BACKTRACE'] = '1'
            for i in range(args.rounds):
                cwd = p/f'round-{i}'
                cwd.mkdir()
                r = run([str(binary), WORKSPACE_ALLOCATION, '--exact', '--nocapture', '--test-threads=1'], cwd, 30)
                log = r['stdout']+r['stderr']
                result['runs'].append(r)
                base = re.search(r'TRACE_BASE base=(0x[\da-f]+) count=(\d+)', log)
                main = re.search(r'TRACE_MAIN thread=(\d+)', log)
                assert base and main, log[-12000:]
                events = []
                for size, thread, raw in re.findall(r'TRACE_EVENT size=(\d+) thread=(\d+) frames=([^\n]+)', log):
                    frames = raw.rstrip(',').split(',')
                    symbolized = run(['atos', '-o', str(binary), '-l', base[1]]+frames, p, 15)
                    assert symbolized['exit'] == 0
                    events.append(dict(size=int(size), thread=int(thread), on_paint_thread=thread == main[1], frames=frames, symbols=symbolized['stdout']))
                assert len(events) == int(base[2]), 'trace overflow'
                path = re.search(r'WORKSPACE_PATHS[^\n]*paint_allocations=(\d+) paint_reallocations=(\d+)', log)
                r.update(events=events, path_allocations=None if not path else int(path[1]), path_reallocations=None if not path else int(path[2]))
                print(json.dumps(dict(round=i, exit=r['exit'], events=len(events), paint_thread_events=sum(e['on_paint_thread'] for e in events))), flush=True)
                assert r['exit'] == 0, log[-12000:]
                assert path and int(path[1]) == len(events) and int(path[2]) == 0
            assert sources(root) == result['source_sha256']
            result['passed'] = True
    finally:
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
