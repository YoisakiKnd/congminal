#!/usr/bin/env python3
"""Epoch requested-byte evidence for the 2026-10-29 fixes, in a temporary copy."""
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
    fixtures = root / 'tools/fixtures/review_resources_20261029.json'
    meter = root / 'tools/fixtures/executor_allocation_meter.rs'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    result = dict(source_sha256=sources(root), tool=identity(Path(__file__)),
        manifests={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']},
        fixtures={str(p.relative_to(root)): identity(p) for p in [fixtures, meter]},
        scope='All-thread epoch-tagged Rust allocation requests in a temporary release test copy. '
        'Each parser/pipes/local-directory/sampler case runs alone. Input fixtures, parent budgets and singleton '
        'initialization precede the epoch. Checks retained requests against retained fees, peak requests '
        'against peak reservation and final release. Pipes include the admitted cancellation token used by LatestTask; '
        'streams close before delayed process exit to deterministically exercise lazy synchronization. '
        'Preexisting token state is outside the epoch; its128B lazy-sync allowance is checked separately. '
        'This is not native malloc/kernel/stack/RSS coverage, '
        'not a whole-program memory bound, and the meter is not used for timing.')
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
            for case in ['git::review_meter::parser', 'ssh::capture::review_meter::pipes',
                         'sftp::local_plan::review_meter::directory', 'monitor::review_meter::sampler']:
                r = run([str(binary), case, '--exact', '--test-threads=1', '--nocapture'], copy, 60)
                result['cases'][case] = r
                print(json.dumps({'case': case, 'exit': r['exit']}), flush=True)
                if r['exit'] or 'REVIEW_ALLOC ' not in r['stdout']+r['stderr']:
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
