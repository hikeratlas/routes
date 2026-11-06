#!/usr/bin/env python3
# oracle.py BASE_DIR NEW_DIR [ORACLE.json] > summary.md
#
# Known-hike recall on the oracle sample (fixtures/oracle.json), from two
# run.sh runs over it: how many of the parks' OSM hiking routes each build
# finds (one route covering 70% of it), for the tuning half and the held-out
# half separately. Tune route-proposer changes on the tuning half; the held-out
# half shows whether the gain is real or just fits these parks. The relations
# only measure the routes here: nothing in routes proposes from them.
import json, os, sys

FOUND = 0.7
base_dir, new_dir = sys.argv[1], sys.argv[2]
oracle = json.load(open(sys.argv[3] if len(sys.argv) > 3 else os.path.join(os.path.dirname(os.path.abspath(__file__)), 'oracle.json')))


def known(d, key):
    out = {}
    path = os.path.join(d, key, 'known.jsonl')
    if os.path.exists(path):
        for line in open(path):
            k = json.loads(line)
            if k['id'] not in out or k['coverage'] > out[k['id']]['coverage']:
                out[k['id']] = k
    return out


def titles(d, key):
    path = os.path.join(d, key, 'routes.geojsonl')
    if not os.path.exists(path):
        return set()
    return {json.loads(line)['properties']['title'] for line in open(path)}


def status(d, key):
    try:
        secs, st = open(os.path.join(d, f'{key}.secs')).read().split()
        return int(secs), int(st)
    except (OSError, ValueError):
        return None


halves = {}
for half in ('tune', 'holdout'):
    parks = [p for p in oracle if p['half'] == half]
    h = {'parks': len(parks), 'checked': 0, 'base': 0, 'new': 0, 'changed_parks': 0,
         'failed': [], 'gained': [], 'lost': [], 'base_s': 0, 'new_s': 0}
    for p in parks:
        k = p['key']
        b, n = known(base_dir, k), known(new_dir, k)
        h['checked'] += len(n or b)
        h['base'] += sum(x['coverage'] >= FOUND for x in b.values())
        h['new'] += sum(x['coverage'] >= FOUND for x in n.values())
        if titles(base_dir, k) != titles(new_dir, k):
            h['changed_parks'] += 1
        for d, label in ((base_dir, 'main'), (new_dir, 'this change')):
            s = status(d, k)
            if s is None or s[1] != 0:
                h['failed'].append(f"{p['park']} ({label}: {'not run' if s is None else 'exit ' + str(s[1])})")
        sb, sn = status(base_dir, k), status(new_dir, k)
        h['base_s'] += sb[0] if sb else 0
        h['new_s'] += sn[0] if sn else 0
        for i in set(b) | set(n):
            bc, nc = b.get(i, {}).get('coverage', 0.0), n.get(i, {}).get('coverage', 0.0)
            x = n.get(i) or b[i]
            line = f"[{x['name']}](https://www.openstreetmap.org/relation/{i}) ({x['km']} km), {p['park']}: {bc:.0%} → {nc:.0%}"
            if bc < FOUND <= nc:
                h['gained'].append(line)
            elif nc < FOUND <= bc:
                h['lost'].append(line)
    halves[half] = h

print('## Known hikes on the oracle sample\n')
print('OSM hiking routes in or near each park, found when one route covers 70% of it. Tune on the tuning half; the held-out half checks the gain carries over.\n')
print('| half | parks | known hikes | found (main) | found (this change) | change | parks with different routes | time (main / this change) |')
print('|---|---|---|---|---|---|---|---|')
for half, h in halves.items():
    pct = lambda x: f"{x / h['checked']:.1%}" if h['checked'] else '—'
    print(f"| {half} | {h['parks']} | {h['checked']} | {h['base']} ({pct(h['base'])}) | {h['new']} ({pct(h['new'])}) | {h['new'] - h['base']:+d} | {h['changed_parks']} | {h['base_s']}s / {h['new_s']}s |")
for half, h in halves.items():
    for kind in ('gained', 'lost'):
        if h[kind]:
            print(f"\n<details><summary>{half}: {len(h[kind])} {'newly found' if kind == 'gained' else 'no longer found'}</summary>\n")
            print('\n'.join(f'- {l}' for l in sorted(h[kind])[:40]))
            print('\n</details>')
    if h['failed']:
        print(f"\n**{half}: didn't finish:** " + '; '.join(h['failed']))
