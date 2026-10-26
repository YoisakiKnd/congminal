#!/usr/bin/env python3
"""字体拥有者元数据：冷分配/转交/重载计量，及冻结源码同负载对照。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import tempfile
from check_font_discovery import build, costs, project, sources
from check_sftp_executor import identity, run


def allocation(root, args, result):
    result['scope'] = 'Temporary-copy all-thread epoch-tagged Rust requested bytes with conservative realloc overlap. Valid minimal sfnt Bytes owner, budget/thread globals and 64KiB UTF8 fixture are preinitialized outside measurement. Creates32x64KiB owned warnings, 64KiB description, 32 fallback and32 emoji Loaded Face slots (synthetic maximum helper containers, not production installed fonts), real try_outline Box/caches, zero-copy take_warnings and later delivery lifetime, overlapping two valid outline Fonts and replacement/drop, then full-root rejection with zero new allocations and static refusal warning. No glyph/raster/mmap cache, filesystem/window, C/OS/allocator metadata/actual stacks/RSS, whole font pipeline or native UI claim.'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    fixtures = [root / 'tools/fixtures' / n for n in ['executor_allocation_meter.rs', 'font_metadata_allocation_cases.rs']]
    result['allocation_fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    with tempfile.TemporaryDirectory(prefix='cg-font-metadata-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p / 'src/executor_allocation_meter.rs')
        main = p / 'src/main.rs'
        main.write_text(main.read_text() + '\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix + ['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'font::metadata_allocation::owned_containers_text_handoff_reload_and_denial_are_funded', '--exact', '--nocapture', '--test-threads=1'], p, 30)
        log = result['measurement']['stdout'] + result['measurement']['stderr']
        keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'FONT_METADATA_ALLOC step=(\w+) live=(-?\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        if result['sanity']['exit'] or result['measurement']['exit'] or len(result['samples']) != 7 or 'FONT_METADATA_FINAL live=0 used=0 handoff_new_allocations=0 denied_new_allocations=0' not in log:
            raise RuntimeError(log[-14000:])
        if any(s['live'] < 0 or s['live'] > s['used'] or s['peak'] > s['budget_peak'] for s in result['samples']):
            raise RuntimeError('unfunded font metadata')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--allocation', action='store_true')
    mode.add_argument('--cost', action='store_true')
    parser.add_argument('--toolchain')
    parser.add_argument('--baseline', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.toolchain and not args.allocation:
        parser.error('--toolchain applies to --allocation')
    root = Path(__file__).resolve().parent.parent
    fixture = root / 'tools/fixtures/font_discovery_cost_cases.rs'
    helpers = [root / 'tools' / n for n in ['check_font_discovery.py', 'check_sftp_executor.py', 'check_ssh_transport.py']]
    result = dict(source_sha256=sources(root), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}, tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), helper_sha256={str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in helpers}, fixture={str(fixture.relative_to(root)): identity(fixture)})
    try:
        if args.cost:
            costs(root, args, result, fixture.read_text())
        else:
            allocation(root, args, result)
        if sources(root) != result['source_sha256']:
            raise RuntimeError('source changed during measurement')
        result['passed'] = True
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
