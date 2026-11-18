#!/usr/bin/env python3
"""gut-check-sample.py RUN_DIR TILES_DIR OUT.json [N]

Sample day hikes the curator picked (RUN_DIR/<key>/candidates.geojsonl,
from fixtures/run.sh with ROUTES_ARGS=--candidates) for gut ratings: every
park, oversampling hikes with features that are often a sign of a bizarre
route, with the trails, roads and water around each from the fixture
tiles (TILES_DIR/<key>/*.pmtiles), so a page can draw it in context."""
import glob, gzip, hashlib, json, math, os, random, sys

import mapbox_vector_tile as mvt
import zstandard
from pmtiles.reader import MmapSource, Reader

run, tiles, out = sys.argv[1], sys.argv[2], sys.argv[3]
N = int(sys.argv[4]) if len(sys.argv) > 4 else 150
PER_PARK = 7
random.seed(95)


def m(a, b):
    k = math.cos(math.radians((a[1] + b[1]) / 2))
    return math.hypot((a[0] - b[0]) * 111320 * k, (a[1] - b[1]) * 110540)


def length(l):
    return sum(m(a, b) for a, b in zip(l, l[1:]))


def dp(line, tol):
    """Douglas-Peucker in metres (local)."""
    if len(line) < 3:
        return line
    keep = [False] * len(line)
    keep[0] = keep[-1] = True
    st = [(0, len(line) - 1)]
    while st:
        a, b = st.pop()
        best, bi = 0, 0
        for i in range(a + 1, b):
            d = seg(line[i], line[a], line[b])
            if d > best:
                best, bi = d, i
        if best > tol:
            keep[bi] = True
            st += [(a, bi), (bi, b)]
    return [p for p, k in zip(line, keep) if k]


def seg(p, a, b):
    k = math.cos(math.radians(p[1]))
    px, py = p[0] * 111320 * k, p[1] * 110540
    ax, ay = a[0] * 111320 * k, a[1] * 110540
    bx, by = b[0] * 111320 * k, b[1] * 110540
    dx, dy = bx - ax, by - ay
    l2 = dx * dx + dy * dy
    t = 0 if l2 == 0 else max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / l2))
    return math.hypot(ax + t * dx - px, ay + t * dy - py)


def retraced(line):
    """Share of a loop's length that runs where it already went (within
    25 m of a point at least 300 m back along it): a loop that's really an
    out-and-back with a knot, or walks a stretch twice."""
    pts, acc, out = [], 0.0, 0.0
    for a, b in zip(line, line[1:]):
        d = m(a, b)
        mid = ((a[0] + b[0]) / 2, (a[1] + b[1]) / 2)
        if any(acc - s > 300 and m(mid, q) < 25 for q, s in pts):
            out += d
        pts.append((a, acc))
        acc += d
    return out / acc if acc else 0


def flags(p, line):
    r = p["route"]
    f = []
    if r.get("road_share", 0) >= 0.2:
        f.append("road")
    if r.get("max_sac", 0) >= 4 and p["dest_kind"] != "peak":
        f.append("scramble")
    if p["kind"] == "loop" and retraced(line) >= 0.15:
        f.append("retrace")
    if r["km"] > 30:
        f.append("long")
    if r["km"] < 1.2:
        f.append("short")
    if len(r.get("via", [])) >= 6:
        f.append("patchwork")
    one = r.get("one_way_km", r["km"])
    straight = m(p["start"], p["dest_at"]) / 1000
    if p["kind"] == "out-and-back" and straight > 0.2 and one / straight > 4:
        f.append("winding")
    if p["dest_kind"] == "trail" and False:
        pass
    return f


# --- tiles ---
_readers, _tiles = {}, {}


def tile(path, z, x, y):
    key = (path, x, y)
    if key not in _tiles:
        r = _readers.get(path)
        if r is None:
            r = _readers[path] = Reader(MmapSource(open(path, "rb")))
        b = r.get(z, x, y)
        d = {}
        if b:
            if b[:2] == b"\x1f\x8b":
                b = gzip.decompress(b)
            elif b[:4] == b"\x28\xb5\x2f\xfd":
                b = zstandard.ZstdDecompressor().decompress(b, max_output_size=1 << 26)
            d = mvt.decode(b)
        _tiles[key] = d
    return _tiles[key]


def tilexy(lon, lat, z=14):
    n = 2**z
    return (lon + 180) / 360 * n, (1 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2 * n


def to_ll(tx, ty, px, py, ext, z=14):
    n = 2**z
    X = (tx + px / ext) / n
    Y = (ty + 1 - py / ext) / n
    return (round(X * 360 - 180, 5), round(math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * Y)))), 5))


def lines_of(g):
    t, c = g["type"], g["coordinates"]
    if t == "LineString":
        return [c]
    if t == "MultiLineString":
        return c
    if t == "Polygon":
        return c
    if t == "MultiPolygon":
        return [r for p in c for r in p]
    return []


