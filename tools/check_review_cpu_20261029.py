#!/usr/bin/env python3
"""Epoch requested-byte evidence for native aggregate CPU/memory and sampler, in a temporary copy."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
from check_font_discovery import sources, build
from check_sftp_executor import run, identity


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--toolchain')
    args = parser.parse_args()
    if os.name != 'posix':
        raise SystemExit('This native allocation harness currently requires a Unix host.')
    root = Path(__file__).resolve().parents[1]
    fixtures = root / 'tools/fixtures/review_cpu_20261029.json'
    meter = root / 'tools/fixtures/executor_allocation_meter.rs'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    result = dict(source_sha256=sources(root), tool=identity(Path(__file__)),
        manifests={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']},
        fixtures={str(p.relative_to(root)): identity(p) for p in [fixtures, meter]},
        scope='All-thread epoch Rust requested bytes, temporary release test copy. Admitted parent budgets, '
        'caller cancellation token and Linux input fixtures precede epoch. Native first plus200 real scalar reads; '
        'million-core modeled aggregate header with infinite trailing rows and exact64KiB/overflow memory input; '
        'existing real SystemSource/disk/process sampler owned by its base reservation with last-release assertions. '
        'Zero native Rust allocations excludes libc/kernel allocations, thread stacks and RSS. The full sampler '
        'numbers are this host only: third-party disk and process metadata are not proved bounded on arbitrary hosts.')
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-fix-meter-') as temporary:
            copy = Path(temporary)
            for name in ['src', 'assets']:
                shutil.copytree(root/name, copy/name)
            for name in ['Cargo.toml', 'Cargo.lock']:
                shutil.copy2(root/name, copy/name)
            (copy/'docs').mkdir()
            shutil.copy2(root/'docs/configuration.md', copy/'docs/configuration.md')
            shutil.copy2(meter, copy/'src/executor_allocation_meter.rs')
            with (copy/'src/main.rs').open('a') as f:
                f.write('\n#[cfg(test)] mod executor_allocation_meter;\n')
            for name, content in json.loads(fixtures.read_text()).items():
                with (copy/name).open('a') as f:
                    f.write('\n'+content)
            result['rustc'] = run(prefix+['rustc', '-Vv'], copy, 15)
            binary, result['build'] = build(copy, prefix)
            result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], copy, 30)
            if result['sanity']['exit']:
                raise RuntimeError(str(result['sanity']))
            result['cases'] = {}
            for case in ['monitor::native::cpu_meter::native_reads', 'monitor::native::cpu_meter::bounded_linux_inputs', 'monitor::review_meter::sampler']:
                r = run([str(binary), case, '--exact', '--test-threads=1', '--nocapture'], copy, 60)
                result['cases'][case] = r
                print(json.dumps({'case': case, 'exit': r['exit']}), flush=True)
                if r['exit'] or not any(marker in r['stdout']+r['stderr'] for marker in ['CPU_ALLOC ', 'REVIEW_ALLOC ']):
                    raise RuntimeError(r['stdout']+r['stderr'])
            if sources(root) != result['source_sha256']:
                raise RuntimeError('Sources changed during measurement; retain this attempt and rerun.')
            result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
