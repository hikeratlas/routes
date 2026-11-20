#!/usr/bin/env python3
# curation-export.py [SITE]
#
# Writes the named regressions kept in the website's curation database
# (routes#252) into fixtures/: proposals/*.jsonl, expect/*.jsonl and
# parks.json, as the website's /dev/regressions page saved them. SITE
# defaults to https://www.hikeratlas.com (its /api/curation/files).
#
# Those files are edited on that page, not here: an export overwrites them.
# A file the database emptied is removed. A file here that the database
# doesn't have (added in a routes PR) is left, and named, so it can be
# added there too (the website's curation/import.mjs).
import json, os, re, sys, urllib.request

site = (sys.argv[1] if len(sys.argv) > 1 else 'https://www.hikeratlas.com').rstrip('/')
here = os.path.dirname(os.path.abspath(__file__))
root = os.path.dirname(here)
PATH = re.compile(r'^fixtures/(proposals/[a-z0-9-]+\.jsonl|expect/[a-z0-9-]+\.jsonl|parks\.json)$')

req = urllib.request.Request(f'{site}/api/curation/files', headers={'User-Agent': 'hikeratlas-routes/1.0'})
with urllib.request.urlopen(req, timeout=60) as r:
    files = json.load(r)['files']
if not files:
    sys.exit('the curation database has no files: nothing exported')

wrote = removed = 0
have = set()
for f in files:
    p = f['path']
    if not PATH.match(p):
        print(f'skipped {p}: not a curated file', file=sys.stderr)
        continue
    have.add(p)
    dest = os.path.join(root, p)
    if f['content'] == '':
        if os.path.exists(dest):
            os.remove(dest)
            removed += 1
        continue
    old = open(dest, encoding='utf-8').read() if os.path.exists(dest) else None
    if old != f['content']:
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, 'w', encoding='utf-8', newline='') as o:
            o.write(f['content'])
        wrote += 1

local = [f'fixtures/{d}/{n}' for d in ('proposals', 'expect') for n in sorted(os.listdir(os.path.join(here, d))) if n.endswith('.jsonl')]
missing = [p for p in local if p not in have]
for p in missing:
    print(f'not in the curation database (left as it is): {p}', file=sys.stderr)
print(f'{wrote} written, {removed} removed, {len(missing)} not in the database')
