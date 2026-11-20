#!/usr/bin/env python3
# curation-export.py [SITE]
#
# Writes the named regressions kept in the website's curation database
# (routes#252), a row per trail, into fixtures/: each park's official list
# as proposals/<stem>.jsonl and its official trails as expect/<stem>.jsonl,
# in their order, each row as the website's /dev/regressions page saved it.
# A park with official trails that isn't a fixture yet is added to
# parks.json, so CI runs it (fixtures/README.md). SITE defaults to
# https://www.hikeratlas.com (its /api/curation).
#
# Only parks changed since the last export are read and written: since the
# revision in fixtures/curation.json, which it then moves on (to spare the
# database's rows read; no file there, or revision 0: every park).
#
# Those files are edited on that page, not here: an export overwrites a
# changed park's, and removes its file when it has no rows of that kind. A
# file here for a park the database doesn't have (added in a routes PR) is
# left, and named, so it can be added there too (the website's
# curation/import.mjs).
import json, os, re, sys, unicodedata, urllib.request

site = (sys.argv[1] if len(sys.argv) > 1 else 'https://www.hikeratlas.com').rstrip('/')
here = os.path.dirname(os.path.abspath(__file__))
STEM = re.compile(r'^[a-z0-9-]+$')
cursor_path = os.path.join(here, 'curation.json')
since = json.load(open(cursor_path))['revision'] if os.path.exists(cursor_path) else 0


def get(query):
    req = urllib.request.Request(f'{site}/api/curation{query}', headers={'User-Agent': 'hikeratlas-routes/1.0'})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


def slug(s):
    s = unicodedata.normalize('NFKD', s)
    return re.sub(r'[^a-z0-9]+', '-', ''.join(c for c in s if not unicodedata.combining(c)).lower()).strip('-')


def write(path, rows):
    """rows: the park's live rows, in order (the API gives them so)."""
    text = ''.join(r['data'] + '\n' for r in rows) if rows else None
    old = open(path, encoding='utf-8').read() if os.path.exists(path) else None
    if text == old:
        return 0
    if text is None:
        os.remove(path)
    else:
        with open(path, 'w', encoding='utf-8', newline='') as o:
            o.write(text)
    return 1


db = get(f'?since={since}')
changed = 0
for p in db['parks']:
    stem = p['stem']
    if not STEM.match(stem):
        print(f"skipped {p['name']}: odd stem {stem!r}", file=sys.stderr)
        continue
    pid = p['park_id']
    changed += write(os.path.join(here, 'proposals', f'{stem}.jsonl'), [r for r in db['listed'] if r['park_id'] == pid])
    changed += write(os.path.join(here, 'expect', f'{stem}.jsonl'), [r for r in db['trails'] if r['park_id'] == pid])

# A changed park with official trails is a fixture too, keyed by its short
# name.
fx_path = os.path.join(here, 'parks.json')
fixtures = json.load(open(fx_path, encoding='utf-8'))
added = 0
for p in db['parks']:
    if not any(r['park_id'] == p['park_id'] for r in db['trails']) or not p.get('bbox'):
        continue
    if any(str(f.get('id', '')) == p['park_id'] or (not f.get('id') and f['park'] == p['name']) for f in fixtures):
        continue
    key = slug(re.sub(r' (National|Provincial|Interprovincial|State) Park.*$| Park$', '', p['name']))
    if any(f['key'] == key for f in fixtures):
        key = f"{key}-{p['park_id']}"
    fixtures.append({'key': key, 'id': p['park_id'], 'park': p['name'], 'why': 'quality: park-listed trails as named regressions (routes#95)', 'bbox': p['bbox']})
    added += 1
if added:
    with open(fx_path, 'w', encoding='utf-8') as o:
        o.write(json.dumps(fixtures, indent=2, ensure_ascii=False) + '\n')

if db['head'] != since:
    with open(cursor_path, 'w', encoding='utf-8') as o:
        o.write(json.dumps({'revision': db['head']}) + '\n')

# Files here the database has no park for (the park list: a row a park).
stems = {p['stem'] for p in get('')['parks']}
missing = [f'fixtures/{d}/{n}' for d in ('proposals', 'expect') for n in sorted(os.listdir(os.path.join(here, d)))
           if n.endswith('.jsonl') and n[:-len('.jsonl')] not in stems]
for m in missing:
    print(f'not in the curation database (left as it is): {m}', file=sys.stderr)
print(f"revisions {since} to {db['head']}: {len(db['parks'])} parks changed, {changed} files written or removed, "
      f'{added} fixtures added, {len(missing)} files not in the database')
