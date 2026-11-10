#!/usr/bin/env python3
# extract.py [--parks FILE] BASE_URL OUT_DIR [KEY...]
#
# For each fixture in parks.json (or FILE, e.g. oracle.json; or just the KEYs given), copy the z8 and z14
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
import bisect, gzip, json, math, os, sys, threading, time
from concurrent.futures import ThreadPoolExecutor

import requests
from pmtiles.tile import Compression, deserialize_directory, deserialize_header, find_tile, zxy_to_tileid
from pmtiles.writer import Writer

LAYERS = ['ways', 'land', 'water', 'parks', 'peaks', 'pois', 'trailareas', 'hikingroutes', 'buildings']
# Layers a build may not have yet: skipped if missing. buildings: parking
# lots' outlines, which join a trail ending on one to the lot's roads.
OPTIONAL = {'trailareas', 'hikingroutes', 'buildings'}
GAP = 1 << 20
session = requests.Session()


class Changed(Exception):
    """The archive was replaced while we read it (a build rewriting it)."""


etags = {}  # url -> the ETag of the version we started reading


def retried(f, tries=6):
    """f(), retried with backoff (1, 2, 4, 8, 16 s) on a connection error, a
    timeout, a body cut off part way (ChunkedEncodingError: the snapshot's
    250 MB) or a 5xx: a build's fixtures job failed on one dropped HEAD."""
    for attempt in range(tries):
        try:
            r = f()
            if r.status_code < 500:
                return r
            r.raise_for_status()
        except (requests.ConnectionError, requests.Timeout, requests.ChunkedEncodingError,
                requests.HTTPError):
            if attempt == tries - 1:
                raise
        time.sleep(2 ** attempt)


def exists(url):
    """Whether the build has the archive at url (a HEAD that isn't 404)."""
    return retried(lambda: session.head(url, timeout=60)).status_code != 404


def fetch(url, offset, length):
    """Bytes [offset, offset+length) of the archive's version we first read:
    with If-Match, a replaced file answers 412 instead of mixing versions."""
    for attempt in range(6):
        headers = {'Range': f'bytes={offset}-{offset + length - 1}'}
        if url in etags:
            headers['If-Match'] = etags[url]
        try:
            r = session.get(url, headers=headers, timeout=120)
            if r.status_code == 412:
                raise Changed(f'{url} changed while extracting; run again once the build is done')
            r.raise_for_status()
            etags.setdefault(url, r.headers.get('ETag'))
            if etags[url] is None:
                del etags[url]
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


# Each archive's header and directories, read once however many boxes come
# from it: the oracle sample's 160 parks read the same 8 archives.
headers, dirs, locks = {}, {}, {}
locks_lock = threading.Lock()


def extract(url, box, out):
    with locks_lock:
        lock = locks.setdefault(url, threading.Lock())
    with lock:
        if url not in headers:
            headers[url] = deserialize_header(fetch(url, 0, 127))
            dirs[url] = {}
    hdr, cache = headers[url], dirs[url]

    def directory(off, ln):
        with lock:
            if (off, ln) not in cache:
                cache[(off, ln)] = deserialize_directory(fetch(url, off, ln))
            return cache[(off, ln)]

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
    if not loc:
        return 0, 0, 0  # no tiles in this box (e.g. no trail areas): no archive
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
    args = sys.argv[1:]
    parks_file = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'parks.json')
    if args[:1] == ['--parks']:
        parks_file, args = args[1], args[2:]
    base, out_dir, keys = args[0].rstrip('/'), args[1], args[2:]
    parks = json.load(open(parks_file))
    jobs = [(p, layer) for p in parks if not keys or p['key'] in keys for layer in LAYERS]
    for p in parks:
        os.makedirs(os.path.join(out_dir, p['key']), exist_ok=True)

    def run(job):
        p, layer = job
        t = time.time()
        if layer in OPTIONAL and not exists(f'{base}/{layer}.pmtiles'):
            return f"{p['key']}/{layer}: none in this build, skipped"
        n, spans, size = extract(f'{base}/{layer}.pmtiles', p['bbox'], os.path.join(out_dir, p['key'], f'{layer}.pmtiles'))
        return f"{p['key']}/{layer}: {n} tiles in {spans} ranges, {size / 1e6:.1f} MB, {time.time() - t:.0f}s"

    with ThreadPoolExecutor(4) as ex:
        for line in ex.map(run, jobs):
            print(line, flush=True)


if __name__ == '__main__':
    main()
