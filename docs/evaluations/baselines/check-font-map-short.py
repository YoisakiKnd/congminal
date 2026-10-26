#!/usr/bin/env python3
"""字体文件映射/控制块/固定弱缓存/错误：真实加载请求计量与冻结源码成本对照。"""
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
    result['scope'] = 'Temporary-copy all-thread epoch Rust requested-byte meter with conservative realloc overlap. Budget/thread context, input filesystem fixture/config/FaceInfo preinitialized. Cold real map/hardlink cache hit/slice final release; direct real owned-read fallback (does not force OS mmap failure),16 retained malformed-font errors and fixed writer, real native default Fonts::load and A/Chinese/snowman/ZWJ materialization, final requested/managed/mapped/count zero, full-budget refusal zero new allocations. Preinitialized input metadata stays charged until dropped. Fixed cache slots have static storage, not heap. Excludes meter headers/allocator/C/OS/actual stack/RSS, budget context global metadata, native UI/kernel deadline, other-platform runtime/long-run acceptance.'
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    fixtures = [root / 'tools/fixtures' / n for n in ['executor_allocation_meter.rs', 'font_map_allocation_cases.rs']]
    result['allocation_fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    with tempfile.TemporaryDirectory(prefix='cg-font-map-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p / 'src/executor_allocation_meter.rs')
        main = p / 'src/main.rs'
        main.write_text(main.read_text() + '\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix + ['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'font::map_allocation::cold_real_mapping_owned_fallback_fixed_errors_and_fonts_release_all_credit', '--exact', '--nocapture', '--test-threads=1'], p, 30)
        log = result['measurement']['stdout'] + result['measurement']['stderr']
        keys = ['live', 'peak', 'used', 'budget_peak', 'mapped', 'mappings', 'allocations', 'reallocations']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'FONT_MAP_ALLOC step=(\w+) live=(-?\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) mapped=(\d+) mappings=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        if result['sanity']['exit'] or result['measurement']['exit'] or len(result['samples']) != 10 or 'FONT_MAP_FINAL live=0 used=0 mapped=0 mappings=0 denied_new_allocations=0' not in log:
            raise RuntimeError(log[-14000:])
        if any(s['live'] < 0 or s['live'] > s['used'] or s['peak'] > s['budget_peak'] for s in result['samples']):
            raise RuntimeError('unfunded font map/loaded glyphs')


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
