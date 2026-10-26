#!/usr/bin/env python3
"""Plain paste admission/lifetime and frozen-source cost, no native delivery claim."""
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

CASE = 'platform::clipboard::paste_cost_probe::same_plain_clipboard_decode_and_bounded_paste_transform'
ADAPTERS = {
    'before': '''fn convert(input:&str,bracketed:bool,_pool:&Arc<Budget>)->String{let text=input.replace("\\r\\n","\\r").replace('\\n',"\\r");if bracketed{let text=text.replace("\\x1b[201~","");format!("\\x1b[200~{text}\\x1b[201~")}else{text}}
fn bytes(text:&String)->&[u8]{text.as_bytes()}
fn decode_utf8(input:&[u8],_pool:&Arc<Budget>)->String{ClipboardRead::default().utf8(input).unwrap()}
fn decode_utf16(input:&[u16],_pool:&Arc<Budget>)->String{ClipboardRead::default().utf16(input).unwrap()}
fn decoded(text:&String)->&str{text}
''',
    'after': '''fn convert(input:&str,bracketed:bool,pool:&Arc<Budget>)->Tracked<Vec<u8>>{paste_bytes(input,bracketed,pool).unwrap()}
fn bytes(text:&Tracked<Vec<u8>>)->&[u8]{&text.data}
fn decode_utf8(input:&[u8],pool:&Arc<Budget>)->PlainPaste{let mut request=PasteRequest::default().admit(pool);let text=request.utf8(input);let Event::ManagedPaste(reply)=request.finish(text)else{panic!()};reply}
fn decode_utf16(input:&[u16],pool:&Arc<Budget>)->PlainPaste{let mut request=PasteRequest::default().admit(pool);let text=request.utf16(input);let Event::ManagedPaste(reply)=request.finish(text)else{panic!()};reply}
fn decoded(text:&PlainPaste)->&str{text.text()}
''',
}


def project(source, target, fixture, adapter=''):
    for name in ['src', 'assets']:
        shutil.copytree(source/name, target/name)
    for name in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(source/name, target/name)
    file = target/'src/platform/clipboard.rs'
    file.write_text(file.read_text()+'\n'+fixture.replace('// PASTE_ADAPTER', adapter))


def allocation(root, args, result):
    result['scope'] = 'All-thread same-epoch conservative Rust request meter.64MiB owning budget and zero current Scope preinitialized, maximum8388596B ASCII input outside epoch. X11 bounded external request8388597B reserved but no actual Xlib API; real managed UTF8 decode/event and one transform to8388608B. External/result/transform drops and full-pool decode/external/transform, invalid strictUTF16 refusals with zero new allocations; final0. External request ticket is not Rust allocated bytes. Does not prove X11/macOS/Windows native clipboard delivery or allocation internals, PTY queue copy/global metadata/allocator/C/OS/stack/RSS/latency/long-run.'
    fixtures = [root/'tools/fixtures'/n for n in ['executor_allocation_meter.rs', 'paste_allocation_cases.rs']]
    result['allocation_fixtures'] = {str(p.relative_to(root)): identity(p) for p in fixtures}
    prefix = ['rustup', 'run', args.toolchain] if args.toolchain else []
    with tempfile.TemporaryDirectory(prefix='cg-paste-allocation-') as temporary:
        p = Path(temporary)
        project(root, p, fixtures[1].read_text())
        shutil.copy2(fixtures[0], p/'src/executor_allocation_meter.rs')
        main = p/'src/main.rs'
        main.write_text(main.read_text()+'\n#[cfg(test)] mod executor_allocation_meter;\n')
        result['rustc'] = run(prefix+['rustc', '-Vv'], p, 15)
        binary, result['build'] = build(p, prefix)
        result['sanity'] = run([str(binary), 'action::tests', '--test-threads=1'], p, 15)
        result['measurement'] = run([str(binary), 'platform::clipboard::paste_allocation_probe::', '--nocapture', '--test-threads=1'], p, 60)
        log = result['measurement']['stdout']+result['measurement']['stderr']
        keys = ['live', 'peak', 'used', 'budget_peak', 'allocations', 'reallocations']
        result['samples'] = [dict(step=m[0], **dict(zip(keys, map(int, m[1:])))) for m in re.findall(r'PASTE_ALLOC step=(\w+) live=(\d+) peak=(\d+) used=(\d+) budget_peak=(\d+) allocations=(\d+) reallocations=(\d+)', log)]
        assert result['sanity']['exit'] == result['measurement']['exit'] == 0, log[-14000:]
        assert len(result['samples']) == 10 and 'PASTE_FINAL denied_new_allocations=0 source_other_used=0 final=0 bytes=8388596' in log
        assert all(s['live'] <= s['used'] and s['peak'] <= s['budget_peak'] for s in result['samples'])


