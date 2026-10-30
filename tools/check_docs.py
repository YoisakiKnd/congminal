#!/usr/bin/env python3
"""Check local links in the maintained README and documentation; no network or packages."""
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit

root = Path(__file__).resolve().parents[1]
documents = [root / 'README.md', *sorted((root / 'docs').rglob('*.md'))]
failures = []
checked = 0
for document in documents:
    text = re.sub(r'^```.*?^```[^\n]*', '', document.read_text(), flags=re.M | re.S)
    for target in re.findall(r'!?\[[^\]]*\]\(([^\n)]+)\)', text):
        target = target.strip('<>')
        parsed = urlsplit(target)
        if parsed.scheme or not parsed.path:
            continue
        checked += 1
        path = document.parent / unquote(parsed.path)
        if not path.exists():
            failures.append(f'{document.relative_to(root)}: {target}')
if failures:
    raise SystemExit('Missing documentation targets:\n' + '\n'.join(failures))
print(f'DOC_LINKS_OK documents={len(documents)} local_links={checked}')
