#!/usr/bin/env python3
"""构建 Windows 原生捕获验收包；交叉构建成功不代表已在 Windows 运行。"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cc', default='x86_64-w64-mingw32-gcc')
    parser.add_argument('--target', default='x86_64-pc-windows-gnu')
    parser.add_argument('--output', type=Path, default=Path('target/windows-capture-validation'))
    args = parser.parse_args()
    if args.target != 'x86_64-pc-windows-gnu':
        parser.error('this fixture currently verifies the Windows x64 GNU ABI')
    root = Path(__file__).resolve().parent.parent
    folder = args.output.resolve()
    folder.mkdir(parents=True, exist_ok=True)
    source = root / 'tools/fixtures/windows_capture.c'
    result = {'scope': 'Cross-built native Windows test bundle. native_runtime_verified=false until run.ps1 succeeds on an actual Windows machine. Does not verify SSH/SFTP authentication, native crypto dependencies, GUI or RSS.', 'native_runtime_verified': False, 'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted((root / 'src').rglob('*.rs'))}, 'fixture_source_sha256': hashlib.sha256(source.read_bytes()).hexdigest(), 'commands': []}
    commands = [
        [args.cc, '-std=c11', '-Wall', '-Wextra', '-Werror', '-municode', '-static-libgcc', str(source), '-o', str(folder / 'fixture.exe')],
        ['cargo', 'test', '--offline', '--target', args.target, '--no-run', '--message-format=json'],
    ]
    try:
        for command in commands:
            start = time.monotonic()
            process = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=240)
            diagnostics = []
            if command[0] == 'cargo':
                for line in process.stdout.splitlines():
                    item = json.loads(line)
                    if item.get('reason') == 'compiler-message':
                        diagnostics.append(item.get('message', {}).get('rendered', ''))
            result['commands'].append({'command': command, 'exit': process.returncode, 'seconds': round(time.monotonic() - start, 3), 'stderr': process.stderr, 'diagnostics': diagnostics})
            if process.returncode:
                raise RuntimeError((''.join(diagnostics) + process.stderr)[-12000:])
        executable = None
        for line in process.stdout.splitlines():
            item = json.loads(line)
            if item.get('reason') == 'compiler-artifact' and item.get('target', {}).get('name') == 'congminal' and item.get('profile', {}).get('test') and item.get('executable'):
                executable = Path(item['executable'])
        if executable is None:
            raise RuntimeError('Windows test executable not found')
        shutil.copy2(executable, folder / 'tests.exe')
        shutil.copy2(root / 'tools/fixtures/run_windows_capture.ps1', folder / 'run.ps1')
        result['files'] = {p.name: {'bytes': p.stat().st_size, 'sha256': hashlib.sha256(p.read_bytes()).hexdigest()} for p in [folder / 'fixture.exe', folder / 'tests.exe', folder / 'run.ps1']}
        print(json.dumps({'bundle': str(folder), 'native_runtime_verified': False, 'files': {n: v['bytes'] for n, v in result['files'].items()}}))
    finally:
        (folder / 'manifest.json').write_text(json.dumps(result, indent=2) + '\n')

if __name__ == '__main__':
    main()
