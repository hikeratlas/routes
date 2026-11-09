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

`expect.json` lists named regressions: routes we've fixed and must keep
offering, named right (e.g. Killarney's Crack, routes#80). CI checks them on
the new run with `fixtures/expect.py out/new`, and fails if one is lost. Add
one whenever a missing or misnamed route is fixed.
