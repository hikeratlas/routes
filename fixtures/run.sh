#!/bin/bash
# run.sh ROUTES_BINARY TILES_DIR OUT_DIR QRANK_DB DEM_CACHE [LIMIT_SECS] [PARKS_FILE]
#
# Run a routes binary over each fixture (fixtures/parks.json, or PARKS_FILE,
# e.g. fixtures/oracle.json) extracted into TILES_DIR by extract.py, writing
# OUT_DIR/<key>/, <key>.log and <key>.secs for compare.py. Each fixture is
# stopped after LIMIT_SECS (default 600). An entry with "id" picks its park by
# osm_id (names aren't unique); otherwise by "park", a name.
set -uo pipefail
bin=$1 tiles=$2 out=$3 qrank=$4 dem=$5 limit=${6:-600}
here=$(dirname "$(readlink -f "$0")")
list=${7:-$here/parks.json}
mkdir -p "$out"
for key in $(jq -r '.[].key' "$list"); do
  id=$(jq -r --arg k "$key" '.[] | select(.key == $k) | .id // ""' "$list")
  if [ -n "$id" ]; then
    pick=(--park-id "$id")
  else
    pick=(--park "$(jq -r --arg k "$key" '.[] | select(.key == $k) | .park' "$list")")
  fi
  args=()
  for f in "$tiles/$key"/*.pmtiles; do args+=(--tiles "$f"); done
  start=$(date +%s)
  timeout "$limit" "$bin" "${args[@]}" "${pick[@]}" --qrank "$qrank" --dem-cache "$dem" \
    --out "$out/$key" 2> "$out/$key.log"
  status=$?
  echo "$(( $(date +%s) - start )) $status" > "$out/$key.secs"
  echo "$key: $(cat "$out/$key.secs") $(grep -c ' day hikes' "$out/$key.log") parks"
done
