#!/usr/bin/env python3
# expect.py ROUTES_OUT [--json OUT.json] > summary.md
#
# Named regressions (fixtures/expect.json): well-known routes that are very
# noticeable when they're missing or misnamed. Two kinds:
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
# ROUTES_OUT is any routes output: one run's (parks/<id>.json, trails.jsonl)
# or the fixtures' (<fixture>/parks/<id>.json, <fixture>/trails.jsonl); a
# park it doesn't have is skipped. It reports and never fails: some of these
# aren't fixed yet, and a build shouldn't stop over one. --json writes the
# results for basemap's build progress check: {"regressions": [{name, park,
# park_id, issue, kind, status ("ok", "failing" or "skipped"), problems,
# checks}], "failing": n, "checked": n}.
import glob, json, math, os, re, sys

WALKS = 0.7

args = sys.argv[1:]
out_json = None
if '--json' in args:
    i = args.index('--json')
    out_json = args[i + 1]
    del args[i:i + 2]
root = args[0]
here = os.path.dirname(os.path.abspath(__file__))
expects = json.load(open(os.path.join(here, 'expect.json')))


def metres(a, b):
    dx = (b[0] - a[0]) * 111320 * math.cos(math.radians(a[1]))
    return math.hypot(dx, (b[1] - a[1]) * 110570)


def norm(s):
    """A name to compare: 'The "Crack" Trail' and 'the crack' are the same."""
    words = re.sub(r'[^0-9a-z]+', ' ', (s or '').lower()).split()
    return ' '.join(w for w in words if w not in ('the', 'trail'))


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


trail_files = glob.glob(os.path.join(root, 'trails.jsonl')) + glob.glob(os.path.join(root, '*', 'trails.jsonl'))
_trails = None


def park_trails(park_id):
    """The park's named trails from trails.jsonl, by norm(name), or None if
    this output has no trails.jsonl. Only the parks expect.json names are
    parsed: a full build's file is large."""
    global _trails
    if not trail_files:
        return None
    if _trails is None:
        want = {e['park_id'] for e in expects if e.get('kind') == 'trail'}
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
    rs = park_routes(e['park_id'])
    problems, checks = [], None
    if rs is not None:
        if kind == 'trail':
            problems, checks = check_trail(e, rs, park_trails(e['park_id']))
        else:
            problems, checks = check_dest(e, rs)
    status = 'skipped' if rs is None else 'failing' if problems else 'ok'
    results.append({k: e[k] for k in ('name', 'park', 'park_id', 'issue')}
                   | {'kind': kind, 'status': status, 'problems': problems}
                   | ({'checks': checks} if checks else {}))

checked = [r for r in results if r['status'] != 'skipped']
failing = [r for r in checked if r['status'] == 'failing']
print(f"### Named regressions: {len(failing)} of {len(checked)} failing\n")
mark = {'ok': '✅', 'failing': '❌', 'skipped': '➖'}
tick = {True: '✓', False: '✗', None: '–'}
for r in sorted(results, key=lambda r: (r['park'], r['kind'], r['name'])):
    detail = ''
    if r.get('checks'):
        detail = ' (' + ', '.join(f"{k} {tick[v]}" for k, v in r['checks'].items()) + ')'
    print(f"- {mark[r['status']]} **{r['name']}** ({r['park']}, {r['issue']}): {r['status']}{detail}"
          + ''.join(f"\n  - {p}" for p in r['problems']))
if out_json:
    json.dump({'regressions': results, 'failing': len(failing), 'checked': len(checked)}, open(out_json, 'w'), indent=1)
