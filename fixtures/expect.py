#!/usr/bin/env python3
# expect.py OUT_DIR > summary.md
#
# Named regressions (fixtures/expect.json): routes we've fixed and must keep
# offering, named right. Each expects, in its fixture's parks, a route to
# `dest` starting within `start_within_m` of `start`, at most `max_km` long,
# named `name`; and no other route named `name`. Exits 1 if any fails, so a
# change that loses one fails the fixtures job.
import glob, json, math, os, sys

out_dir = sys.argv[1]
here = os.path.dirname(os.path.abspath(__file__))
expects = json.load(open(os.path.join(here, 'expect.json')))


def metres(a, b):
    dx = (b[0] - a[0]) * 111320 * math.cos(math.radians(a[1]))
    return math.hypot(dx, (b[1] - a[1]) * 110570)


def routes(fixture):
    """Every chosen route in the fixture's parks: day hikes, point-to-points
    and base camps' hikes."""
    for path in glob.glob(os.path.join(out_dir, fixture, 'parks', '*.json')):
        doc = json.load(open(path))
        yield from doc.get('day_hikes', [])
        yield from doc.get('point_to_points', [])
        for b in doc.get('base_camps', []):
            yield from b.get('hikes', [])


def label(r):
    return f"{r.get('name') or r.get('from')} to {r.get('dest')} ({r['route']['km']} km)"


failed = 0
print('### Named regressions\n')
for e in expects:
    rs = list(routes(e['fixture']))
    want = [r for r in rs if r.get('dest') == e['dest']
            and r.get('start') and metres(r['start'], e['start']) <= e['start_within_m']
            and r['route']['km'] <= e['max_km']]
    named = [r for r in rs if r.get('name') == e['name']]
    problems = []
    if not rs:
        problems.append('no routes for the fixture')
    elif not want:
        to = [label(r) for r in rs if r.get('dest') == e['dest']]
        problems.append(f"no route to {e['dest']} from its trailhead" + (f" (only: {'; '.join(to)})" if to else ''))
    elif not any(r.get('name') == e['name'] for r in want):
        problems.append(f"the route to {e['dest']} isn't named {e['name']} ({label(want[0])})")
    wrong = [label(r) for r in named if r not in want]
    if wrong:
        problems.append(f"{e['name']} names another route: {'; '.join(wrong)}")
    status = 'FAILED' if problems else 'ok'
    failed += bool(problems)
    print(f"- **{e['name']}** ({e['fixture']}, {e['issue']}): {status}" + ''.join(f"\n  - {p}" for p in problems))
sys.exit(1 if failed else 0)
