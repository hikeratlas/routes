#!/usr/bin/env python3
# make-oracle.py KNOWN.jsonl HIKING_ROUTES.geojsonl PARKS.geojsonl [N] > oracle.json
#
# Picks the oracle sample (fixtures/oracle.json) from a north-america build:
# its routes' known.jsonl (the basemap's known-hikes artifact) and its
# hiking-routes and parks exports (geojsonl/north-america/ in R2). Parks, not
# trail areas (which change with the trail-area rules), with at least 2 known
# hikes of a hike's size mostly in or near them, and at most 600 z14 tiles, so
# the sample stays quick to extract and run. N of them (default 160), spread
# by a hash of the park id, and split in halves by another: tune on one half,
# check on the other.
import hashlib, json, math, sys
from collections import defaultdict

known_path, routes_path, parks_path = sys.argv[1:4]
n = int(sys.argv[4]) if len(sys.argv) > 4 else 160
MAX_TILES = 600


def km_of(g):
    parts = [g['coordinates']] if g['type'] == 'LineString' else g['coordinates']
    return sum(math.hypot((b[0] - a[0]) * math.cos(math.radians(a[1])), b[1] - a[1]) * 111.32
               for line in parts for a, b in zip(line, line[1:]))


total = {}
for line in open(routes_path):
    f = json.loads(line)
    total[str(f['properties']['osm_id'])] = km_of(f['geometry'])

per, names = defaultdict(list), {}
for line in open(known_path):
    k = json.loads(line)
    t = total.get(k['id'], 0)
    if t > 100 or k['km'] < 0.5 * t:  # a long trail's section passing by
        continue
    per[k['park_id']].append(k)
    names[k['park_id']] = k['park']
cands = {p for p, ks in per.items() if not p.startswith('trail-') and len(ks) >= 2}

bbox = {}
for line in open(parks_path):
    f = json.loads(line)
    pid = str(f['properties'].get('osm_id'))
    if pid not in cands:
        continue
    xs, ys = [], []

    def walk(c):
        if isinstance(c[0], (int, float)):
            xs.append(c[0]); ys.append(c[1])
        else:
            for x in c:
                walk(x)
    walk(f['geometry']['coordinates'])
    b = bbox.get(pid, [999, 999, -999, -999])
    bbox[pid] = [min(b[0], min(xs)), min(b[1], min(ys)), max(b[2], max(xs)), max(b[3], max(ys))]


def grown(b):
    """The park's box plus 5 km, as routes reads for trailheads outside."""
    lat = (b[1] + b[3]) / 2
    dy = 5000 / 111320
    dx = dy / math.cos(math.radians(lat))
    return [b[0] - dx, b[1] - dy, b[2] + dx, b[3] + dy]


def tiles(b):
    z = 2 ** 14
    x = lambda lon: (lon + 180) / 360 * z
    y = lambda lat: (1 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2 * z
    return (x(b[2]) - x(b[0]) + 1) * (y(b[1]) - y(b[3]) + 1)


small = sorted((p for p in bbox if tiles(grown(bbox[p])) <= MAX_TILES),
               key=lambda p: hashlib.sha1(p.encode()).hexdigest())
out = []
for p in small[:n]:
    half = 'holdout' if int(hashlib.sha1(('half:' + p).encode()).hexdigest(), 16) % 2 else 'tune'
    out.append({'key': p, 'id': p, 'park': names[p], 'half': half, 'known': len(per[p]),
                'bbox': [round(v, 3) for v in grown(bbox[p])]})
print(json.dumps(out, indent=1))
