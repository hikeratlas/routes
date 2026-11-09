#!/usr/bin/env python3
# expect.py ROUTES_OUT [--json OUT.json] > summary.md
#
# Named regressions (fixtures/expect.json): well-known routes that are very
# noticeable when they're missing or misnamed. Each expects, in park
# `park_id`, a route to `dest` that starts within `start_within_m` of
# `start` and is at most `max_km` long, named `name`; and no other route in
# the park named `name`.
#
# ROUTES_OUT is any routes output: one run's (parks/<id>.json) or the
# fixtures' (<fixture>/parks/<id>.json); a park it doesn't have is skipped.
# It reports and never fails: some of these aren't fixed yet, and a build
# shouldn't stop over one. --json writes the results for basemap's build
# progress check: {"regressions": [{name, park, park_id, issue, status
# ("ok", "failing" or "skipped"), problems}], "failing": n, "checked": n}.
import glob, json, math, os, sys

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


def park_routes(park_id):
    """The park's chosen routes (day hikes, point-to-points, base camps'
    hikes), or None if this output doesn't have the park."""
    paths = glob.glob(os.path.join(root, 'parks', f'{park_id}.json')) + \
        glob.glob(os.path.join(root, '*', 'parks', f'{park_id}.json'))
    if not paths:
        return None
    doc = json.load(open(paths[0]))
    rs = list(doc.get('day_hikes', [])) + list(doc.get('point_to_points', []))
    for b in doc.get('base_camps', []):
        rs += b.get('hikes', [])
    return rs


def label(r):
    return f"{r.get('name') or r.get('from')} to {r.get('dest')} ({r['route']['km']} km)"


results = []
for e in expects:
    rs = park_routes(e['park_id'])
    problems = []
    if rs is not None:
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
    status = 'skipped' if rs is None else 'failing' if problems else 'ok'
    results.append({k: e[k] for k in ('name', 'park', 'park_id', 'issue')} | {'status': status, 'problems': problems})

checked = [r for r in results if r['status'] != 'skipped']
failing = [r for r in checked if r['status'] == 'failing']
print(f"### Named regressions: {len(failing)} of {len(checked)} failing\n")
for r in results:
    mark = {'ok': '✅', 'failing': '❌', 'skipped': '➖'}[r['status']]
    print(f"- {mark} **{r['name']}** ({r['park']}, {r['issue']}): {r['status']}" + ''.join(f"\n  - {p}" for p in r['problems']))
if out_json:
    json.dump({'regressions': results, 'failing': len(failing), 'checked': len(checked)}, open(out_json, 'w'), indent=1)
