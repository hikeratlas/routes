#!/usr/bin/env python3
# expect.py ROUTES_OUT [--json OUT.json] > summary.md
#
# Named regressions (fixtures/expect/<region>-<park>.jsonl, one file per
# park, one entry per line; see expectations.py): well-known routes that are
# very noticeable when they're missing or misnamed. Two kinds:
#
# "dest" (the default): in park `park_id`, a route to `dest` that starts
# within `start_within_m` of `start` and is at most `max_km` long, named
# `name`; and no other route in the park named `name`.
#
# "trail": a trail the park itself lists (e.g. on its website, `source`),
# by its `name` there and its length there, `km`. Three checks, on the same
# hike: one is offered under that name (or `trail`, or one of `aka`;
# ignoring case, punctuation, "the" and "trail"); it is within `km_tolerance` (default 0.3)
# of `km`; and it walks at least 70% of the OSM trail `trail` (default:
# `name`), from routes' trails.jsonl (routes --trails). Outputs without
# trails.jsonl check the first two only.
#
# ROUTES_OUT is any routes output: one run's (parks/<id>.json, trails.jsonl,
# listed.jsonl) or the fixtures' (<fixture>/parks/<id>.json, ...). Each
# entry's park, by listed.jsonl (every park routes found in its area, and
# what became of it, routes#109):
#   - written: checked;
#   - a twin (the same park mapped twice, its routes in the other), or a
#     small park inside a bigger one: checked on that park's routes;
#   - found but not curated (under --min-area, over --max-tiles, no routes):
#     failing, as nothing is offered there;
#   - not found: skipped, as the park isn't in this output's area (Killarney
#     in a Nova Scotia build). Outputs without listed.jsonl skip every park
#     they haven't written. It reports and never fails on what it
# finds: some of these aren't fixed yet, and a build shouldn't stop over one.
# It does fail (exit 1) on a malformed or misnamed expectations file. --json
# writes the results for basemap's build progress check: {"regressions":
# [{name, park, park_id, file, issue, kind, status ("ok", "failing" or
# "skipped"), problems, checks}], "failing": n, "checked": n}.
import glob, json, math, os, re, sys
from functools import lru_cache

from expectations import load, norm

WALKS = 0.7

args = sys.argv[1:]
out_json = None
if '--json' in args:
    i = args.index('--json')
    out_json = args[i + 1]
    del args[i:i + 2]
root = args[0]
expects = load()


def metres(a, b):
    dx = (b[0] - a[0]) * 111320 * math.cos(math.radians(a[1]))
    return math.hypot(dx, (b[1] - a[1]) * 110570)


@lru_cache(maxsize=None)
def park_routes(park_id):
    """The park's offered routes (day hikes, multi-day routes, base camps'
    hikes; not their approaches), each with the `from` and `dest` that
    trails.jsonl names it by, or None if this output doesn't have the park."""
    paths = glob.glob(os.path.join(root, 'parks', f'{park_id}.json')) + \
        glob.glob(os.path.join(root, '*', 'parks', f'{park_id}.json'))
    if not paths:
        return None
    doc = json.load(open(paths[0]))
    rs = [r | {'type': 'day'} for r in doc.get('day_hikes', [])]
    rs += [r | {'type': 'multi-day', 'dest': r.get('to')} for r in doc.get('point_to_points', [])]
    for b in doc.get('base_camps', []):
        rs += [r | {'type': 'from-camp', 'from': b.get('camp')} for r in b.get('hikes', [])]
    return rs


def read_listed():
    """listed.jsonl's entries by park id: what became of each park routes
    found. A park found by several fixtures counts as written if any wrote it."""
    out = {}
    for path in glob.glob(os.path.join(root, 'listed.jsonl')) + glob.glob(os.path.join(root, '*', 'listed.jsonl')):
        for line in open(path):
            if line.strip():
                v = json.loads(line)
                if v['id'] not in out or v['status'] == 'written':
                    out[v['id']] = v
    return out


listed = read_listed()
trail_files = glob.glob(os.path.join(root, 'trails.jsonl')) + glob.glob(os.path.join(root, '*', 'trails.jsonl'))
_trails = None


def park_trails(park_id):
    """The park's named trails from trails.jsonl, by norm(name), or None if
    this output has no trails.jsonl. Only the parks with trail entries are
    parsed: a full build's file is large."""
    global _trails
    if not trail_files:
        return None
    if _trails is None:
        want = {e['park_id'] for e in expects if e.get('kind') == 'trail'}
        want |= {listed[i].get('twin') or listed[i]['in'] for i in want
                 if listed.get(i, {}).get('twin') or listed.get(i, {}).get('in')}
        _trails = {}
        for path in trail_files:
            for line in open(path):
                m = re.search(r'"park_id":"([^"]*)"', line)
                if m and m.group(1) in want:
                    t = json.loads(line)
                    _trails.setdefault(t['park_id'], {}).setdefault(norm(t['name']), t)
    return _trails.get(park_id, {})


def label(r):
    return f"{r.get('name') or r.get('from')} to {r.get('dest')} ({r['route']['km']} km)"


def same(a, b):
    """A trails.jsonl route summary and a park document's route."""
    return (a.get('type'), a.get('from'), a.get('dest'), a.get('km')) == \
        (b.get('type'), b.get('from'), b.get('dest'), b['route']['km'])


