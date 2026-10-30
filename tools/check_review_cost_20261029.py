#!/usr/bin/env python3
"""System-allocator before/after comparison; fixed Git workload and monitor API mix."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
from check_font_discovery import build, sources
from check_sftp_executor import identity


def project(root, target, fixture):
    for name in ['src', 'assets']:
        shutil.copytree(root/name, target/name)
    for name in ['Cargo.toml', 'Cargo.lock']:
        shutil.copy2(root/name, target/name)
    (target/'docs').mkdir()
    shutil.copy2(root/'docs/configuration.md', target/'docs/configuration.md')
    with (target/'src/git/git_tests.rs').open('a') as stream:
        stream.write('\n'+fixture)


def trial(root, binary, mode):
    case = 'git::tests::review_cost::status_same_repository' if mode == 'git' else 'monitor::monitor_tests::monitor_sample_cost'
    command = ['/usr/bin/time', '-lp', str(binary), case, '--exact', '--ignored', '--nocapture', '--test-threads=1']
    run = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=120)
    log = run.stdout+run.stderr
    if run.returncode:
        raise RuntimeError(log)
    result = dict(command=command, exit=run.returncode, log=log)
    for label, pattern in [('rss', r'^\s*(\d+)\s+maximum resident set size$'),
                           ('user', r'^user\s+([\d.]+)$'), ('system', r'^sys\s+([\d.]+)$')]:
        result[label] = float(re.search(pattern, log, re.M)[1])
    if mode == 'git':
        match = re.search(r'REVIEW_COST git times=(\[[^\n]+?\]) retirement=(\[[^\n]+?\]) signature=(\d+)', log)
        result.update(times=json.loads(match[1]), retirement=json.loads(match[2]), signature=int(match[3]))
        result['median_ns'] = statistics.median(result['times'])
    else:
        result['metrics'] = {name: dict(n=int(n), median_us=float(median), p95_us=float(p95), maximum_us=float(maximum))
            for name, n, median, p95, maximum in re.findall(r'([\w-]+): n=(\d+) median=([\d.]+)us p95=([\d.]+)us max=([\d.]+)us', log)}
        result['rows'] = int(re.search(r'process rows=(\d+)', log)[1])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not Path('/usr/bin/time').exists() or os.uname().sysname != 'Darwin':
        raise SystemExit('This RSS/CPU harness currently requires macOS time -lp.')
    root = Path(__file__).resolve().parents[1]
    fixture = root/'tools/fixtures/review_git_cost_20261029.rs'
    result = dict(source_sha256=sources(root), baseline_source_sha256=sources(args.baseline),
        manifests={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml', 'Cargo.lock']},
        fixture=identity(fixture), tool=identity(Path(__file__)), scope='Unmodified System allocator. '
        'Same 500 modified files, two warm status calls then seven measured status/retirement intervals per process; '
        'existing monitor mix (5 cold/200 normal samples/5 process refreshes). Two warm trial processes per cohort, '
        'then five alternating pairs per mode on the same host. Live process populations can change and process '
        'enumeration is capped at512 only in the candidate; do not treat monitor timing as identical process records '
        'unless the host population fits. CPU/RSS include harness/setup/fixtures/children, not GUI latency or RSS bounds.',
        builds={}, warmups={}, pairs={})
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-fix-cost-') as temporary:
            projects = {}
            for name, source in [('before', args.baseline), ('after', root)]:
                target = Path(temporary)/name
                target.mkdir()
                project(source, target, fixture.read_text())
                binary, result['builds'][name] = build(target, [])
                projects[name] = (target, binary)
            for mode in ['git', 'monitor']:
                result['warmups'][mode] = []
                for _ in range(2):
                    for name in ['before', 'after']:
                        result['warmups'][mode].append(dict(cohort=name, **trial(*projects[name], mode)))
                result['pairs'][mode] = []
                for index in range(5):
                    pair = {}
                    for name in (['before', 'after'] if index % 2 == 0 else ['after', 'before']):
                        pair[name] = trial(*projects[name], mode)
                    if mode == 'git' and pair['before']['signature'] != pair['after']['signature']:
                        raise RuntimeError('Different Git status contents across cohorts')
                    result['pairs'][mode].append(pair)
                    print(json.dumps({'mode': mode, 'pair': index+1}), flush=True)
            result['summary'] = {}
            for mode, pairs in result['pairs'].items():
                metrics = ['median_ns'] if mode == 'git' else ['cold', 'sample-no-disk', 'sample-with-disk', 'processes']
                summary = {}
                for metric in metrics:
                    def value(cohort):
                        return cohort[metric] if mode == 'git' else cohort['metrics'][metric]['median_us']
                    changes = [100*(value(p['after'])/value(p['before'])-1) for p in pairs]
                    summary[metric] = dict(median_percent=statistics.median(changes), pairs_percent=changes)
                summary['rss_delta_bytes'] = [p['after']['rss']-p['before']['rss'] for p in pairs]
                summary['cpu_delta_seconds'] = [p['after']['user']+p['after']['system']-p['before']['user']-p['before']['system'] for p in pairs]
                result['summary'][mode] = summary
            if sources(root) != result['source_sha256']:
                raise RuntimeError('Source changed during comparison; retain this attempt and rerun.')
            result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        args.output.write_text(json.dumps(result, indent=2)+'\n')


if __name__ == '__main__':
    main()
