#!/usr/bin/env python3
# extract.py BASE_URL OUT_DIR [KEY...]
#
# For each fixture in parks.json (or just the KEYs given), copy the z8 and z14
# tiles covering its box from each layer archive at BASE_URL (e.g.
# https://public.hikeratlas.com/layers/north-america) into
# OUT_DIR/<key>/<layer>.pmtiles, for routes --tiles.
#
# It reads each archive's directories once, then fetches the tiles as a few
# large byte ranges: an archive stores tiles in Hilbert order, so the tiles in
# a box sit in a few runs, and ranges less than GAP apart are merged into one
# request (as `pmtiles extract --overfetch` does). All of parks.json takes
# ~5 min (mostly decoding the archives' leaf directories); a request per tile
# took most of an hour for one box.
#
# Needs: pip install pmtiles requests
import bisect, gzip, json, math, os, sys, time
from concurrent.futures import ThreadPoolExecutor

import requests
from pmtiles.tile import Compression, deserialize_directory, deserialize_header, find_tile, zxy_to_tileid
from pmtiles.writer import Writer

LAYERS = ['ways', 'land', 'water', 'parks', 'peaks', 'pois', 'trailareas', 'hikingroutes']
# Layers a build may not have yet: skipped if missing.
OPTIONAL = {'trailareas', 'hikingroutes'}
GAP = 1 << 20
session = requests.Session()


def fetch(url, offset, length):
    for attempt in range(6):
        try:
            r = session.get(url, headers={'Range': f'bytes={offset}-{offset + length - 1}'}, timeout=120)
            r.raise_for_status()
            return r.content
        except requests.RequestException:
            if attempt == 5:
                raise
            time.sleep(2 ** attempt)


def tiles(box, z):
    n = 2 ** z
    x = lambda lon: int((lon + 180) / 360 * n)
    y = lambda lat: int((1 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2 * n)
    return [(z, i, j) for i in range(x(box[0]), x(box[2]) + 1) for j in range(y(box[3]), y(box[1]) + 1)]


def extract(url, box, out):
    hdr = deserialize_header(fetch(url, 0, 127))
    dirs = {}

    def directory(off, ln):
        if (off, ln) not in dirs:
            dirs[(off, ln)] = deserialize_directory(fetch(url, off, ln))
        return dirs[(off, ln)]

    def locate(tid):
        off, ln = hdr['root_offset'], hdr['root_length']
        for _ in range(4):
            e = find_tile(directory(off, ln), tid)
            if e is None:
                return None
            if e.run_length == 0:
                off, ln = hdr['leaf_directory_offset'] + e.offset, e.length
            else:
                return e.offset, e.length
        return None

    want = sorted(zxy_to_tileid(*t) for t in tiles(box, 8) + tiles(box, 14))
    loc = {t: l for t in want if (l := locate(t))}
    spans = []
    for o, l in sorted(set(loc.values())):
        if spans and o <= spans[-1][1] + GAP:
            spans[-1][1] = max(spans[-1][1], o + l)
        else:
            spans.append([o, o + l])
    base = hdr['tile_data_offset']
    with ThreadPoolExecutor(8) as ex:
        blobs = list(ex.map(lambda sp: fetch(url, base + sp[0], sp[1] - sp[0]), spans))
    starts = [sp[0] for sp in spans]
    meta = fetch(url, hdr['metadata_offset'], hdr['metadata_length']) if hdr['metadata_length'] else b'{}'
    if hdr['internal_compression'] == Compression.GZIP:
        meta = gzip.decompress(meta)
    with open(out, 'wb') as f:
        w = Writer(f)
        for tid in sorted(loc):
            o, l = loc[tid]
            i = bisect.bisect_right(starts, o) - 1
            w.write_tile(tid, blobs[i][o - starts[i]: o - starts[i] + l])
        h = dict(hdr)
        h.update(min_zoom=8, max_zoom=14, min_lon_e7=int(box[0] * 1e7), min_lat_e7=int(box[1] * 1e7),
                 max_lon_e7=int(box[2] * 1e7), max_lat_e7=int(box[3] * 1e7))
        w.finalize(h, json.loads(meta))
    return len(loc), len(spans), sum(b - a for a, b in spans)


def main():
    base, out_dir, keys = sys.argv[1].rstrip('/'), sys.argv[2], sys.argv[3:]
    parks = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'parks.json')))
    jobs = [(p, layer) for p in parks if not keys or p['key'] in keys for layer in LAYERS]
    for p in parks:
        os.makedirs(os.path.join(out_dir, p['key']), exist_ok=True)

    def run(job):
        p, layer = job
        t = time.time()
        if layer in OPTIONAL and requests.head(f'{base}/{layer}.pmtiles', timeout=60).status_code == 404:
            return f"{p['key']}/{layer}: none in this build, skipped"
        n, spans, size = extract(f'{base}/{layer}.pmtiles', p['bbox'], os.path.join(out_dir, p['key'], f'{layer}.pmtiles'))
        return f"{p['key']}/{layer}: {n} tiles in {spans} ranges, {size / 1e6:.1f} MB, {time.time() - t:.0f}s"

    with ThreadPoolExecutor(4) as ex:
        for line in ex.map(run, jobs):
            print(line, flush=True)


if __name__ == '__main__':
    main()
