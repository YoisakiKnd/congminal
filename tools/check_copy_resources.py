#!/usr/bin/env python3
"""Outgoing clipboard: actual request lifetimes and same-work frozen-source cost."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
from check_font_discovery import build, sources
from check_sftp_executor import identity, run

CASE = 'platform::clipboard::cost_probe::same_output_copy_clipboard_payload_and_windows_encoding'
ADAPTERS = {
    'before': '''type Payload=String;
fn copied(text:&str,_pool:&Arc<Budget>)->Payload{text.to_owned()}
fn encoded(text:&Payload)->Vec<u16>{text.replace('\\n',"\\r\\n").encode_utf16().chain(Some(0)).collect()}
fn units(wide:&Vec<u16>)->&[u16]{wide.as_slice()}
''',
    'after': '''type Payload=CopyText;
fn copied(text:&str,pool:&Arc<Budget>)->Payload{CopyText::copy(text,pool).unwrap()}
fn encoded(text:&Payload)->WideCopy{WideCopy::new(text).unwrap()}
fn units(wide:&WideCopy)->&[u16]{wide.data.as_slice()}
''',
}


def project(source, target, fixture, adapter=''):
    for name in ['src', 'assets']:
        shutil.copytree(source/name, target/name)
    for name in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(source/name, target/name)
    file = target/'src/platform/clipboard.rs'
    file.write_text(file.read_text()+'\n'+fixture.replace('// COPY_ADAPTER', adapter))


def allocation(root, args, result):
    result['scope'] = 'Same-epoch all-thread Rust requested-byte meter, conservative realloc old/new overlap. Independent32MiB owner grows2MiB output text; pending request/platform-owned moves retain same credit; actual clear frees it. Max-newline2MiB text with real Windows-format Vec and prepaid equal GlobalAlloc request overlap (no Windows OS call on macOS). Full-pool/limit refusals allocate nothing. Separately actual 1.92MiB CopyOutput from populated terminal in its source budget, different current Scope, handoff after job/source drop, final root0. Source terminal/history/input strings/pools/context/locks preinitialized; terminal phase root peak includes preinitialized resources, independent payload phase proves growth peak. No allocator/C/OS/actual-stack/native clipboard delivery/incoming plain paste/full-root metadata/GUI latency claim.'
    fixtures = [root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs', 'copy_allocation_cases.rs']]
    result['allocation_fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-copy-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p/'src/executor_allocation_meter.rs')
        file = p/'src/main.rs'
        file.write_text(file.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix+['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'platform::clipboard::allocation_probe::', '--nocapture', '--test-threads=1'], p, 60)
        log = result['measurement']['stdout']+result['measurement']['stderr']
        keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
        result['samples'] = [dict(mode=m[0], step=m[1], **dict(zip(keys, map(int, m[2:])))) for m in re.findall(
            r'COPY_ALLOC mode=(\w+) step=(\w+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        assert result['sanity']['exit'] == result['measurement']['exit'] == 0, log[-14000:]
        assert len(result['samples']) == 10, log[-14000:]
        assert 'COPY_PAYLOAD_FINAL denied_new_allocations=0 owner_other_used=0 final=0' in log
        assert re.search(r'COPY_OUTPUT_FINAL bytes=1920000 steps=\d+ owner_other_used=0 root_final=0', log)
        assert all(s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] for s in result['samples'])


def cost(root, args, result, fixture):
    if args.baseline is None:
        raise RuntimeError('--cost requires frozen --baseline')
    result['scope'] = 'Same System-allocator fixture body in baseline/current source copies, explicit embedded adapters call original String::to_owned and original Windows replace/encode/collect expression versus actual managed CopyText and WideCopy. Actual original/new CopyOutput API unadapted. Two warmups/cohort then five alternating pairs.1000short64B output jobs,8large1.92MiB output jobs with per-step samples,5000direct700B UTF8 payload copies,8large Windows-format conversions. FNV bytes/UTF16 equal; timer stops before hashing/drop. Large aggregate includes step-loop/recording overhead; slice Vec preallocated and cannot grow. time-lp whole-process CPU/RSS includes terminal construction/parser/hash/drop, not API-only/native OS clipboard/GUI/incoming paste/platform/long-run acceptance. No new package/runtime helper/thread.'
    result['adapters'] = {k: dict(source=v, sha256=hashlib.sha256(v.encode()).hexdigest()) for k, v in ADAPTERS.items()}
    result['baseline_source_sha256'] = sources(args.baseline)
    result['baseline_manifests_sha256'] = {n: hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-copy-cost-') as temporary:
        result['rustc'] = run(['rustc', '-Vv'], Path(temporary), 15)
        builds, result['builds'] = {}, {}
        for cohort, source in [('before', args.baseline), ('after', root)]:
            p = Path(temporary)/cohort
            p.mkdir()
            project(source, p, fixture, ADAPTERS[cohort])
            binary, result['builds'][cohort] = build(p, [])
            builds[cohort] = p, binary

        def trial(cohort):
            p, binary = builds[cohort]
            proc = subprocess.run(['/usr/bin/time', '-lp', str(binary), CASE, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=p, capture_output=True, text=True, timeout=90)
            log = proc.stdout+proc.stderr
            match = re.search(r'COPY_COST signature=(\d+) short=(\[[^\n]+\]) large=(\[[^\n]+\]) slices=(\[[^\n]+\]) direct=(\[[^\n]+\]) encoding=(\[[^\n]+\]) budget_peak=(\d+) final=0', log)
            assert proc.returncode == 0 and match is not None, log[-14000:]
            r = dict(cohort=cohort, exit=proc.returncode, log=log, signature=int(match[1]), budget_peak=int(match[7]))
            for key, raw, count in [('short', match[2], 1000), ('large', match[3], 8), ('slices', match[4], None), ('direct', match[5], 5000), ('encoding', match[6], 8)]:
                samples = json.loads(raw)
                assert samples and (count is None or len(samples) == count)
                ordered = sorted(samples)
                n = len(samples)
                r[key] = dict(samples_ns=samples, total_ns=sum(samples), p50_ns=ordered[(n-1)//2], p95_ns=ordered[(n*95-1)//100], p99_ns=ordered[(n*99-1)//100], maximum_ns=ordered[-1])
            for key, pattern in [('user_seconds', r'^user\s+([\d.]+)$'), ('system_seconds', r'^sys\s+([\d.]+)$'), ('rss_bytes', r'^\s*(\d+)\s+maximum resident set size$')]:
                value = re.search(pattern, log, re.M)
                assert value is not None, log[-14000:]
                r[key] = int(value[1]) if key == 'rss_bytes' else float(value[1])
            return r

        result['warmups'] = [trial(c) for _ in range(2) for c in ['before', 'after']]
        result['runs'] = []
        for pair in range(5):
            for cohort in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                r = trial(cohort)
                r['pair'] = pair
                result['runs'].append(r)
                print(json.dumps(dict(cohort=cohort, pair=pair, large_ns=r['large']['total_ns'], direct_ns=r['direct']['total_ns'])), flush=True)
        assert len({r['signature'] for r in result['runs']+result['warmups']}) == 1
        result['summary'] = {}
        for key in ['short', 'large', 'direct', 'encoding', 'rss', 'cpu']:
            values = []
            for pair in range(5):
                a, b = [next(r for r in result['runs'] if r['cohort'] == c and r['pair'] == pair) for c in ['before', 'after']]
                values.append(b['rss_bytes']-a['rss_bytes'] if key == 'rss' else
                              b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if key == 'cpu' else
                              100*(b[key]['total_ns']/a[key]['total_ns']-1))
            result['summary'][key] = dict(median=statistics.median(values), minimum=min(values), maximum=max(values), pairs=values)


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
        parser.error('--toolchain applies to allocation')
    root = Path(__file__).resolve().parent.parent
    fixture = root/'tools/fixtures/copy_cost_cases.rs'
    result = dict(source_sha256=sources(root), manifests_sha256={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}, tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), fixtures={str(fixture.relative_to(root)): identity(fixture)})
    try:
        allocation(root, args, result) if args.allocation else cost(root, args, result, fixture.read_text())
        assert sources(root) == result['source_sha256']
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
