# Fixtures

A fixed set of parks to curate on every pull request (`.github/workflows/fixtures.yml`),
so a change shows how it affects speed and routes in minutes, without a
basemap build. `parks.json` says why each park is here: the slow ones, and ones
with routes we've fixed before.

To run them locally:

    pip install pmtiles requests
    fixtures/extract.py https://public.hikeratlas.com/layers/north-america /tmp/fx-tiles

or use the snapshot the last north-america build froze, as CI does:

    curl -s "$(curl -s https://public.hikeratlas.com/fixtures/latest.json | jq -r .url)" | tar xz -C /tmp && mv /tmp/tiles /tmp/fx-tiles

then:

    fixtures/run.sh target/release/routes /tmp/fx-tiles /tmp/fx-out/new qrank.db /tmp/dem
    fixtures/run.sh OTHER_BUILD /tmp/fx-tiles /tmp/fx-out/base qrank.db /tmp/dem
    fixtures/compare.py /tmp/fx-out/base /tmp/fx-out/new

`qrank.db` is from https://github.com/hikeratlas/qrank/releases.

`expect.json` lists named regressions: well-known routes that are very
noticeable when we don't offer them, or name them wrong (e.g. Killarney's
Crack, routes#80, not fixed yet). `fixtures/expect.py DIR [--json OUT]`
checks them on any routes output, by park id: CI reports them on the
fixtures for main and the change, and basemap's builds on every build, in
the build progress check. It reports and never fails. Add one whenever a
missing or misnamed route is noticed.

There are two kinds. A `dest` entry (the default) is one route as we expect
it: to a destination, from a trailhead, under a name. A `trail` entry is a
trail the park itself lists, e.g. on its website (routes#95): its name there
and its length there (`km`). It checks that one hike is offered under that
name, within 30% of that length (`km_tolerance`), and walks at least 70% of
the OSM trail of that name (`trail`, if OSM's name differs). routes writes
each park's named trails, and the hikes walking each one, to `trails.jsonl`
for that last check; older outputs check name and length only.

To add a park's list, write it as `[{"park_id", "name", "km", "source"}, ...]`
(by hand, or from its page any other way) and draft the entries with

    fixtures/trail-candidates.py list.json OUT_DIR > entries.json

on a routes run over the park. It matches each name to an OSM trail and says
on stderr what to review: fuzzy matches, lengths far from OSM's, trails
outside the park, and names with no OSM trail at all (gaps in the map, not
in routes, so not drafted).
