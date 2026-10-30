#!/usr/bin/env python3
"""Epoch requested-byte evidence for Git metadata and bounded network reads, in a temporary copy."""
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
    fixtures = root / 'tools/fixtures/review_metadata_20261029.json'
    meter = root / 'tools/fixtures/executor_allocation_meter.rs'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    result = dict(source_sha256=sources(root), tool=identity(Path(__file__)),
        manifests={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']},
        fixtures={str(p.relative_to(root)): identity(p) for p in [fixtures, meter]},
        scope='All-thread epoch Rust requested bytes, temporary release test copy. Request inputs, parent budgets (including measured first-lock initialization), '
        'error-clone vector and network input precede epoch. Cases: five concurrent requests with SSH metadata, '
        'shared malformed stderr errors including denial and last clone, bounded network read including endless input. '
        'Checks allocation-before-denial, retained/peak admission, clone reuse and final zero. Excludes stack/native '
        'malloc/kernel/RSS and existing GUI/configuration ownership. Timing uses the unmodified System allocator separately.')
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
            for case in ['git::metadata_meter::requests', 'git::metadata_meter::errors', 'monitor::metadata_meter::network']:
                r = run([str(binary), case, '--exact', '--test-threads=1', '--nocapture'], copy, 60)
                result['cases'][case] = r
                print(json.dumps({'case': case, 'exit': r['exit']}), flush=True)
                if r['exit'] or 'METADATA_ALLOC ' not in r['stdout']+r['stderr']:
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
