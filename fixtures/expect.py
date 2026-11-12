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
# hike: one is offered under that name (or `trail`, or one of `aka`), as
# its own name or where it goes (its destination;
# ignoring case, punctuation, "the" and "trail"); it is within `km_tolerance` (default 0.3)
# of `km`; and it walks at least 70% of the OSM trail `trail` (default:
# `name`), from routes' trails.jsonl (routes --trails). Outputs without
# trails.jsonl check the first two only.
#
# A hike made more than one way (from either of two lots) gives the others
# as `traces`, a list of traces: the entry passes on any one (routes#152).
#
# A `trail` entry with a `trace` (a rough line of the hike that's meant,
# [[lon, lat], ...]: a dozen points on the hike, spread along it) checks
# that instead of the OSM trail: the named hike of the right length must
# pass within `trace_within_m` (default 50) of at least `trace_cover`
# (default 0.7) of its points. For a trail OSM splits, forks or under-maps, where the walks check
# would compare against the wrong ways. It needs no trails.jsonl. The
# trace's first point is the trailhead, and isn't just one point among the
# rest: the same hike must start within `trace_start_m` (default 150) of it,
# so a hike from the wrong lot that walks over to the trail fails however
# much of the trace it covers (routes#102). `"trace_start_m": null` turns
# this off, for a trace that doesn't begin at a trailhead. Its last point is
# where the hike ends, likewise: the same hike must end within
# `trace_end_m` (default 150) of it (`null`: unchecked). A loop's trace ends
# where it starts; Killarney's La Cloche Silhouette ends 770 m from its start. With a trace,
# the length also passes within TRACE_KM (0.85-1.4) times the trace's own
# length: the trace says which hike is meant better than a park's rounded
# figure (Granite Ridge: the park says 2 km, the trail as drawn 2.5). A
# trace is a few points and cuts corners, so it's shorter than the hike;
# draw one in more detail if that's not enough. The report notes a length
# that passes by the trace only.
#
# A blessed entry is a `trail` entry with a trace made from a hike routes
# offered (the website's /dev/regressions: "Bless"), to keep it as it is:
# `blessed` says which ({key: the route's key, km, date, build}), and `km`
# is its length then; `name` is what the site calls it, its name or else
# "<dest> from <trailhead>". It's checked as any trail entry with a trace, but the
# length within 15% of `km` (`km_tolerance`), not by the trace: it's the
# hike's own length, not a park's rounded figure. When it fails, the report
# says what became of the route of that key: renamed, longer or shorter,
# moved, or gone.
#
# ROUTES_OUT is any routes output: one run's (parks/<id>.json, trails.jsonl,
# listed.jsonl) or the fixtures' (<fixture>/parks/<id>.json, ...). Each
# entry's park, by listed.jsonl (every park routes found in its area, and
# what became of it, routes#109):
#   - written: checked;
#   - a twin (the same park mapped twice, its routes in the other), a
#     small park inside a bigger one, or a piece of a park curated with it
#     (same-named pieces that touch): checked on that park's routes;
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
TRACE_WITHIN_M, TRACE_COVER, TRACE_START_M, TRACE_END_M = 50, 0.7, 150, 150
TRACE_KM = (0.85, 1.4)
BLESSED_TOLERANCE = 0.15

args = sys.argv[1:]
out_json = None
if '--json' in args:
    i = args.index('--json')
    out_json = args[i + 1]
    del args[i:i + 2]
root = args[0]
expects = load()
import proposals  # noqa: E402
proposals.load()  # a malformed proposal fails here too, as an entry would


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


def other(found):
    """The park whose document has a listed park's routes: its twin (the
    same park mapped twice), the bigger park it's in, or the park it's a
    piece of (routes curates same-named pieces that touch as one, #178)."""
    return found.get('twin') or found.get('in') or found.get('of')


def park_trails(park_id):
    """The park's named trails from trails.jsonl, by norm(name), or None if
    this output has no trails.jsonl. Only the parks with trail entries are
    parsed: a full build's file is large."""
    global _trails
    if not trail_files:
        return None
    if _trails is None:
        want = {e['park_id'] for e in expects if e.get('kind') == 'trail'}
        want |= {other(listed[i]) for i in want if other(listed.get(i, {}))}
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
    return problems, None, []


def near_line(p, line, within):
    """Whether point p is within `within` metres of the polyline."""
    kx = 111320 * math.cos(math.radians(p[1]))
    px, py = p[0] * kx, p[1] * 110570
    for a, b in zip(line, line[1:]):
        ax, ay, bx, by = a[0] * kx, a[1] * 110570, b[0] * kx, b[1] * 110570
        dx, dy = bx - ax, by - ay
        t = 0 if dx == dy == 0 else max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
        if math.hypot(px - ax - t * dx, py - ay - t * dy) <= within:
            return True
    return len(line) == 1 and metres(p, line[0]) <= within


