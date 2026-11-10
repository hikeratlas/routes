# Fixtures

A fixed set of parks to curate on every pull request (`.github/workflows/fixtures.yml`),
so a change shows how it affects speed and routes in minutes, without a
basemap build. `parks.json` says why each park is here: the slow ones, and ones
with routes we've fixed before.

To run them locally, fetch the tiles:

    pip install pmtiles requests
    fixtures/fetch.py /tmp/fx-tiles

It downloads the snapshot the last north-america build froze (as CI uses),
then extracts from the live layers only the parks the snapshot lacks or
whose `bbox` changed. Run it again after editing `parks.json` or
`oracle.json`: a new park costs its own tiles (~30 s), and nothing is
downloaded twice. Then:

    fixtures/run.sh target/release/routes /tmp/fx-tiles /tmp/fx-out/new qrank.db /tmp/dem
    fixtures/run.sh OTHER_BUILD /tmp/fx-tiles /tmp/fx-out/base qrank.db /tmp/dem
    fixtures/compare.py /tmp/fx-out/base /tmp/fx-out/new
    fixtures/expect.py /tmp/fx-out/new

To work on one park, run just it: `ONLY="cape-split" fixtures/run.sh ...`
(keys from `parks.json`, space-separated); a small park takes seconds.

`qrank.db` is from https://github.com/hikeratlas/qrank/releases.

`expect/` holds the named regressions: well-known routes that are very
noticeable when we don't offer them, or name them wrong (e.g. Killarney's
Crack, routes#80, not fixed yet). There is one file per park, named for its
province or state and its name, slugged:
`expect/ontario-killarney-provincial-park.jsonl`. Each line is one whole
entry, `park` and `park_id` included, with the `issue` that tracks it.
`fixtures/expect.py DIR [--json OUT]` checks them on any routes output, by
park id: CI reports them on the fixtures for main and the change, and
basemap's builds on every build, in the build progress check. It reports
and never fails on what it finds, but does fail on a malformed file (bad
JSON, a missing field, two parks in one file, or a file not named
`<region>-<park>.jsonl`). Add one whenever a missing or misnamed route is
noticed.

A park the output doesn't have is checked by routes' `listed.jsonl`, every
park it found in its area and what became of it. If routes found the park
but didn't curate it (under `--min-area`, over `--max-tiles`, or nothing to
offer), its entries fail: nothing is offered there (Cape Split, routes#109).
If the park is the same park mapped twice, its entries are checked on the
twin's routes. Only a park routes didn't find at all is skipped, as outside
the output's area (Killarney in a Nova Scotia build). A park with entries
should be a fixture too (`parks.json`), so CI always runs it.

There are two kinds. A `dest` entry (the default) is one route as we expect
it: to a destination, from a trailhead, under a name. A `trail` entry is a
trail the park itself lists, e.g. on its website (routes#95): its name there
and its length there (`km`). It checks that one hike is offered under that
name, within 30% of that length (`km_tolerance`), and walks at least 70% of
the OSM trail of that name (`trail`, if OSM's name differs). With `--trails`,
routes writes each park's named trails, and the hikes walking each one, to
`trails.jsonl` for that last check; `run.sh` passes it. Without it (e.g.
basemap's builds, which don't need the extra files) only name and length
are checked.

Where OSM splits, forks or under-maps a trail, that last check compares
against the wrong ways. Give the entry a `trace` instead: a rough line of
the hike that's meant, `[[lon, lat], ...]`, a dozen points on it spread
along its length. The named hike of the right length must then pass within
50 m (`trace_within_m`) of at least 70% (`trace_cover`) of those points; it
checks the hike's own line, so it needs no `trails.jsonl` and works on
basemap's builds too. When nothing under the name follows the trace, the
report names the offered hikes that do. The trace's first point is the
trailhead: the same hike must start within 150 m of it (`trace_start_m`;
`null` for a trace that doesn't begin at one), however much of the rest it
covers, so a hike from the wrong lot fails (routes#102). Draw a trace from
the trailhead. With a trace, the length also passes within 0.85-1.4 times
the trace's own length, not just the park's figure (Granite Ridge: the park
says 2 km, the trail 2.8); the report notes a length that passes by the
trace only. A trace's points cut corners, so it's shorter than the hike:
draw one in more detail where that matters (a long trail).

Traces are made on https://www.hikeratlas.com/dev/regressions (website#77):
it shows each proposal (below) with the OSM trail of its name and the hikes
routes offers near it; accept one, or draw the hike along the trails, and
it commits the entry, with the hike simplified to at most 20 points as its
`trace`, to a pull request here.

To see why routes doesn't offer a hike, `routes --candidates` writes every
day hike each park considered, `picked` or not, to `candidates.geojsonl`
(one GeoJSON feature per line, with its park, trailhead, destination, km
and score).

`proposals/` holds the trails parks list that aren't entries yet: one
file per park, named as `expect/`'s are, one trail per line as the park
lists it (its name, the whole hike's length, the park's own words and where
they are), with a `status`: `open`, `accepted` (it's an entry now) or
`rejected`. They wait for a person to promote them, with a trace of the
hike meant, or reject them. `expect.py` fails on a malformed one, as on a
malformed entry. The first 390, for 28 Canadian national and provincial
parks, came from the parks' websites (routes#95).

To add a park's list, write it as
`[{"park_id", "region", "name", "km", "source"}, ...]` (by hand, or from its
page any other way), run routes over the park with `--trails`, and

    fixtures/trail-candidates.py list.json OUT_DIR --write

It matches each name to an OSM trail, appends the drafts to each park's file
(made if need be; names a file already has are left out, so it can be run
again as the list grows), and says on stderr what to review: fuzzy matches,
lengths far from OSM's, trails outside the park, and names with no OSM trail
at all (gaps in the map, not in routes, so not drafted). Without `--write`
it prints the drafts.
