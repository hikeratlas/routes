# The named regressions: fixtures/expect/<region>-<park>.jsonl, one file
# per park, one expectation per line (see expect.py for the kinds). The file
# is named for the park's region and its name, slugged:
# "ontario-killarney-provincial-park.jsonl" for Killarney Provincial Park.
# Each line is a whole entry, park and park_id included, so lines can be
# appended, sorted or moved without the rest of the file.
import glob, json, os, re, unicodedata

DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'expect')


def slug(s):
    """'Lac-Mégantic Provincial Park' -> 'lac-megantic-provincial-park'."""
    s = unicodedata.normalize('NFKD', s).encode('ascii', 'ignore').decode()
    return re.sub(r'[^a-z0-9]+', '-', s.lower()).strip('-')


def norm(s):
    """A name to compare: 'The "Crack" Trail' and 'the crack' are the same."""
    words = re.sub(r'[^0-9a-z]+', ' ', (s or '').lower()).split()
    return ' '.join(w for w in words if w not in ('the', 'trail'))


def file_for(region, park):
    return os.path.join(DIR, f'{slug(region)}-{slug(park)}.jsonl')


def load(d=DIR):
    """Every expectation, each with `file` (its file's name, no .jsonl), in
    file then line order. Raises SystemExit naming every malformed line or
    misnamed file: a mistake here should fail loudly, not skip a check."""
    out, errors = [], []
    for path in sorted(glob.glob(os.path.join(d, '*.jsonl'))):
        stem = os.path.basename(path)[:-len('.jsonl')]
        ids, names = set(), set()
        for n, line in enumerate(open(path, encoding='utf-8'), 1):
            if not line.strip():
                continue
            where = f'{stem}.jsonl:{n}'
            try:
                e = json.loads(line)
            except json.JSONDecodeError as x:
                errors.append(f'{where}: not JSON ({x})')
                continue
            missing = [k for k in ('name', 'park', 'park_id', 'issue') if not e.get(k)]
            if missing:
                errors.append(f"{where}: no {', '.join(missing)}")
                continue
            e['park_id'] = str(e['park_id'])
            ids.add(e['park_id'])
            p = slug(e['park'])
            if not (stem.endswith('-' + p) and len(stem) > len(p) + 1):
                errors.append(f"{where}: {e['park']} belongs in <region>-{p}.jsonl, not {stem}.jsonl")
            key = (e.get('kind', 'dest'), e['name'])
            if key in names:
                errors.append(f"{where}: a second {key[0]} entry named {e['name']}")
            names.add(key)
            out.append(e | {'file': stem})
        if len(ids) > 1:
            errors.append(f"{stem}.jsonl: more than one park ({', '.join(sorted(ids))}); one file per park")
    if errors:
        raise SystemExit('fixtures/expect: ' + '\n  '.join(['malformed:', *errors]))
    return out
