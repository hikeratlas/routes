#!/usr/bin/env python3
# trail-candidates.py LIST.json ROUTES_OUT > entries.json
#
# Draft "trail" entries for fixtures/expect.json (routes#95) from a park's
# own list of its trails, e.g. its website's: LIST.json is
#   [{"park_id": "14107244", "name": "Granite Ridge Trail", "km": 2,
#     "source": "https://...", "why": "..."}, ...]
# for any number of parks, however it was made (by hand, a scraper, an LLM
# reading the page). ROUTES_OUT is a routes run over those parks (with
# trails.jsonl, routes#95 or later).
#
# Each name is matched to the OSM trail of the same name (ignoring case,
# punctuation, "the" and "trail"), else the closest similar name, which is
# set as `trail` with a note to review. A name with no OSM trail near it
# isn't drafted (a gap in the map, not in routes) and is listed on stderr,
# as are stated lengths far from the OSM trail's (a return trip, or a
# different trail). Review, then add the entries to expect.json.
import difflib, glob, json, os, re, sys

lst = json.load(open(sys.argv[1]))
root = sys.argv[2]


def norm(s):
    words = re.sub(r'[^0-9a-z]+', ' ', (s or '').lower()).split()
    return ' '.join(w for w in words if w not in ('the', 'trail'))


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
    out.append(e)

print(json.dumps(out, indent=2, ensure_ascii=False))
for n in notes:
    print(n, file=sys.stderr)