def check_dest(e, rs):
    problems = []
    want = [r for r in rs if r.get('dest') == e['dest']
            and r.get('start') and metres(r['start'], e['start']) <= e['start_within_m']
            and r['route']['km'] <= e['max_km']]
    if not want:
        to = [label(r) for r in rs if r.get('dest') == e['dest']]
        problems.append(f"no route to {e['dest']} from its trailhead" + (f"; only {'; '.join(to)}" if to else ''))
    elif not any(r.get('name') == e['name'] for r in want):
        problems.append(f"the route to {e['dest']} isn't named {e['name']}")
    wrong = [label(r) for r in rs if r.get('name') == e['name'] and r not in want]
    if wrong:
        problems.append(f"{e['name']} names another route: {'; '.join(wrong)}")
    return problems, None


def check_trail(e, rs, trails):
    names = {norm(n) for n in [e['name'], e.get('trail'), *e.get('aka', [])] if n}
    tol = e.get('km_tolerance', 0.3)
    lo, hi = e['km'] * (1 - tol), e['km'] * (1 + tol)
    fits = lambda r: lo <= r['route']['km'] <= hi
    named = [r for r in rs if norm(r.get('name')) in names]
    sized = [r for r in named if fits(r)]
    checks = {'name': bool(named), 'length': bool(sized), 'walks': None}
    problems = []
    want = f"{e['name']} ({e['km']} km, {lo:.1f}-{hi:.1f})"
    if not named:
        problems.append(f"no hike named {e['name']}")
    elif not sized:
        problems.append(f"{e['name']} is {', '.join(label(r) for r in named)}; expected {lo:.1f}-{hi:.1f} km")
    osm = e.get('trail', e['name'])
    t = None if trails is None else trails.get(norm(osm))
    if trails is not None and t is None:
        checks['walks'] = False
        problems.append(f"no OSM trail named {osm} in or near the park")
    elif t is not None:
        walked = [w for w in t['routes'] if w['coverage'] >= WALKS]
        checks['walks'] = any(same(w, r) for w in walked for r in (sized or named))
        if not checks['walks']:
            def desc(w):
                return f"{w.get('name') or (w.get('dest') or '') + ' from ' + (w.get('from') or '')} ({w['km']} km, {w['coverage']:.0%})"
            best = [w for w in t['routes'] if lo <= w['km'] <= hi][:2] or t['routes'][:2]
            what = 'the named hike walks too little of it' if named else 'nothing offered under the name walks it'
            problems.append(f"{what} ({osm}, {t['km']} km in OSM); " +
                            (f"it's walked most by {'; '.join(desc(w) for w in best)}" if best else 'no hike walks it'))
    return problems, checks


results = []
for e in expects:
    kind = e.get('kind', 'dest')
    pid = e['park_id']
    rs = park_routes(pid)
    problems, checks = [], None
    found = listed.get(pid)
    if rs is None and found and (found.get('twin') or found.get('in')):
        # The same park mapped twice, or a small park inside a bigger one:
        # its routes are in the other's document.
        pid = found.get('twin') or found['in']
        rs = park_routes(pid)
    if rs is not None:
        if kind == 'trail':
            problems, checks = check_trail(e, rs, park_trails(pid))
        else:
            problems, checks = check_dest(e, rs)
    elif found:
        why = {'under --min-area': f"it's under --min-area ({found['km2']} km²)",
               'over --max-tiles': "it's over --max-tiles",
               'no routes': 'routes found nothing to offer there'}.get(found['status'], found['status'])
        problems = [f"the park isn't curated: {why}"]
    status = 'skipped' if rs is None and not found else 'failing' if problems else 'ok'
    results.append({k: e[k] for k in ('name', 'park', 'park_id', 'file', 'issue')}
                   | {'kind': kind, 'status': status, 'problems': problems}
                   | ({'checks': checks} if checks else {}))

checked = [r for r in results if r['status'] != 'skipped']
failing = [r for r in checked if r['status'] == 'failing']
mark = {'ok': '✅', 'failing': '❌', 'skipped': '➖'}
tick = {True: '✓', False: '✗', None: '–'}
by_file = {}
for r in results:
    by_file.setdefault(r['file'], []).append(r)
skipped = [f for f, rs in by_file.items() if all(r['status'] == 'skipped' for r in rs)]
print(f"### Named regressions: {len(failing)} of {len(checked)} failing"
      f" ({len(by_file) - len(skipped)} of {len(by_file)} parks in this output)\n")
# Parks with failures first, then by file name.
order = sorted((f for f in by_file if f not in skipped),
               key=lambda f: (not any(r['status'] == 'failing' for r in by_file[f]), f))
for f in order:
    rs = by_file[f]
    n_fail = sum(r['status'] == 'failing' for r in rs)
    print(f"#### {rs[0]['park']} ({f}): {n_fail} of {len(rs)} failing\n")
    for r in sorted(rs, key=lambda r: (r['status'] != 'failing', r['kind'], r['name'])):
        detail = ''
        if r.get('checks'):
            detail = ' (' + ', '.join(f"{k} {tick[v]}" for k, v in r['checks'].items()) + ')'
        print(f"- {mark[r['status']]} **{r['name']}** ({r['issue']}): {r['status']}{detail}"
              + ''.join(f"\n  - {p}" for p in r['problems']))
    print()
if skipped:
    print(f"➖ Not in this output: {', '.join(sorted(skipped))}")
if out_json:
    json.dump({'regressions': results, 'failing': len(failing), 'checked': len(checked)}, open(out_json, 'w'), indent=1)
