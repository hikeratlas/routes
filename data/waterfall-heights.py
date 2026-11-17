#!/usr/bin/env python3
"""Write waterfall-heights.tsv: each waterfall on Wikidata with a height
(P2048), its id and height in metres (the tallest, if it has several).

routes builds the table in (src/area.rs, waterfall_heights) to reach a tall
waterfall from further away: it's seen from its foot, which can be its
height from where OSM maps it (Takakkaw Falls, 302 m: the path ends 337 m
from its point). Run it again to refresh the table:

    python3 data/waterfall-heights.py > data/waterfall-heights.tsv
"""
import json
import sys
import urllib.parse
import urllib.request

QUERY = """
SELECT ?w (MAX(?h) AS ?height) WHERE {
  ?w wdt:P31/wdt:P279* wd:Q34038 ;
     p:P2048/psn:P2048/wikibase:quantityAmount ?h .
} GROUP BY ?w
"""


def main():
    url = "https://query.wikidata.org/sparql?" + urllib.parse.urlencode({"query": QUERY})
    req = urllib.request.Request(url, headers={
        "Accept": "application/sparql-results+json",
        "User-Agent": "hikeratlas-routes/1.0 (waterfall heights)",
    })
    with urllib.request.urlopen(req, timeout=120) as r:
        rows = json.load(r)["results"]["bindings"]
    out = {}
    for b in rows:
        qid = b["w"]["value"].rsplit("/", 1)[1]
        h = float(b["height"]["value"])
        # Normalized to metres (psn:); a few heights are nonsense (0, or
        # kilometres entered as metres): keep the plausible ones.
        if 1 <= h <= 1000:
            out[qid] = h
    w = sys.stdout
    w.write("wikidata\theight_m\n")
    for qid in sorted(out, key=lambda q: int(q[1:])):
        w.write(f"{qid}\t{out[qid]:g}\n")
    print(f"{len(out)} waterfalls with a height", file=sys.stderr)


if __name__ == "__main__":
    main()
