#!/usr/bin/env python3
# avoid.py BASE_DIR NEW_DIR [GUT_CHECK.jsonl] > summary.md
#
# The anti-oracle (routes#95): hikes a person wouldn't pick, from gut
# ratings of hikes routes offered (fixtures/gut-check.jsonl, made with the
# Trailhead Gut Check page: each rated hike as routes proposed it then, its
# line and its candidate record, and the rating). The oracle (oracle.py)
# counts known hikes found, which only rewards offering more; this counts
# what shouldn't be offered, in two run.sh runs over the fixtures:
#   - bizarre (verdict "bad"): still offered at all;
#   - better as an overnight trip (category "multi"): still offered as a day
#     hike;
#   - sensible (verdict "good"): still offered, the other way round, so a
#     change that drops bizarre hikes by dropping everything shows.
# Fewer of the first two and as many of the third is better. Meh hikes are
# kept but not counted.
#
# A rated hike is still offered when one of the park's offered hikes is the
# same walk: each passes within SAME_M of SAME_COVER of the other's length.
# Its name, start or score may have changed; the walk is what was rated.
# A hike that goes the same way and further, or stops short, is another
# hike.
import json, math, os, sys
from functools import lru_cache

SAME_M, SAME_COVER, STEP_M = 50, 0.8, 25
args = sys.argv[1:]
base_dir, new_dir = args[0], args[1]
path = args[2] if len(args) > 2 else os.path.join(os.path.dirname(os.path.abspath(__file__)), 'gut-check.jsonl')
lines = [json.loads(l) for l in open(path)]
rated = [r for r in lines[1:] if 'id' in r]


def xy(p, lat0):
    return (p[0] * 111320 * math.cos(math.radians(lat0)), p[1] * 110540)


def sample(line, lat0):
    """Points every STEP_M metres along a line, in local metres."""
    pts = [xy(p, lat0) for p in line]
    out = [pts[0]]
    for a, b in zip(pts, pts[1:]):
        d = math.dist(a, b)
        n = int(d // STEP_M)
        for i in range(1, n + 1):
            t = i * STEP_M / d
            out.append((a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t))
    out.append(pts[-1])
    return out


def grid(pts):
    g = {}
    for x, y in pts:
        g.setdefault((int(x // SAME_M), int(y // SAME_M)), []).append((x, y))
    return g


def covered(pts, g):
    """Share of pts within SAME_M of a point of the other line's grid."""
    n = 0
    for x, y in pts:
        cx, cy = int(x // SAME_M), int(y // SAME_M)
        if any(math.hypot(x - qx, y - qy) <= SAME_M
               for dx in (-1, 0, 1) for dy in (-1, 0, 1) for qx, qy in g.get((cx + dx, cy + dy), ())):
            n += 1
    return n / max(1, len(pts))


@lru_cache(maxsize=None)
def offered(d, fixture):
    """The fixture's offered hikes: (type, title, line)."""
    p = os.path.join(d, fixture, 'routes.geojsonl')
    if not os.path.exists(p):
        return None
    out = []
    for l in open(p):
        j = json.loads(l)
        g = j['geometry']
        line = g['coordinates'] if g['type'] == 'LineString' else [q for part in g['coordinates'] for q in part]
        if len(line) >= 2:
            out.append((j['properties'].get('type'), j['properties'].get('title'), line))
    return out


def match(d, r, types):
    """The offered hike that is the rated hike's walk, or None; False if
    the fixture wasn't run."""
    hs = offered(d, r['fixture'])
    if hs is None:
        return False
    lat0 = r['line'][0][1]
    mine = sample(r['line'], lat0)
    gm = grid(mine)
    for t, title, line in hs:
        if types and t not in types:
            continue
        theirs = sample(line, lat0)
        if covered(mine, grid(theirs)) >= SAME_COVER and covered(theirs, gm) >= SAME_COVER:
            return title
    return None


GROUPS = [
    ('Bizarre, still offered', lambda r: r['verdict'] == 'bad', None, 'fewer is better'),
    ('Better overnight, still a day hike', lambda r: r.get('category') == 'multi', {'day'}, 'fewer is better'),
    ('Sensible, still offered', lambda r: r['verdict'] == 'good' and r.get('category') != 'multi', None, 'more is better'),
]

print('## Hikes a person wouldn\'t pick (anti-oracle)\n')
print(f"Gut ratings of {len(rated)} hikes routes offered ({os.path.basename(path)}). "
      "A rated hike counts as offered when an offered hike is the same walk "
      f"(each within {SAME_M} m of {round(SAME_COVER * 100)}% of the other).\n")
print('| | rated | main | this change | |')
print('|---|---|---|---|---|')
details = []
for name, sel, types, better in GROUPS:
    rs = [r for r in rated if sel(r)]
    b = {r['id']: match(base_dir, r, types) for r in rs}
    n = {r['id']: match(new_dir, r, types) for r in rs}
    run = [r for r in rs if b[r['id']] is not False and n[r['id']] is not False]
    nb = sum(1 for r in run if b[r['id']])
    nn = sum(1 for r in run if n[r['id']])
    print(f"| {name} | {len(run)} | {nb} | {nn} | {better} |")
    gone = [r for r in run if b[r['id']] and not n[r['id']]]
    back = [r for r in run if n[r['id']] and not b[r['id']]]
    for what, rs2 in (('no longer offered', gone), ('offered again', back)):
        if rs2:
            items = '\n'.join(f"- {r['title']} ({r['park']}, {r['route']['km']} km)"
                              + (f": {r['note'].splitlines()[0][:140]}" if r.get('note') else '') for r in rs2)
            details.append(f"<details><summary>{name}: {len(rs2)} {what}</summary>\n\n{items}\n\n</details>\n")
print()
for d in details:
    print(d)
