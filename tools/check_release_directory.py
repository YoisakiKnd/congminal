#!/usr/bin/env python3
"""Current-source directory scan/result lifetime: Rust requested bytes versus admitted ledger, no production allocator change."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    sources = lambda: {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
                       for p in sorted((root / 'src').rglob('*.rs'))}
    fixtures = [root / 'tools/fixtures' / name for name in
                ['executor_allocation_meter.rs', 'release_directory_allocation_cases.rs']]
    result = dict(platform=platform.platform(), source_sha256=sources(),
        manifest_sha256={name: hashlib.sha256((root/name).read_bytes()).hexdigest() for name in ['Cargo.toml','Cargo.lock']},
        tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        fixtures_sha256={str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in fixtures},
        scope='Temporary source copy with epoch-tagged all-thread Rust requested-byte meter. Real Scan and Plan, 2000 files or bounded recursive fanout, actual page/table/path allocations, retained result credit and final zero. Shared Budget/TLS context initialized first. Conservative realloc overlap; excludes meter headers, allocator/C/OS internals, stacks and RSS. Fake metadata source: no SSH/helper/network/window or latency claim.')
    try:
        with tempfile.TemporaryDirectory(prefix='cg-release-directory-') as temporary:
            copy = Path(temporary)
            for name in ['src', 'assets', 'docs']:
                shutil.copytree(root / name, copy / name)
            for name in ['Cargo.toml', 'Cargo.lock']:
                shutil.copy2(root / name, copy / name)
            shutil.copy2(fixtures[0], copy / 'src/executor_allocation_meter.rs')
            main_file = copy / 'src/main.rs'
            main_file.write_text(main_file.read_text() + '\n#[cfg(test)] mod executor_allocation_meter;\n')
            plan = copy / 'src/sftp/plan.rs'
            plan.write_text(plan.read_text() + '\n' + fixtures[1].read_text())
            command = ['cargo','test','--offline','--release',
                       'sftp::plan::release_directory_probe::retained_directory_plan_and_scan_transients_are_funded',
                       '--','--exact','--nocapture','--test-threads=1']
            run = subprocess.run(command, cwd=copy, capture_output=True, text=True, timeout=240)
            result['run'] = dict(command=command, exit=run.returncode, stdout=run.stdout, stderr=run.stderr)
            result['samples'] = [dict(mode=m[0], **dict(zip(['files','directories','live','peak','used','admitted_peak'],map(int,m[1:]))))
                for m in re.findall(r'DIRECTORY_ALLOC mode=(\w+) files=(\d+) directories=(\d+) live=(\d+) peak=(\d+) used=(\d+) admitted_peak=(\d+) final=0', run.stderr)]
            if run.returncode or len(result['samples']) != 2:
                raise RuntimeError((run.stdout + run.stderr)[-10000:])
    finally:
        result['source_unchanged'] = result['source_sha256'] == sources()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')
    assert result['source_unchanged']


if __name__ == '__main__':
    main()
