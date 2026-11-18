#!/usr/bin/env python3
"""gut-check-links.py HIKES.json RUN_DIR LIVE_DIR

Give each sampled hike the URL of its hike sheet on www.hikeratlas.com, from
the live north-america routes build: LIVE_DIR/index.json (park paths, and
where each park's document is in the bundle) and the park's document,
fetched once per park by byte range. Slugs as the website makes them
(app/hikes.ts' hikeEntries). A hike that isn't in the live build (it came
from a change not built yet) keeps no url, and the page falls back to the
map."""
import gzip, json, re, sys, unicodedata, urllib.request

hikes_path, run, live = sys.argv[1:4]
ROOT = "https://public.hikeratlas.com/routes/north-america"
index = json.load(open(f"{live}/index.json"))


def slugify(s):
    s = unicodedata.normalize("NFKD", s)
    s = "".join(c for c in s if not unicodedata.combining(c)).lower()
    return re.sub(r"[^a-z0-9]+", "-", s).strip("-")


def entries(doc):
    days = doc.get("day_hikes") or []
    camps = doc.get("base_camps") or []
    multis = doc.get("point_to_points") or []
    seen = {}

    def count(s):
        seen[s.lower()] = seen.get(s.lower(), 0) + 1

    named = lambda r, o: r.get("name") if isinstance(r.get("name"), str) and r.get("name") else o
    for h in days:
        count(h["dest"])
        if h.get("name"):
            count(h["name"])
    for b in camps:
        count(b["camp"])
    for t in multis:
        count(named(t, f"{t['from']} to {t['to']}"))
    used = {}

    def claim(base):
        n = used.get(base, 0) + 1
        used[base] = n
        return base if n == 1 else f"{base}-{n}"

    out = []
    for h in days:
        title = h["dest"] if not h.get("name") and seen.get(h["dest"].lower(), 0) == 1 else named(h, f"{h['dest']} from {h['from']}")
        out.append((h, title, claim(slugify(title) or "hike")))
    for h in doc.get("stops") or []:
        claim(slugify(h["dest"]) or "stop")
    for b in camps:
        claim(slugify(b["camp"]) or "camp")
    for t in multis:
        claim(slugify(named(t, f"{t['from']} to {t['to']}")) or "hike")
    return out


def park_doc(pid):
    at = index["parks"].get(pid)
    if not at:
        return None
    off, n = at
    req = urllib.request.Request(f"{ROOT}/{index['bundle']}", headers={"Range": f"bytes={off}-{off + n - 1}", "User-Agent": "hikeratlas-routes/1.0"})
    with urllib.request.urlopen(req, timeout=60) as r:
        b = r.read()
        if r.status != 206:
            b = b[off : off + n]
    return json.loads(gzip.decompress(b))


hikes = json.load(open(hikes_path))
park_ids = {}
for h in hikes:
    if h["key"] not in park_ids:
        for l in open(f"{run}/{h['key']}/candidates.geojsonl"):
            p = json.loads(l)["properties"]
            if p["park"] == h["park"]:
                park_ids[h["key"]] = str(p["park_id"])
                break
docs, found = {}, 0
for h in hikes:
    pid = park_ids.get(h["key"])
    h.pop("url", None)
    h.pop("live_title", None)
    if not pid:
        continue
    if pid not in docs:
        docs[pid] = park_doc(pid)
    doc, path = docs[pid], index["paths"].get(pid)
    if not doc or not path:
        continue
    best = None
    for d, title, slug in entries(doc):
        if d["from"] == h["from"] and d["dest"] == h["dest"] and d["kind"] == h["kind"]:
            diff = abs(d["route"]["km"] - h["km"])
            if best is None or diff < best[0]:
                best = (diff, title, slug)
    if best and best[0] <= max(1.0, 0.15 * h["km"]):
        path = path if path.startswith("/") else "/hikes/" + path
        h["url"] = f"https://www.hikeratlas.com{path}/{best[2]}"
        h["live_title"] = best[1]
        found += 1
json.dump(hikes, open(hikes_path, "w"), separators=(",", ":"))
print(f"{found} of {len(hikes)} hikes found in the live build; {len(docs)} parks fetched", file=sys.stderr)