def cover(trace, r, within):
    """The share of the trace's points within `within` metres of route r.
    Its points, not the straight lines between them: a dozen points on a
    winding trail cut its corners by more than 50 m."""
    line = r['route'].get('line') or []
    if not line:
        return 0.0
    return sum(near_line(p, line, within) for p in trace) / len(trace)


def start_off(trace, r):
    """How far route r starts (its line's first point, where the walk
    begins) from the trace's trailhead, its first point."""
    line = r['route'].get('line') or []
    return metres(trace[0], line[0]) if line else math.inf


def end_off(trace, r):
    """How far route r ends (its line's last point) from the trace's last."""
    line = r['route'].get('line') or []
    return metres(trace[-1], line[-1]) if line else math.inf


def check_trace(e, rs, named, sized, problems):
    """The start, end and follows checks, as {'start': ..., 'end': ...,
    'follows': ...} ('start' or 'end' left out where the entry turns it
    off)."""
    within, need = e.get('trace_within_m', TRACE_WITHIN_M), e.get('trace_cover', TRACE_COVER)
    start_m = e.get('trace_start_m', TRACE_START_M)
    end_m = e.get('trace_end_m', TRACE_END_M)
    got = [(cover(e['trace'], r, within), r) for r in (sized or named)]
    follows = any(c >= need for c, _ in got)
    checks = {}
    # The same hike must start and end where the trace does, and follow it:
    # of the ones that follow it (if any do, else any under the name), the
    # one that fails fewest of the two, then is nearest.
    pool = [r for c, r in got if c >= need] or [r for _, r in got]
    ends = [(start_off(e['trace'], r), end_off(e['trace'], r)) for r in pool]
    lim = (math.inf if start_m is None else start_m, math.inf if end_m is None else end_m)
    best = min(ends, key=lambda o: ((o[0] > lim[0]) + (o[1] > lim[1]), o[0] + o[1]), default=None)
    for k, i, m, what in (('start', 0, start_m, "starts {off:.0f} m from the trace's trailhead (its first point"),
                          ('end', 1, end_m, "ends {off:.0f} m from where the trace ends (its last point")):
        if m is None:
            continue
        checks[k] = None if best is None else best[i] <= m
        if checks[k] is False:
            problems.append("the named hike " + what.format(off=best[i]) + f"; needs {m} m)")
    checks['follows'] = follows
    if checks['follows'] or not rs:
        return checks
    # What does follow it, under any name: the clue to what went wrong.
    best = sorted(((cover(e['trace'], r, within), r) for r in rs), key=lambda x: -x[0])[:2]
    best = [f"{label(r)} ({c:.0%})" for c, r in best if c >= need / 2]
    what = (f"the named hike follows only {max(c for c, _ in got):.0%} of the trace"
            if got else "nothing offered under the name follows the trace")
    problems.append(f"{what} (needs {need:.0%} within {within} m)"
                    + (f"; it's followed most by {'; '.join(best)}" if best else "; nothing offered follows it"))
    return checks


def title(r):
    """What the site calls a route: its name, else "<dest> from <from>"
    (as routes' routes.geojsonl titles it). A blessed entry is named this,
    as most hikes have no name of their own."""
    return r.get('name') or f"{r.get('dest')} from {r.get('from')}"


def became(e, rs):
    """What became of a blessed entry's route: the one routes offers under
    its key now, compared with what was blessed."""
    b = e['blessed']
    r = next((r for r in rs if r.get('key') == b['key']), None)
    if r is None:
        return f"routes no longer offers the blessed route ({b['key']})"
    was, now = b.get('km', e['km']), r['route']['km']
    diffs = []
    if norm(title(r)) != norm(e['name']):
        diffs.append(f"called {title(r)}, not {e['name']}")
    if abs(now - was) > 0.05 * was:
        diffs.append(f"{now} km, was {was}")
    for what, off, m in (('starts', start_off, TRACE_START_M), ('ends', end_off, TRACE_END_M)):
        if off(e['trace'], r) > m:
            diffs.append(f"{what} {off(e['trace'], r):.0f} m from where it did")
    c = cover(e['trace'], r, e.get('trace_within_m', TRACE_WITHIN_M))
    if c < e.get('trace_cover', TRACE_COVER):
        diffs.append(f"follows {c:.0%} of its old line")
    return f"the blessed route ({b['key']}) is now {label(r)}" + (f": {'; '.join(diffs)}" if diffs else '')


def line_km(line):
    return sum(metres(a, b) for a, b in zip(line, line[1:])) / 1000


