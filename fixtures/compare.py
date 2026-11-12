#!/usr/bin/env python3
# compare.py BASE_DIR NEW_DIR > summary.md
#
# Compare two runs over the fixtures (see run.sh): for each fixture, how long
# each took and how its routes differ, as markdown for a CI job summary.
# Each DIR holds <key>/routes.geojsonl, <key>.log and <key>.secs (wall time,
# then the exit status: 124 if `timeout` stopped it).
import glob, json, os, sys

from featured import describe as describe_featured, featured

base_dir, new_dir = sys.argv[1], sys.argv[2]
parks = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'parks.json')))


def run(d, key):
    """(seconds, status, routes by id, profile line) for one fixture."""
    try:
        secs, status = open(os.path.join(d, f'{key}.secs')).read().split()
    except (OSError, ValueError):
        return None
    routes = {}
    path = os.path.join(d, key, 'routes.geojsonl')
    if os.path.exists(path):
        for line in open(path):
            p = json.loads(line)['properties']
            routes[p['id']] = p
    log = open(os.path.join(d, f'{key}.log'), errors='replace').read()
    timeouts = [l for l in log.splitlines() if l.startswith('TIMEOUT: ')]
    return int(secs), int(status), routes, timeouts


def known(d, key):
    """OSM hiking routes checked in the fixture: id -> its known.jsonl line,
    the best coverage over the parks it's in."""
    out = {}
    path = os.path.join(d, key, 'known.jsonl')
    if os.path.exists(path):
        for line in open(path):
            k = json.loads(line)
            if k['id'] not in out or k['coverage'] > out[k['id']]['coverage']:
                out[k['id']] = k
    return out


def found(ks):
    return sum(k['coverage'] >= 0.7 for k in ks.values())


def fmt_time(r):
    if r is None:
        return 'not run'
    secs, status, _, timeouts = r
    if status == 124:
        return f'**over {secs}s, stopped**'
    if status != 0:
        return f'**failed ({status})** after {secs}s'
    return f'{secs}s' + (' (park timeout)' if timeouts else '')


def park_featured(d, f):
    """The fixture's park's featured hike on the homepage (featured.py), as
    a line, or None if the run has no document for the park."""
    for path in glob.glob(os.path.join(d, f['key'], 'parks', '*.json')):
        doc = json.load(open(path))
        p = doc.get('park', {})
        if str(p.get('id')) == str(f.get('id')) or p.get('name') == f['park']:
            h = featured(doc)
            return describe_featured(h) if h else '—'
    return None


def describe(p):
    return f"{p['title']} ({p['type']}, {p.get('kind', '')}, {p['km']} km, {p['gain']:.0f} m)"


rows, details, known_rows, known_details = [], [], [], []
featured_rows, featured_changed = [], []
for f in parks:
    bf, nf = park_featured(base_dir, f), park_featured(new_dir, f)
    if bf or nf:
        featured_rows.append(f"| {f['key']} | {bf or '—'} | {'same' if bf == nf else nf or '—'} |")
        if bf != nf:
            featured_changed.append(f"- **{f['key']}**: {bf or '—'} → {nf or '—'}")
    bk_, nk_ = known(base_dir, f['key']), known(new_dir, f['key'])
    if bk_ or nk_:
        known_rows.append(f"| {f['key']} | {len(nk_)} | {found(bk_)} | {found(nk_)} |")
        for i in sorted(set(bk_) | set(nk_)):
            bc = bk_.get(i, {}).get('coverage', 0.0)
            nc = nk_.get(i, {}).get('coverage', 0.0)
            k = nk_.get(i) or bk_[i]
            if (bc >= 0.7) != (nc >= 0.7) or abs(bc - nc) >= 0.2:
                known_details.append(f"- {f['key']}: [{k['name']}](https://www.openstreetmap.org/relation/{i}) ({k['km']} km): {bc:.0%} → {nc:.0%}, best now {nk_.get(i, {}).get('best') or '—'}")
    b, n = run(base_dir, f['key']), run(new_dir, f['key'])
    br, nr = (b[2] if b else {}), (n[2] if n else {})
    # Routes match by title and type; ids are positions, which shift.
    bk = {(p['title'], p['type']): p for p in br.values()}
    nk = {(p['title'], p['type']): p for p in nr.values()}
    added = [nk[k] for k in nk if k not in bk]
    removed = [bk[k] for k in bk if k not in nk]
    changed = [(bk[k], nk[k]) for k in bk if k in nk and (bk[k]['km'], bk[k]['gain']) != (nk[k]['km'], nk[k]['gain'])]
    same = not (added or removed or changed)
    diff = 'same' if same else f'+{len(added)} −{len(removed)} ~{len(changed)}'
    rows.append(f"| {f['key']} | {fmt_time(b)} | {fmt_time(n)} | {len(br)} | {len(nr)} | {diff} |")
    if not same:
        lines = [f"<details><summary><b>{f['key']}</b>: {diff}</summary>\n"]
        lines += [f'- added: {describe(p)}' for p in added]
        lines += [f'- removed: {describe(p)}' for p in removed]
        lines += [f"- changed: {p['title']} ({p['type']}): {p['km']} → {q['km']} km, {p['gain']:.0f} → {q['gain']:.0f} m" for p, q in changed]
        lines.append('\n</details>')
        details.append('\n'.join(lines))

print('## Routes on the fixture parks\n')
print('Both builds ran on the same tiles (the live north-america layers, see fixtures/parks.json for why each park is here).\n')
print('| fixture | main | this change | routes (main) | routes (this change) | routes added / removed / changed |')
print('|---|---|---|---|---|---|')
print('\n'.join(rows))
if details:
    print('\n' + '\n\n'.join(details))
if known_rows:
    print('\n### Known hikes\n')
    print('OSM hiking routes in or near each fixture park: found when one route covers 70% of it.\n')
    print('| fixture | known hikes | found (main) | found (this change) |')
    print('|---|---|---|---|')
    print('\n'.join(known_rows))
    if known_details:
        print('\nChanged:\n\n' + '\n'.join(known_details))
if featured_rows:
    print('\n### Featured hikes\n')
    print("Each fixture park's featured hike on the homepage, as the website picks it (fixtures/featured.py).\n")
    print('\n'.join(featured_changed) if featured_changed else 'No featured hike changed.')
    print('\n<details><summary>All</summary>\n')
    print('| fixture | main | this change |')
    print('|---|---|---|')
    print('\n'.join(featured_rows))
    print('\n</details>')
for d, label in ((base_dir, 'main'), (new_dir, 'this change')):
    for f in parks:
        r = run(d, f['key'])
        if r and r[3]:
            print(f"\n{label}, {f['key']}: " + '; '.join(r[3]))
