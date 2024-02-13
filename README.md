# routes

Hiking trails on OpenStreetMap are tagged haphazardly. This repo tries to
transform a collection of OSM objects into a canonicalized list of trails.

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
