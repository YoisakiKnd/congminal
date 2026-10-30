#!/usr/bin/env python3
"""System allocator: constructor/copy/drop, shared errors and bounded network parsing."""
import argparse
import hashlib
import json
import re
import statistics
import tempfile
from pathlib import Path
from check_font_discovery import build, sources
from check_sftp_executor import identity, run
from check_review_cost_20261029 import project


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    fixture = root/'tools/fixtures/review_metadata_cost_20261029.rs'
    result = dict(source_sha256=sources(root), baseline_source_sha256=sources(args.baseline),
        manifests={n: hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},
        tool=identity(Path(__file__)), fixture=identity(fixture), builds={}, warmups=[], pairs=[],
        scope='Unmodified System allocator. Same synthetic SSH metadata, five request constructors/copies/drops '
        '(including input copy formerly done by GUI), same ASCII error text with one clone, same complete 64KiB '
        'network input. 500/5000/100 calls per batch, two warm batches then seven measurements per process; '
        'two warm trial pairs then five alternating pairs. No commands/network IO/GUI/RSS coverage. '
        'New invalid-UTF8 error byte cap is tested separately; ASCII text contents are asserted equal.')
    try:
        with tempfile.TemporaryDirectory(prefix='congminal-metadata-cost-') as temporary:
            projects = {}
            for name, source in [('before',args.baseline),('after',root)]:
                target = Path(temporary)/name
                target.mkdir()
                text = fixture.read_text().replace('/* WRITE_REQUEST */',
                    'WriteRequest::new(1, 1, path, WriteOp::Commit { message: message.clone() }, &limits, &pool).unwrap()' if name == 'before' else
                    'WriteRequest::from_source(1, 1, path, WriteSource::Commit { message: &message }, &limits, &pool).unwrap()')
                text = text.replace('/* ERROR */', 'failure_detail(error_input, Some(1))' if name == 'before' else 'failure_detail(error_input, Some(1), &pool).unwrap()')
                text = text.replace('/* ERROR_TEXT */','error.as_str()')
                project(source,target,text)
                with (target/'src/monitor/mod.rs').open('a') as f:
                    f.write('\n#[cfg(test)] pub(crate) fn metadata_cost_network(input: &[u8]) -> Option<(u64,u64)> {\n'+
                        ('use std::io::Read; let mut text = String::new(); std::io::Cursor::new(input).read_to_string(&mut text).ok()?; parse_net_dev(&text)' if name == 'before' else 'read_net_dev(std::io::Cursor::new(input))')+'\n}\n')
                binary,result['builds'][name] = build(target,[])
                projects[name]=(target,binary)
            def trial(name):
                target,binary=projects[name]
                r=run([str(binary),'git::tests::metadata_cost::same_inputs','--exact','--ignored','--nocapture','--test-threads=1'],target,60)
                if r['exit'] or 'METADATA_COST ' not in r['stdout']+r['stderr']:
                    raise RuntimeError(str(r))
                metrics={k:json.loads(v) for k,v in re.findall(r'(requests|errors|network)=(\[[^\n]+?\])',r['stdout']+r['stderr'])}
                if set(metrics) != {'requests','errors','network'} or any(len(v)!=7 for v in metrics.values()):
                    raise RuntimeError(str(r))
                return dict(run=r,metrics=metrics)
            for _ in range(2):
                result['warmups'].append({name:trial(name) for name in ['before','after']})
            for i in range(5):
                pair={name:trial(name) for name in (['before','after'] if i%2==0 else ['after','before'])}
                result['pairs'].append(pair)
            result['summary']={}
            for metric in ['requests','errors','network']:
                before=[statistics.median(p['before']['metrics'][metric]) for p in result['pairs']]
                after=[statistics.median(p['after']['metrics'][metric]) for p in result['pairs']]
                changes=[100*(a/b-1) for a,b in zip(after,before)]
                result['summary'][metric]=dict(before_median_ns=statistics.median(before),after_median_ns=statistics.median(after),
                    paired_median_percent=statistics.median(changes),pairs_percent=changes)
            if sources(root)!=result['source_sha256']:
                raise RuntimeError('Source changed during measurement')
            result['passed']=True
    except Exception as error:
        result['error']=str(error)
        raise
    finally:
        args.output.write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result['summary']))


if __name__=='__main__':
    main()