def check_trail(e, rs, trails):
    names = {norm(n) for n in [e['name'], e.get('trail'), *e.get('aka', [])] if n}
    blessed = e.get('blessed')
    tol = e.get('km_tolerance', BLESSED_TOLERANCE if blessed else 0.3)
    lo, hi = e['km'] * (1 - tol), e['km'] * (1 + tol)
    fits = lambda r: lo <= r['route']['km'] <= hi
    # Offered under the name: the hike's own (an OSM relation's), or where
    # it goes, as the site titles it ("Cascade Amphitheatre from ...").
    named = [r for r in rs if norm(title(r)) in names] if blessed else \
        [r for r in rs if norm(r.get('name')) in names or norm(r.get('dest')) in names]
    notes = []
    want = f"{lo:.1f}-{hi:.1f} km"
    if e.get('trace') and not blessed:
        # Or within TRACE_KM of the trace's own length.
        tk = line_km(e['trace'])
        tlo, thi = tk * TRACE_KM[0], tk * TRACE_KM[1]
        by_trace = lambda r: tlo <= r['route']['km'] <= thi
        sized = [r for r in named if fits(r) or by_trace(r)]
        want += f", or {tlo:.1f}-{thi:.1f} by its trace ({tk:.1f} km)"
        if sized and not any(fits(r) for r in sized):
            notes.append(f"the length passes by the trace only: {label(sized[0])}, the trace {tk:.1f} km;"
                         f" the park's {e['km']} km wants {lo:.1f}-{hi:.1f}")
    else:
        sized = [r for r in named if fits(r)]
    checks = {'name': bool(named), 'length': bool(sized), 'walks': None}
    problems = []
    if not named:
        problems.append(f"no hike named {e['name']}")
    elif not sized:
        problems.append(f"{e['name']} is {', '.join(label(r) for r in named)}; expected {want}")
    if e.get('trace'):
        checks = {'name': checks['name'], 'length': checks['length']} | check_trace(e, rs, named, sized, problems)
        if blessed and problems:
            problems.append(became(e, rs))
        return problems, checks, notes
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
    return problems, checks, notes


def check_traces(e, rs, trails):
    """check_trail against each of the entry's traces (`trace`, then
    `traces`: the same hike another way, e.g. from another lot, routes#152),
    keeping the best: passing, else the fewest failed checks, then the
    fewest problems."""
    ts = [e['trace']] + e.get('traces', []) if e.get('trace') else []
    if len(ts) < 2:
        return check_trail(e, rs, trails)
    tried = []
    for i, t in enumerate(ts):
        problems, checks, notes = check_trail(e | {'trace': t}, rs, trails)
        failed = sum(v is False for v in (checks or {}).values())
        tried.append(((bool(problems), failed, len(problems), i), (problems, checks, notes)))
    (_, _, _, i), (problems, checks, notes) = min(tried, key=lambda x: x[0])
    return problems, checks, notes + [f"against trace {i + 1} of {len(ts)}"]


results = []
for e in expects:
    kind = e.get('kind', 'dest')
    pid = e['park_id']
    rs = park_routes(pid)
    problems, checks, notes = [], None, []
    found = listed.get(pid)
    if rs is None and found and other(found):
        # The same park mapped twice, a small park inside a bigger one, or a
        # piece of a park curated with it: its routes are in the other's
        # document.
        pid = other(found)
        rs = park_routes(pid)
    if rs is not None:
        if kind == 'trail':
            problems, checks, notes = check_traces(e, rs, park_trails(pid))
        else:
            problems, checks, notes = check_dest(e, rs)
    elif found:
        why = {'under --min-area': f"it's under --min-area ({found['km2']} km²)",
               'over --max-tiles': "it's over --max-tiles",
               'no routes': 'routes found nothing to offer there'}.get(found['status'], found['status'])
        problems = [f"the park isn't curated: {why}"]
    status = 'skipped' if rs is None and not found else 'failing' if problems else 'ok'
    results.append({k: e[k] for k in ('name', 'park', 'park_id', 'file')}
                   | {'issue': e.get('issue') or 'blessed'}
                   | {'kind': kind, 'status': status, 'problems': problems}
                   | ({'checks': checks} if checks else {})
                   | ({'notes': notes} if notes else {}))

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
              + ''.join(f"\n  - {p}" for p in r['problems'])
              + ''.join(f"\n  - ℹ️ {n}" for n in r.get('notes', [])))
    print()
if skipped:
    print(f"➖ Not in this output: {', '.join(sorted(skipped))}")
if out_json:
    json.dump({'regressions': results, 'failing': len(failing), 'checked': len(checked)}, open(out_json, 'w'), indent=1)