def context(key, bbox):
    w, s, e, n = bbox
    x0, y1 = tilexy(w, s)
    x1, y0 = tilexy(e, n)
    trails, roads, water, seen = [], [], [], set()
    d = os.path.join(tiles, key)
    for tx in range(int(x0), int(x1) + 1):
        for ty in range(int(y0), int(y1) + 1):
            for fname, layers in (("ways.pmtiles", ("path", "road")), ("water.pmtiles", ("water_poly",))):
                path = os.path.join(d, fname)
                if not os.path.exists(path):
                    continue
                t = tile(path, 14, tx, ty)
                for ln in layers:
                    L = t.get(ln)
                    if not L:
                        continue
                    ext = L.get("extent", 4096)
                    for f in L["features"]:
                        pr = f["properties"]
                        if ln == "road" and pr.get("kind") in ("service",) and pr.get("parking_aisle"):
                            continue
                        for ring in lines_of(f["geometry"]):
                            pts = [to_ll(tx, ty, x, y, ext) for x, y in ring]
                            if not any(w <= p[0] <= e and s <= p[1] <= n for p in pts):
                                continue
                            pts = dp(pts, 6 if ln != "water_poly" else 15)
                            k = (ln, tuple(pts[:2]), tuple(pts[-2:]), len(pts))
                            if k in seen or len(pts) < 2:
                                continue
                            seen.add(k)
                            item = {"p": pts}
                            if pr.get("name"):
                                item["n"] = pr["name"]
                            if ln == "path":
                                item["k"] = pr.get("kind", "path")
                                trails.append(item)
                            elif ln == "road":
                                item["k"] = pr.get("kind", "road")
                                roads.append(item)
                            else:
                                water.append(item)
    return {"trails": trails, "roads": roads, "water": water}


def main():
    parks = {}
    for f in sorted(glob.glob(os.path.join(run, "*", "candidates.geojsonl"))):
        key = f.split("/")[-2]
        if key == "oracle":
            continue
        rows = []
        for l in open(f):
            j = json.loads(l)
            p = j["properties"]
            if not p.get("picked"):
                continue
            g = j["geometry"]
            line = g["coordinates"] if g["type"] == "LineString" else [q for part in g["coordinates"] for q in part]
            p["_line"] = line
            p["_flags"] = flags(p, line)
            p["_key"] = key
            rows.append(p)
        parks[key] = rows
    # Per park: flagged first (shuffled), then the rest, up to PER_PARK.
    pool = []
    for key, rows in parks.items():
        fl = [r for r in rows if r["_flags"]]
        ok = [r for r in rows if not r["_flags"]]
        random.shuffle(fl)
        random.shuffle(ok)
        nf = min(len(fl), (PER_PARK * 3 + 3) // 5)
        pick = fl[:nf] + ok[: PER_PARK - nf]
        if len(pick) < PER_PARK:
            pick += fl[nf : nf + PER_PARK - len(pick)]
        pool += pick
    random.shuffle(pool)
    pool = pool[:N]
    out_rows = []
    for p in pool:
        r, line = p["route"], p["_line"]
        lons, lats = [q[0] for q in line], [q[1] for q in line]
        pad_lat = 400 / 110540
        pad_lon = 400 / (111320 * math.cos(math.radians(sum(lats) / len(lats))))
        bbox = [min(lons) - pad_lon, min(lats) - pad_lat, max(lons) + pad_lon, max(lats) + pad_lat]
        sid = hashlib.sha1(json.dumps([p["_key"], p["from"], p["dest"], p["kind"], r["km"]]).encode()).hexdigest()[:10]
        out_rows.append(
            {
                "id": sid,
                "key": p["_key"],
                "park": p["park"],
                "from": p["from"],
                "dest": p["dest"],
                "dest_kind": p["dest_kind"],
                "kind": p["kind"],
                "km": r["km"],
                "gain": round(r.get("gain", 0)),
                "max_sac": r.get("max_sac", 0),
                "road": round(r.get("road_share", 0), 2),
                "via": r.get("via", []),
                "passes": [a[1] for a in r.get("along", [])],
                "flags": p["_flags"],
                "start": p["start"],
                "dest_at": p["dest_at"],
                "line": [[round(a, 5), round(b, 5)] for a, b in dp(line, 4)],
                "bbox": [round(v, 5) for v in bbox],
                "ctx": context(p["_key"], bbox),
            }
        )
    json.dump(out_rows, open(out, "w"), separators=(",", ":"))
    import collections

    c = collections.Counter(f for r in out_rows for f in r["flags"])
    print(len(out_rows), "hikes;", sum(1 for r in out_rows if r["flags"]), "flagged;", dict(c), file=sys.stderr)
    print(len({r["key"] for r in out_rows}), "parks;", os.path.getsize(out) // 1024, "KiB", file=sys.stderr)


main()
