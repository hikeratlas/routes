# routes

Curates hikes for every park in a [hikeratlas/basemap](https://github.com/hikeratlas/basemap)
build, so the map is never a blank slate: a handful of good day hikes, base
camps to stay at, and multi-day point-to-points.

It reads the basemap's own z14 vector tiles: trails and roads, land cover, water,
park outlines, peaks and POIs. The routes it finds therefore follow the trails
the map shows.

```bash
cargo build --release
./target/release/routes --tiles north-america.pmtiles --qrank qrank.db --out out/
```

`--tiles` may be given more than once, for example once per layer archive of a
build. While developing, you can point it at the public build and narrow it
down to one park:

```bash
./target/release/routes \
  --tiles https://public.hikeratlas.com/north-america.pmtiles \
  --tile-cache cache/tiles --dem-cache cache/dem --qrank qrank.db \
  --bbox -123.35,49.65,-122.55,50.25 --park garibaldi --out out/
```

The output:

- `out/routes.geojsonl`: one line per route, for a map layer. Each line has
  `id`, `park`, `park_id`, `type` (`day`, `approach`, `from-camp` or
  `multi-day`), `title`, `kind` (`out-and-back`, `loop` or `point-to-point`),
  `km` and `gain`.
- `out/parks/<osm id>.json`: each park's routes in full. This includes the
  stats, the land cover and features passed, the campsites along the way with
  their distance from the start, and `why`, the score breakdown.

## How it picks hikes

It works destination first, and prefers variety.

1. **Parks.** Named protected areas, national parks and nature reserves over
   5 km² (`--min-area`), from the z8 park layer. Each park is curated from its
   z14 tiles, plus 5 km around it.
2. **Trailheads.** Parking lots and `highway=trailhead` within 5 km of the
   park, snapped to the network and merged when within 400 m of each other.
   An unnamed lot is named after the first named trail it leads to.
3. **Destinations.** Peaks, viewpoints, waterfalls, huts, and named lakes over
   2 ha. A lake can be reached from anywhere within 75 m of its shore. Copies
   of one place are merged, such as "Panorama Ridge" and "Panorama Ridge
   overlook".
4. **Candidates.** From each trailhead, a search along trails finds every
   destination 1.5–15 km away. Each gets an out-and-back, plus a loop when
   there's a way back that shares less than 30% of the trail and is at most
   1.6× as long. Roads cost 2×, tracks 1.3× and bike-only trails 1.8× as much
   as trails.
5. **Scoring** (see `score` in `src/curate.rs`).
   - Rewards: the destination's appeal and Wikidata popularity
     ([QRank](https://github.com/hikeratlas/qrank)), the kinds of features
     passed, variety of land cover, time in open alpine terrain, relief, and
     being a loop.
   - Penalties: road walking, bike-only trails, length over 20 km and climbing
     over 1,700 m.
6. **Selection.** Coverage first: each trailhead's best route, if it's mostly
   new trail. Then the rest, by score × novelty, with one route per
   destination. Bigger parks get more routes (3–15).
7. **Base camps.** Backcountry campsites and huts in the park, meaning no road
   within 400 m, at least 2 km from the nearest trailhead. Each comes with the
   walk in and its best four day hikes from camp.
8. **Multi-day point-to-points.** Trailhead to trailhead, 18–60 km, at least
   60% in the park, via two or more backcountry campsites. They aren't split
   into days. The campsites are listed with their distance from the start, so
   hikers can plan their own days.

Routes found by a bigger park are dropped from smaller parks that overlap it.

Elevation comes from the [Terrarium](https://github.com/tilezen/joerd/blob/master/docs/formats.md#terrarium)
DEM, which is the same data the website uses.

## History

This repo used to hold a TypeScript attempt at inferring trails from OSM ways.
It was replaced by this curation approach, which started as a Python prototype
tuned on Garibaldi Provincial Park.
