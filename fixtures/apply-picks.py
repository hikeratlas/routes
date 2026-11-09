#!/usr/bin/env python3
# apply-picks.py PICKS.json
#
# Writes the trace picker's picks (picker-data.py) into the named
# regressions: each pick's line, simplified to the points that keep its
# shape, becomes its entry's `trace` (see expect.py). PICKS.json is
#   [{"entry": "<file stem>:<line index>", "name": "<entry name>",
#     "mode": "candidate"|"drawn"|"none", "line": [[lon, lat], ...], "note": "..."}, ...]
# as the picker saves them. "none" picks are reported, not written. Entries
# are found by file and line, and a pick whose name no longer matches that
# line (the file changed since the picker was made) is refused.
#
# Needs: pip install shapely
import json, os, sys

from shapely.geometry import LineString

from expectations import DIR, load

MAX_POINTS = 20


def simplify(line):
    """A few points on the line that keep its shape: Douglas-Peucker with a
    tolerance doubled until MAX_POINTS or fewer remain (at least 10 m)."""
    g = LineString(line)
    tol = 10 / 111320  # degrees, roughly
    while True:
        s = list(g.simplify(tol).coords)
        if len(s) <= MAX_POINTS:
            return [[round(x, 5), round(y, 5)] for x, y in s]
        tol *= 2


picks = json.load(open(sys.argv[1]))
by_file = {}
for p in picks:
    stem, _, i = p['entry'].rpartition(':')
    by_file.setdefault(stem, []).append((int(i), p))
for stem, ps in by_file.items():
    path = os.path.join(DIR, stem + '.jsonl')
    lines = open(path, encoding='utf-8').read().splitlines()
    for i, p in ps:
        e = json.loads(lines[i]) if i < len(lines) else {}
        if p.get('name') and e.get('name') != p['name']:
            print(f"{stem}:{i}: picked for {p['name']!r}, but that line is now {e.get('name')!r}; "
                  "not written (remake the picker)", file=sys.stderr)
            continue
        if p['mode'] == 'none':
            print(f"{stem}:{i} {e['name']}: none of the candidates; not written"
                  + (f" ({p['note']})" if p.get('note') else ''), file=sys.stderr)
            continue
        e['trace'] = simplify(p['line'])
        if p.get('note'):
            e['trace_note'] = p['note']
        lines[i] = json.dumps(e, ensure_ascii=False)
        print(f"{stem}:{i} {e['name']}: trace of {len(e['trace'])} points ({p['mode']})", file=sys.stderr)
    open(path, 'w', encoding='utf-8').write('\n'.join(lines) + '\n')
load()  # what was written must load
