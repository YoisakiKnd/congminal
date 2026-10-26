#!/usr/bin/env python3
"""临时源码注入探针：平台解码的分配前限量及 macOS 暂存释放前清零。"""
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
    parser.add_argument('--toolchain', help='already installed Rust toolchain; never installs')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    fixtures = [root / 'tools/fixtures' / name for name in [
        'auth_allocation_meter.rs', 'input_allocation_cases.rs', 'input_native_allocation_cases.rs', 'input_scoped_allocation_cases.rs']]
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    result = {
        'scope': 'Current-thread Rust GlobalAlloc requested bytes in temporary source. Inputs and native NSStrings are prepared before metering. Accepted UTF8/UTF16 decodes, rejected oversized/invalid input, legacy replacement and empty/NUL preservation. On macOS, actual NSString conversion and insertText callback replacement/rejection with initialized bytes inspected BEFORE dealloc. Excludes allocator/C/ObjC/OS memory, RSS, input event queue admission, key_events fallback, native window routing, latency and presentation. Also measures scoped clipboard request/response with an editor: original editor dropped while text is in flight, recreated view rejection, accepted/NUL/cancelled paths, budget lifetime and release-before-free scrubbing. No native window, actual clipboard provider, IME identity or end-to-end credential claim.',
        'platform': platform.platform(),
        'manifests_sha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in ['Cargo.toml', 'Cargo.lock']},
        'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))},
        'fixtures': {str(p.relative_to(root)): {'sha256': hashlib.sha256(p.read_bytes()).hexdigest(), 'source': p.read_text()} for p in fixtures},
        'tool_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    }
    version = subprocess.run(prefix + ['rustc','-Vv'],capture_output=True,text=True,timeout=15)
    result['rustc'] = {'exit':version.returncode,'stdout':version.stdout,'stderr':version.stderr}
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-input-allocation-') as temporary:
            copy = Path(temporary)
            for name in ['src', 'assets']:
                shutil.copytree(root / name, copy / name)
            for name in ['Cargo.toml', 'Cargo.lock']:
                shutil.copy2(root / name, copy / name)
            shutil.copy2(fixtures[0], copy / 'src/auth_allocation_meter.rs')
            injections = [('src/main.rs', '\n#[cfg(test)] mod auth_allocation_meter;\n'),
                          ('src/platform/mod.rs', fixtures[1].read_text()),
                          ('src/platform/macos.rs', fixtures[2].read_text()),
                          ('src/ssh/auth_editor.rs', fixtures[3].read_text())]
            for name, source in injections:
                path = copy / name
                path.write_text(path.read_text() + '\n' + source)
            command = prefix + ['cargo', 'test', '--offline', '--release', 'measured_input_', '--', '--nocapture', '--test-threads=1']
            env = dict(os.environ)
            env.pop('CONGMINAL_TRACE', None)
            process = subprocess.Popen(command, cwd=copy, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       text=True, encoding='utf-8', errors='replace', start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=180)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate(timeout=10)
                result['timed_out'] = True
            result.update(command=command, exit=process.returncode, stdout=stdout, stderr=stderr)
            result['samples'] = [dict(case=m[0], **dict(zip(['allocations', 'reallocations', 'peak', 'final_live'], map(int, m[1:]))))
                                 for m in re.findall(r'INPUT_ALLOC case=(\w+) allocations=(\d+) reallocations=(\d+) peak=(\d+) final_live=(-?\d+)', stderr)]
            native = re.search(r'INPUT_NATIVE allocations=(\d+) reallocations=(\d+) peak=(\d+) final_live=(-?\d+) scrub_checks=(\d+) scrub_failures=(\d+)', stderr)
            if native:
                result['native'] = dict(zip(['allocations', 'reallocations', 'peak', 'final_live', 'scrub_checks', 'scrub_failures'], map(int, native.groups())))
            keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
            result['scoped_samples'] = [dict(step=m[0], **dict(zip(keys,map(int,m[1:])))) for m in re.findall(
                r'SCOPED_INPUT_SAMPLE step=(\w+) live=(-?\d+) peak=(-?\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)',stderr)]
            scoped = re.search(r'SCOPED_INPUT checks=(\d+) failures=(\d+) decode_allocations=(\d+) decode_reallocations=(\d+) event_size=(\d+) request_size=(\d+)',stderr)
            if scoped:
                result['scoped'] = dict(zip(['scrub_checks','scrub_failures','decode_allocations','decode_reallocations','event_size','request_size'],map(int,scoped.groups())))
            if not scoped or len(result['scoped_samples']) != 9:
                raise RuntimeError((stdout + stderr)[-16000:])
            if process.returncode or len(result['samples']) != 11 or (platform.system() == 'Darwin' and not native):
                raise RuntimeError((stdout + stderr)[-16000:])
            print(json.dumps({key: result[key] for key in ['samples', 'native', 'scoped_samples', 'scoped'] if key in result}), flush=True)
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
