#!/usr/bin/env python3
# trail-candidates.py LIST.json|PROPOSALS.jsonl... ROUTES_OUT [--write] > entries.jsonl
#
# Draft "trail" entries for the named regressions (fixtures/expect/, routes#95)
# from a park's own list of its trails, e.g. its website's: LIST.json is
#   [{"park_id": "14107244", "region": "Ontario", "name": "Granite Ridge Trail",
#     "km": 2, "source": "https://...", "why": "..."}, ...]
# for any number of parks, however it was made (by hand, a scraper, an LLM
# reading the page). ROUTES_OUT is a routes run over those parks with
# --trails (as fixtures/run.sh does).
#
# Prints the drafts as JSONL. With --write, appends each to its park's file,
# fixtures/expect/<region>-<park>.jsonl (made if need be; `region` is the
# park's province or state, needed for --write), leaving out names the file
# already has, so it can be run again as a list grows.
#
# Each name is matched to the OSM trail of the same name (ignoring case,
# punctuation, "the" and "trail"), else the closest similar name, which is
# set as `trail` with a note to review. A name with no OSM trail near it
# isn't drafted (a gap in the map, not in routes) and is listed on stderr,
# as are stated lengths far from the OSM trail's (a return trip, or a
# different trail). Review the drafts (git diff, with --write).
import difflib, glob, json, os, re, sys

from expectations import file_for, load, norm

args = sys.argv[1:]
write = '--write' in args
if write:
    args.remove('--write')
# A list (.json), or proposals files (fixtures/proposals/*.jsonl): their
# open proposals.
lst = []
for a in args[:-1]:
    if a.endswith('.jsonl'):
        lst += [p for p in (json.loads(x) for x in open(a, encoding='utf-8') if x.strip()) if p.get('status') == 'open']
    else:
        lst += json.load(open(a))
root = args[-1]


want = {str(t['park_id']) for t in lst}
trails, parks = {}, {}
for path in glob.glob(os.path.join(root, 'trails.jsonl')) + glob.glob(os.path.join(root, '*', 'trails.jsonl')):
    for line in open(path):
        m = re.search(r'"park_id":"([^"]*)"', line)
        if m and m.group(1) in want:
            t = json.loads(line)
            trails.setdefault(t['park_id'], {}).setdefault(norm(t['name']), t)
            parks[t['park_id']] = t['park']

out, notes = [], []
for t in lst:
    pid = str(t['park_id'])
    have = trails.get(pid)
    if have is None:
        notes.append(f"{t['name']}: park {pid} isn't in {root} (or has no named trails)")
        continue
    k = norm(t['name'])
    match = have.get(k)
    fuzzy = False
    if match is None:
        # "Crack" for "The Crack Trail", "Lake of the Woods" for
        # "Lake of The Woods Loop": a name containing the other, else the
        # closest by spelling.
        near = [n for n in have if k and (n in k or k in n)]
        near = near or difflib.get_close_matches(k, list(have), n=1, cutoff=0.75)
        if near:
            match, fuzzy = have[min(near, key=lambda n: abs(len(n) - len(k)))], True
    if match is None:
        notes.append(f"{t['name']} ({parks.get(pid, pid)}): no OSM trail of that name; not drafted")
        continue
    e = {'kind': 'trail', 'name': t['name'], 'park': t.get('park') or match['park'], 'park_id': pid,
         'issue': t.get('issue', 'routes#95'), 'source': t.get('source', ''), 'km': t['km']}
    if fuzzy or norm(match['name']) != k:
        e['trail'] = match['name']
        notes.append(f"{t['name']} ({e['park']}): matched to the OSM trail {match['name']!r}; check it's the same")
    ratio = t['km'] / match['km'] if match['km'] else 0
    if not 0.5 <= ratio <= 2.5:
        notes.append(f"{t['name']} ({e['park']}): stated {t['km']} km, OSM trail {match['km']} km; check it's the same trail")
    if match['inside'] < 0.5:
        notes.append(f"{t['name']} ({e['park']}): only {match['inside']:.0%} of it is inside the park")
    if t.get('why'):
        e['why'] = t['why']
    out.append((t.get('region'), e))

if write:
    have = {(x['file'], x.get('kind', 'dest'), x['name']) for x in load()}
    os.makedirs(os.path.dirname(file_for('x', 'y')), exist_ok=True)
for region, e in out:
    print(json.dumps(e, ensure_ascii=False))
    if not write:
        continue
    if not region:
        notes.append(f"{e['name']} ({e['park']}): no region in the list; not written")
        continue
    path = file_for(region, e['park'])
    stem = os.path.basename(path)[:-len('.jsonl')]
    if (stem, 'trail', e['name']) in have:
        notes.append(f"{e['name']} ({e['park']}): already in {stem}.jsonl; not written again")
        continue
    with open(path, 'a', encoding='utf-8') as f:
        f.write(json.dumps(e, ensure_ascii=False) + '\n')
    have.add((stem, 'trail', e['name']))
    notes.append(f"{e['name']} ({e['park']}): added to {stem}.jsonl")
if write:
    load()  # what was written must load
for n in notes:
    print(n, file=sys.stderr)
