#!/usr/bin/env python3
"""Give open proposals that have no OSM trail of their name a place to look.

A proposal without `osm_bbox` (trail-candidates.py found no OSM trail of its
name) leaves the website's /dev/regressions page nothing to frame but the
whole park: hundreds of km across in Jasper. Most trails are named for where
they go (Lake Annette Loop, Wabasso Lake, Lower Sunwapta Falls), so this
looks that place up on OSM's Nominatim, within the park's box, and writes
`near` (what it found, as OSM names it) and `near_bbox` ([W, S, E, N], a few
km around it) for the page to frame instead.

    fixtures/locate-proposals.py [--write] [FILE ...]

Without --write it prints what it would set. Proposals that already have
`near_bbox`, or aren't open, are left alone, so it can be run again as
proposals are added. Nominatim's policy: at most one request a second.
"""
import argparse, json, math, os, re, sys, time, urllib.error, urllib.parse, urllib.request

from proposals import DIR

UA = 'hikeratlas-routes-fixtures (https://github.com/hikeratlas/routes)'
# Words that name the kind of hike, not the place.
HIKE = r'(loop|trail|trails|circuit|hike|walk|path|route|trailhead|lookout trail|viewpoint trail|interpretive|nature)'
# Boxes are padded to at least this many km on a side, and at most this many:
# a long lake or river would otherwise frame half the park again.
MIN_KM, MAX_KM = 2.0, 6.0


def variants(name):
    """Place names to try, most specific first."""
    n = re.sub(r'\(.*?\)', '', name).strip()
    out = [n]
    bare = re.sub(rf'(\s+{HIKE})+$', '', n, flags=re.I).strip()
    bare = re.sub(r'^(the|lower|upper)\s+', '', bare, flags=re.I).strip()
    out.append(bare)
    if re.search(r'\blakes$', bare, re.I):
        # Mina-Riley Lakes -> Mina Lake
        first = re.split(r'[-–&/]| and ', bare[:-len('lakes')])[0].strip()
        out += [bare[:-1], f'{first} Lake']
    for part in re.split(r'\s+(?:to|via)\s+', bare, flags=re.I)[1:]:
        out.append(part.strip())   # Trail to X Falls -> X Falls
    seen = []
    for v in out:
        if v and len(v) > 2 and v.lower() not in (s.lower() for s in seen):
            seen.append(v)
    return seen


_last = [0.0]


def get(path, params):
    time.sleep(max(0.0, 1.1 - (time.time() - _last[0])))
    _last[0] = time.time()
    url = f'https://nominatim.openstreetmap.org/{path}?' + urllib.parse.urlencode(params)
    # Nominatim answers 429 when it's busy: back off and try again.
    for wait in (5, 30, 120, 300, None):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers={'User-Agent': UA}), timeout=60) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code != 429 or wait is None:
                raise
            time.sleep(wait)


def park_shape(park_id):
    """The park's outline (the OSM relation park_id), as polygons of rings,
    or None if Nominatim hasn't got it."""
    hits = get('lookup', {'osm_ids': f'R{park_id}', 'format': 'jsonv2', 'polygon_geojson': 1, 'polygon_threshold': 0.001})
    g = hits[0].get('geojson') if hits else None
    if not g or g['type'] not in ('Polygon', 'MultiPolygon'):
        return None
    return [g['coordinates']] if g['type'] == 'Polygon' else g['coordinates']


def inside(shape, lon, lat):
    def in_ring(ring):
        hit = False
        for (x1, y1), (x2, y2) in zip(ring, ring[1:] + ring[:1]):
            if (y1 > lat) != (y2 > lat) and lon < x1 + (lat - y1) * (x2 - x1) / (y2 - y1):
                hit = not hit
        return hit
    return any(in_ring(poly[0]) and not any(in_ring(h) for h in poly[1:]) for poly in shape)


def words(s):
    return sorted(w for w in re.sub(r'[^0-9a-z]+', ' ', s.lower()).split() if w != 'the')


def nominatim(q, bbox, shape):
    """The first place named q (its words, in any order: Lake Annette is
    Annette Lake) in the park: inside its outline where that's known, as
    a box can take in a neighbour's lake of the same name."""
    w, s, e, n = bbox
    hits = get('search', {'q': q, 'format': 'jsonv2', 'viewbox': f'{w},{n},{e},{s}', 'bounded': 1, 'limit': 10})
    for h in hits:
        if words(h.get('name') or '') == words(q) and (shape is None or inside(shape, float(h['lon']), float(h['lat']))):
            return h
    return None


def box_around(hit):
    """The hit's own box, padded to MIN_KM and clamped to MAX_KM around its point."""
    lat, lon = float(hit['lat']), float(hit['lon'])
    s, n, w, e = map(float, hit['boundingbox'])
    ky, kx = 111.0, 111.0 * math.cos(math.radians(lat))
    half_x = min(MAX_KM, max(MIN_KM, (e - w) * kx + 1.0)) / 2 / kx
    half_y = min(MAX_KM, max(MIN_KM, (n - s) * ky + 1.0)) / 2 / ky
    cx, cy = (w + e) / 2, (s + n) / 2
    if (e - w) * kx > MAX_KM or (n - s) * ky > MAX_KM:
        cx, cy = lon, lat
    r = lambda v: round(v, 4)
    return [r(cx - half_x), r(cy - half_y), r(cx + half_x), r(cy + half_y)]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('files', nargs='*')
    ap.add_argument('--write', action='store_true')
    a = ap.parse_args()
    files = a.files or sorted(os.path.join(DIR, f) for f in os.listdir(DIR) if f.endswith('.jsonl'))
    found = missed = 0
    for path in files:
        lines = open(path, encoding='utf-8').read().split('\n')
        changed = False
        shapes = {}
        for i, line in enumerate(lines):
            if not line.strip():
                continue
            p = json.loads(line)
            if p['status'] != 'open' or p.get('osm_bbox') or p.get('near_bbox') or not p.get('park_bbox'):
                continue
            if p['park_id'] not in shapes:
                shapes[p['park_id']] = park_shape(p['park_id'])
            hit = q = None
            for q in variants(p['name']):
                hit = nominatim(q, p['park_bbox'], shapes[p['park_id']])
                if hit:
                    break
            if not hit:
                missed += 1
                print(f"{os.path.basename(path)}: {p['name']}: nothing found", file=sys.stderr)
                continue
            found += 1
            near = f"{hit.get('name') or q} ({hit.get('category', hit.get('class'))}={hit['type']})"
            p['near'], p['near_bbox'] = near, box_around(hit)
            print(f"{os.path.basename(path)}: {p['name']} -> {near} {p['near_bbox']}")
            # Appended, so the rest of the line keeps its spelling.
            lines[i] = re.sub(r'\}\s*$', f", \"near\": {json.dumps(near, ensure_ascii=False)}, \"near_bbox\": {json.dumps(p['near_bbox'])}}}", line)
            changed = True
        if changed and a.write:
            open(path, 'w', encoding='utf-8').write('\n'.join(lines))
    print(f'{found} located, {missed} not', file=sys.stderr)


if __name__ == '__main__':
    main()
