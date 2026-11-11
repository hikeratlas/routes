# routes

Curates hikes for every park in a [hikeratlas/basemap](https://github.com/hikeratlas/basemap)
build, so the map is never a blank slate: a handful of good day hikes, base
camps to stay at, and multi-day point-to-points.

It reads the basemap's own z14 vector tiles: trails and roads, land cover, water,
park outlines, peaks and POIs. The routes it finds therefore follow the trails
the map shows.

Where OSM maps no land cover, it falls back on [ESA WorldCover
2021](https://esa-worldcover.org/) (10 m, CC BY 4.0), which basemap's
`bin/worldcover.py` publishes as z12 tiles (`--landcover`; basemap#167). OSM
often maps a mountain's forest and leaves what's above the trees blank, so
without it most alpine hikes look like they never leave the forest. Anything
shown from it needs WorldCover's attribution.

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
  `km`, `gain` and `key`.
- `out/parks/<osm id>.json`: each park's routes in full. This includes the
  stats, the land cover and features passed, the campsites along the way with
  their distance from the start, and `why`, the score breakdown. `landcover`
  is each class's share of the route, biggest first, then `other`: what no
  class covers (unmapped, or farmland, a town).

`id` (`<park>/day/3`) is a route's position in its park's list, and changes
when a rebuild adds, drops or reorders routes. `key` stays the same as long
as the route does: the park, the route type, its trailhead (or camp) and
destination, and its kind, lower-cased (`2226817|day|rubble creek
trailhead|the black tusk|out-and-back`; a multi-day trip's ends are sorted).
Use it to attach other data to routes, such as the trip reports
[hikeratlas/cc-crawler](https://github.com/hikeratlas/cc-crawler) links to them.

For the same reason the park documents carry OSM ids where the tiles have
them: `dest_osm_ids` (the destination, and copies merged into it),
`from_osm_id` (the trailhead or parking node, or the campsite a from-camp
hike starts at), `camp_osm_id` and `from_osm_id` on base camps, and
`name_osm_id` (the hiking route relation a named route follows). They're
left out when unknown: a start where a trail meets a road has no OSM
feature, and a destination's layer may carry no `osm_id`.

## How it picks hikes

It works destination first, and prefers variety.

1. **Parks.** Named protected areas, national parks and nature reserves over
   5 km² (`--min-area`), from the z8 park layer. Parks closed to the public
   (`access=private` or `no`, such as Vancouver's watersheds) are skipped. Each park is curated from its
   z14 tiles, plus 5 km around it. Parks that would need more than 50,000
   tiles (`--max-tiles`) are skipped for now, because their tiles would all be
   in memory at once.
2. **Trailheads.** Parking lots and `highway=trailhead` within 5 km of the
   park, snapped to the network and merged when within 400 m of each other.
   An unnamed lot is named after the first named trail it leads to. The
   basemap only has parking lots inside parks, so places where a trail meets a
   drivable road within 2 km of the park count too. That catches lots just
   outside the boundary and unmapped trailheads. Each such place must be at
   least 1 km from every other trailhead.
3. **Destinations.** Peaks, viewpoints, waterfalls, huts, and named lakes over
   2 ha. A lake can be reached from anywhere within 75 m of its shore. Copies
   of one place are merged, such as "Panorama Ridge" and "Panorama Ridge
   overlook".
4. **Candidates.** From each trailhead, a search along trails finds every
   destination 1.5–15 km away. Each gets an out-and-back, plus a loop when
   there's a way back that shares less than 30% of the trail and is at most
   1.6× as long. Roads cost 2×, tracks 1.3× and bike-only trails 1.8× as much
   as trails. A trail is bike-only if it's designated for bikes or graded for
   mountain bikes (`mtb:scale`), and walking isn't designated too.
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
