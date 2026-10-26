#!/usr/bin/env python3
"""Verify the published handoff snapshot; source changes require new evidence, not edited old hashes."""
import hashlib
import json
import re
from pathlib import Path


def digest(path):
    data = path.read_bytes()
    if path.suffix.lower() in {'.rs', '.py', '.c', '.h', '.md', '.json', '.toml', '.yml', '.yaml', '.sh', '.ps1', '.txt'} or path.name in {'.gitattributes', '.gitignore'}:
        data = data.replace(b'\r\n', b'\n')
    return hashlib.sha256(data).hexdigest()


def main():
    root = Path(__file__).resolve().parent.parent
    snapshot = json.loads((root / 'docs/handoff/source-snapshot.json').read_text())
    failures = []
    for name, expected in snapshot['files_sha256'].items():
        path = root / name
        if not path.is_file() or digest(path) != expected:
            failures.append(name)
    if failures:
        raise SystemExit('Snapshot differs (expected after new work): ' + ', '.join(failures))
    validation = snapshot['validation']
    for key in ['audit', 'build']:
        path = root / validation[key]
        if digest(path) != validation[key + '_sha256']:
            raise SystemExit('Validation report differs: ' + str(path))
    audit = json.loads((root / validation['audit']).read_text())
    source = {n: d for n, d in snapshot['files_sha256'].items() if n.startswith('src/') and n.endswith('.rs')}
    assert audit['passed'] and audit['source_sha256'] == source
    assert snapshot['roadmap_snapshot']['pending'] == 50 and snapshot['roadmap_snapshot']['checked'] == 45
    assert digest(root / snapshot['roadmap_snapshot']['source']) == snapshot['roadmap_snapshot']['sha256']
    document = root / 'docs/DEEPSEEK_HANDOFF.md'
    missing = []
    for target in re.findall(r'\]\(([^)]+)\)', document.read_text()):
        if re.match(r'\w+://', target) or target.startswith('#'):
            continue
        path = document.parent / target.split('#', 1)[0]
        if not path.exists():
            missing.append(target)
    if missing:
        raise SystemExit('Missing handoff links: ' + ', '.join(missing))
    print(json.dumps(dict(passed=True, files=len(snapshot['files_sha256']), sources=len(source),
                          handoff='docs/DEEPSEEK_HANDOFF.md', functional_plan_complete=False)))


if __name__ == '__main__':
    main()
