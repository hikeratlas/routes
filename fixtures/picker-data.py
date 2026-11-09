#!/usr/bin/env python3
# picker-data.py EXPECT.jsonl ROUTES_OUT TILES_DIR > picker.json
#
# The data for the trace picker: for each `trail` entry in one park's
# named regressions file (fixtures/expect/<region>-<park>.jsonl), the hikes
# routes considered near it, so a person can pick the one that's meant (or
# draw it) and its line becomes the entry's `trace` (see expect.py).
#
# ROUTES_OUT is a routes run over the park with --candidates (and --trails);
# TILES_DIR holds the park's ways.pmtiles and water.pmtiles (extract.py), for
# the map: its paths, roads and lakes, as routes sees them.
#
# Writes {"park", "park_id", "entries": [{id, name, km, why, source, issue,
# trail (the OSM trail's lines), candidates: [GeoJSON features]}],
# "paths", "roads", "lakes"}, coordinates rounded to ~1 m.
#
# Needs: pip install pmtiles mapbox-vector-tile zstandard shapely
import json, math, os, sys

import mapbox_vector_tile, zstandard
from pmtiles.reader import MmapSource, Reader, all_tiles
from shapely.geometry import LineString, MultiLineString, Point, Polygon, box, mapping, shape
from shapely.ops import unary_union

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from expectations import norm  # noqa: E402

NEAR_M = 400      # a candidate within this of the OSM trail is shown
PAD_M = 1500      # the map around the trail and its candidates
MAX_CANDS = 30

path, out, tiles = sys.argv[1:4]
entries = [json.loads(line) for line in open(path, encoding='utf-8') if line.strip()]
park_id = entries[0]['park_id']


def tile_features(layer_file, layers):
    """Every z14 feature of the given layers, as (layer, props, shapely geom in lon/lat)."""
    f = open(os.path.join(tiles, layer_file), 'rb')
    r = Reader(MmapSource(f))
    out = []
    for (z, x, y), data in all_tiles(r.get_bytes):
        if z != 14:
            continue
        t = mapbox_vector_tile.decode(zstandard.ZstdDecompressor().decompressobj().decompress(data),
                                      default_options={'y_coord_down': True})
        for name, layer in t.items():
            if name not in layers:
                continue
            ext = layer.get('extent', 4096)

            def ll(p):
                lon = (x + p[0] / ext) / 2 ** z * 360 - 180
                n = math.pi - 2 * math.pi * (y + p[1] / ext) / 2 ** z
                return (lon, math.degrees(math.atan(math.sinh(n))))

            def conv(c):
                return [conv(v) for v in c] if isinstance(c[0][0], (list, tuple)) else [ll(p) for p in c]
            for ft in layer['features']:
                g = ft['geometry']
                try:
                    geom = shape({'type': g['type'], 'coordinates': conv(g['coordinates'])
                                  if g['type'] != 'Point' else ll(g['coordinates'])})
                except (ValueError, TypeError, IndexError):
                    continue
                out.append((name, ft['properties'], geom))
    return out


def deg(m, lat):
    """Metres as degrees of latitude and longitude at lat, roughly."""
    return m / 110570, m / (111320 * math.cos(math.radians(lat)))


def near(geom, other, m):
    dlat, dlon = deg(m, geom.centroid.y)
    return geom.distance(other) <= max(dlat, dlon)


def rounded(geom):
    return json.loads(json.dumps(mapping(geom.simplify(0.00003)), default=list),
                      parse_float=lambda v: round(float(v), 5))


ways = tile_features('ways.pmtiles', {'path', 'road'})
water = tile_features('water.pmtiles', {'water_poly'})
cands = []
for f in ('candidates.geojsonl', '*/candidates.geojsonl'):
    import glob
    for p in glob.glob(os.path.join(out, f)):
        for line in open(p):
            c = json.loads(line)
            if str(c['properties'].get('park_id')) == park_id:
                cands.append(c)
if not cands:
    sys.exit(f'picker-data: no candidates for park {park_id} in {out} (run routes with --candidates)')

result = {'park': entries[0]['park'], 'park_id': park_id, 'entries': []}
focus = []
for i, e in enumerate(entries):
    if e.get('kind', 'dest') != 'trail':
        continue
    osm = norm(e.get('trail', e['name']))
    lines = [g for n, p, g in ways if p.get('name') and norm(p['name']) == osm]
    trail = unary_union(lines) if lines else None
    pick = []
    for c in cands:
        g = shape(c['geometry'])
        if trail is not None and near(g, trail, NEAR_M):
            share = 0
            if trail.length:
                dlat, _ = deg(50, g.centroid.y)
                share = trail.intersection(g.buffer(dlat)).length / trail.length
            pick.append((share, c, g))
    pick.sort(key=lambda x: (-x[0], abs(float(x[1]['properties']['route']['km']) - e['km'])))
    pick = pick[:MAX_CANDS]
    area = unary_union([g for _, _, g in pick] + ([trail] if trail is not None else []))
    if area.is_empty:
        print(f"picker-data: {e['name']}: no OSM trail of that name and no candidates near it; skipped",
              file=sys.stderr)
        continue
    dlat, dlon = deg(PAD_M, area.centroid.y)
    w, s, ea, n = area.bounds
    bb = (w - dlon, s - dlat, ea + dlon, n + dlat)
    focus.append(box(*bb))
    feats = []
    for share, c, g in pick:
        pr = c['properties']
        feats.append({'type': 'Feature', 'geometry': rounded(g), 'properties': {
            'from': pr['from'], 'dest': pr['dest'], 'kind': pr['kind'], 'km': pr['route']['km'],
            'score': pr['score'], 'picked': pr.get('picked', False), 'on_trail': round(share, 2),
            'via': pr['route'].get('via', [])[:6]}})
    result['entries'].append({
        'id': f"{os.path.basename(path)[:-len('.jsonl')]}:{i}", 'line': i, 'name': e['name'],
        'km': e['km'], 'why': e.get('why', ''), 'source': e.get('source', ''), 'issue': e['issue'],
        'osm_trail': e.get('trail', e['name']), 'bbox': [round(v, 5) for v in bb],
        'trail': rounded(trail) if trail is not None else None, 'candidates': feats,
        'trace': e.get('trace')})

region = unary_union(focus)
result['paths'] = [rounded(g.intersection(region)) for n, p, g in ways
                   if n == 'path' and g.intersects(region)]
result['roads'] = [rounded(g.intersection(region)) for n, p, g in ways
                   if n == 'road' and g.intersects(region)]
result['lakes'] = [rounded(g.intersection(region)) for n, p, g in water
                   if isinstance(g, Polygon) or g.geom_type == 'MultiPolygon'
                   if g.intersects(region)]
for k in ('paths', 'roads', 'lakes'):
    result[k] = [g for g in result[k] if g.get('coordinates')]
json.dump(result, sys.stdout, separators=(',', ':'))
print(f"picker-data: {len(result['entries'])} entries, "
      f"{sum(len(e['candidates']) for e in result['entries'])} candidates, "
      f"{len(result['paths'])} paths", file=sys.stderr)
