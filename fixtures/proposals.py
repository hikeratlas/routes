# Proposed named regressions: fixtures/proposals/<region>-<park>.jsonl, one
# file per park (named as fixtures/expect/'s are), one trail per line, as the
# park itself lists it (e.g. on its website). Each waits to be promoted to an
# entry in fixtures/expect/ (a `trail` entry, with a `trace` of the hike
# meant), or rejected; the website's /dev/regressions page does both.
#
# A line: park, park_id, region, name, km (the whole hike as walked:
# out-and-back lengths doubled), source (where the park lists it), status
# ("open", "accepted" or "rejected"), and optionally quote (the park's words),
# km_basis ("return", "one-way-doubled", "loop", "point-to-point" or
# "unclear"), why, hours, elev_m, issue, park_bbox [W, S, E, N], osm_trail
# (the OSM trail of its name, if one matched, spelled as in OSM), osm_bbox (the
# box of its ways, for the website to read them from the tiles), osm_km, suggest_km and
# suggest_why (a length to consider instead, and why), note, near and
# near_bbox (with no osm_bbox: the place its name goes to, as OSM names it,
# and a box a few km around it, for the website to frame; locate-proposals.py).
import glob, json, os

from expectations import slug

DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'proposals')
STATUSES = ('open', 'accepted', 'rejected')
BASES = ('return', 'one-way-doubled', 'loop', 'point-to-point', 'unclear')


def file_for(region, park):
    return os.path.join(DIR, f'{slug(region)}-{slug(park)}.jsonl')


def load(d=DIR):
    """Every proposal, each with `file` (its file's name, no .jsonl). Raises
    SystemExit naming every malformed line or misnamed file."""
    out, errors = [], []
    for path in sorted(glob.glob(os.path.join(d, '*.jsonl'))):
        stem = os.path.basename(path)[:-len('.jsonl')]
        ids, keys = set(), set()
        for n, line in enumerate(open(path, encoding='utf-8'), 1):
            if not line.strip():
                continue
            where = f'proposals/{stem}.jsonl:{n}'
            try:
                e = json.loads(line)
            except json.JSONDecodeError as x:
                errors.append(f'{where}: not JSON ({x})')
                continue
            missing = [k for k in ('park', 'park_id', 'region', 'name', 'km', 'source', 'status') if e.get(k) in (None, '')]
            if missing:
                errors.append(f"{where}: no {', '.join(missing)}")
                continue
            if e['status'] not in STATUSES:
                errors.append(f"{where}: status {e['status']!r} isn't one of {', '.join(STATUSES)}")
            if e.get('km_basis') and e['km_basis'] not in BASES:
                errors.append(f"{where}: km_basis {e['km_basis']!r} isn't one of {', '.join(BASES)}")
            if not isinstance(e['km'], (int, float)) or e['km'] <= 0:
                errors.append(f"{where}: km must be a positive number")
            e['park_id'] = str(e['park_id'])
            ids.add(e['park_id'])
            if stem != f"{slug(e['region'])}-{slug(e['park'])}":
                errors.append(f"{where}: {e['park']} belongs in {slug(e['region'])}-{slug(e['park'])}.jsonl")
            # A park can list two options under one name (Skyline, a loop or
            # out and back): name and length together are what's unique.
            key = (e['name'], e['km'])
            if key in keys:
                errors.append(f"{where}: {e['name']} ({e['km']} km) is listed twice")
            keys.add(key)
            out.append(e | {'file': stem})
        if len(ids) > 1:
            errors.append(f"proposals/{stem}.jsonl: more than one park ({', '.join(sorted(ids))}); one file per park")
    if errors:
        raise SystemExit('fixtures/proposals: ' + '\n  '.join(['malformed:', *errors]))
    return out