def cost(root, args, result, fixture):
    assert args.baseline is not None, '--cost requires frozen --baseline'
    result['scope'] = 'Identical fixture body/System allocator in frozen before/current sources, explicit embedded adapters execute original replace/replace/replace/format and ClipboardRead UTF8 versus real managed read/result and bounded transform. Two warmups/cohort, five alternating pairs.1000short mixed transformations,8each ~1MiB ASCII plain/bracketed, Unicode mixed and control-rich bracketed transforms,5000each UTF8/UTF16 managed decode600B,5000separate600B result releases and8separate maximum8388596B result releases; release includes managed clear plus credit return. Byte FNV equal; timers exclude hash/final drop but include transformation intermediates/ownership creation. time-lp whole-process CPU/RSS includes input setup/hash/drop. No PTY queue/system API/presentation/other-platform/long-run claim; no runtime helper/thread/package.'
    result['adapters'] = {k: dict(source=v, sha256=hashlib.sha256(v.encode()).hexdigest()) for k, v in ADAPTERS.items()}
    result['baseline_source_sha256'] = sources(args.baseline)
    result['baseline_manifests_sha256'] = {n: hashlib.sha256((args.baseline/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']}
    with tempfile.TemporaryDirectory(prefix='cg-paste-cost-') as temporary:
        result['rustc'] = run(['rustc', '-Vv'], Path(temporary), 15)
        builds, result['builds'] = {}, {}
        for cohort, source in [('before', args.baseline), ('after', root)]:
            p = Path(temporary)/cohort
            p.mkdir()
            project(source, p, fixture, ADAPTERS[cohort])
            binary, result['builds'][cohort] = build(p, [])
            builds[cohort] = p, binary

        def metric(samples):
            ordered = sorted(samples)
            n = len(samples)
            return dict(samples_ns=samples, total_ns=sum(samples), p50_ns=ordered[(n-1)//2], p95_ns=ordered[(n*95-1)//100], p99_ns=ordered[(n*99-1)//100], maximum_ns=ordered[-1])

        def trial(cohort):
            p, binary = builds[cohort]
            proc = subprocess.run(['/usr/bin/time', '-lp', str(binary), CASE, '--exact', '--ignored', '--nocapture', '--test-threads=1'], cwd=p, capture_output=True, text=True, timeout=90)
            log = proc.stdout+proc.stderr
            match = re.search(r'PASTE_COST signature=(\d+) decode=(\[[^\n]+\]) decode16=(\[[^\n]+\]) release=(\[[^\n]+\]) release_large=(\[[^\n]+\]) budget_peak=(\d+) final=0', log)
            assert proc.returncode == 0 and match is not None, log[-14000:]
            r = dict(cohort=cohort, exit=proc.returncode, log=log, signature=int(match[1]), budget_peak=int(match[6]), decode=metric(json.loads(match[2])), decode16=metric(json.loads(match[3])), release=metric(json.loads(match[4])), release_large=metric(json.loads(match[5])))
            for mode, raw, size in re.findall(r'PASTE_TIME mode=(\w+) times=(\[[^\n]+\]) bytes=(\d+)', log):
                r[mode] = dict(metric(json.loads(raw)), input_bytes=int(size))
            for key, count in [('short', 1000), ('ascii', 8), ('ascii_bracket', 8), ('mixed', 8), ('controls', 8), ('decode', 5000), ('decode16', 5000), ('release', 5000), ('release_large', 8)]:
                assert len(r[key]['samples_ns']) == count
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
                print(json.dumps(dict(cohort=cohort, pair=pair, ascii_ns=r['ascii']['total_ns'], mixed_ns=r['mixed']['total_ns'])), flush=True)
        assert len({r['signature'] for r in result['runs']+result['warmups']}) == 1
        result['summary'] = {}
        for key in ['short', 'ascii', 'ascii_bracket', 'mixed', 'controls', 'decode', 'decode16', 'release', 'release_large', 'rss', 'cpu']:
            values = []
            for pair in range(5):
                a, b = [next(r for r in result['runs'] if r['cohort'] == c and r['pair'] == pair) for c in ['before', 'after']]
                values.append(b['rss_bytes']-a['rss_bytes'] if key == 'rss' else b['user_seconds']+b['system_seconds']-a['user_seconds']-a['system_seconds'] if key == 'cpu' else 100*(b[key]['total_ns']/a[key]['total_ns']-1))
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
    fixture = root/'tools/fixtures/paste_cost_cases.rs'
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
