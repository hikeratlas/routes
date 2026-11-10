#!/usr/bin/env python3
# fetch.py OUT_DIR [--layers BASE_URL] [--snapshot-only]
#
# The fixture tiles in OUT_DIR, as run.sh reads them: OUT_DIR/<key>/ for
# parks.json, OUT_DIR/oracle/<key>/ for oracle.json. Starts from the snapshot
# the last north-america build froze (fixtures/latest.json), downloaded only
# when OUT_DIR doesn't have that one yet, then extracts from the live layers
# (BASE_URL, default north-america's) only the parks the snapshot lacks or
# whose bbox changed: adding a park costs its own tiles, ~30 s, not every
# fixture's. Run it again after editing parks.json or oracle.json.
#
# A park's tiles are current when its parks.pmtiles was cut from its bbox
# (extract.py writes the box into the archive's header). Live tiles may be
# newer than the snapshot's; a comparison stays fair because both builds run
# on the same OUT_DIR.
#
# Needs: pip install pmtiles requests
import io, json, os, shutil, sys, tarfile

import requests
from pmtiles.reader import MmapSource, Reader

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, here)
import extract  # noqa: E402

LATEST = 'https://public.hikeratlas.com/fixtures/latest.json'
LAYERS = 'https://public.hikeratlas.com/layers/north-america'


def current(d, bbox):
    """Whether d/ holds tiles cut from bbox."""
    try:
        with open(os.path.join(d, 'parks.pmtiles'), 'rb') as f:
            h = Reader(MmapSource(f)).header()
    except (OSError, ValueError):
        return False
    want = [int(v * 1e7) for v in bbox]
    have = [h['min_lon_e7'], h['min_lat_e7'], h['max_lon_e7'], h['max_lat_e7']]
    return all(abs(a - b) <= 1 for a, b in zip(want, have))


def snapshot(out):
    """Download and unpack the latest snapshot into out/, unless it's there."""
    latest = extract.retried(lambda: requests.get(LATEST, timeout=60)).json()
    marker = os.path.join(out, '.snapshot')
    if os.path.exists(marker) and open(marker).read().strip() == str(latest['id']):
        print(f"snapshot {latest['id']} already in {out}")
        return
    print(f"downloading snapshot {latest['id']} (built {latest.get('built')})", flush=True)
    r = extract.retried(lambda: requests.get(latest['url'], timeout=600))
    r.raise_for_status()
    with tarfile.open(fileobj=io.BytesIO(r.content), mode='r:gz') as tar:
        members = []
        for m in tar.getmembers():
            # The archive holds tiles/<key>/... and tiles/oracle/<key>/...
            if not m.name.startswith('tiles/') or not (m.isfile() or m.isdir()):
                continue
            m.name = m.name[len('tiles/'):]
            if m.name:
                members.append(m)
        tar.extractall(out, members=members, filter='data')
    with open(marker, 'w') as f:
        f.write(str(latest['id']))


def main():
    args = sys.argv[1:]
    layers = LAYERS
    if '--layers' in args:
        i = args.index('--layers')
        layers = args[i + 1].rstrip('/')
        del args[i:i + 2]
    snapshot_only = '--snapshot-only' in args
    if snapshot_only:
        args.remove('--snapshot-only')
    out = args[0]
    os.makedirs(out, exist_ok=True)
    snapshot(out)
    if snapshot_only:
        return
    stale = []
    for name, sub in (('parks.json', ''), ('oracle.json', 'oracle')):
        for p in json.load(open(os.path.join(here, name))):
            d = os.path.join(out, sub, p['key'])
            if not current(d, p['bbox']):
                stale.append((p, d))
    if not stale:
        print('every fixture is in the snapshot')
        return
    print(f"{len(stale)} parks not in the snapshot (or their bbox changed): "
          f"{', '.join(p['key'] for p, _ in stale)}", flush=True)
    for p, d in stale:
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        for layer in extract.LAYERS:
            url = f'{layers}/{layer}.pmtiles'
            if layer in extract.OPTIONAL and not extract.exists(url):
                continue
            n, spans, size = extract.extract(url, p['bbox'], os.path.join(d, f'{layer}.pmtiles'))
            print(f"  {p['key']}/{layer}: {n} tiles, {size / 1e6:.1f} MB", flush=True)


if __name__ == '__main__':
    main()
