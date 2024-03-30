# routes

Hiking trails on OpenStreetMap are tagged haphazardly. This repo tries to
transform a collection of OSM objects into a canonicalized list of trails.

or a lasso.

Challenges:

- most trails are not `route=hiking` relation
- many trails consist of multiple ways that need to be joined together
- the start/end points of trails are often not clear

## Input

Clone [hikeratlas/basemap](https://github.com/hikeratlas/basemap) and run:

```bash
./export/export path-to-some-pbf.pbf ztrails
```

This will create a GeoJSONL file of paths, parking lots, lakes and peaks.
This file can then be consumed by this project to emit trails.

## Definitions

A `trail` is a continuous linestring. It may be closed, an out-and-back, or a lasso. It starts at a trailhead. It never doubles back when it could explore a new path. It never visits the same node more than twice.

A `trailhead` is a footpath that intersections a parking lot or highway.

## Strategy

1. Enumerate trailheads.
2. For each trailhead, compute all valid trails from it.
3. Take the "best" trail. TBD how to define what is best.
