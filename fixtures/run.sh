#!/bin/bash
# run.sh ROUTES_BINARY TILES_DIR OUT_DIR QRANK_DB DEM_CACHE [LIMIT_SECS] [PARKS_FILE]
#
# Run a routes binary over each fixture (fixtures/parks.json, or PARKS_FILE,
# e.g. fixtures/oracle.json) extracted into TILES_DIR by extract.py, writing
# OUT_DIR/<key>/, <key>.log and <key>.secs for compare.py. Each fixture is
# stopped after LIMIT_SECS (default 600). An entry with "id" picks its park by
# osm_id (names aren't unique); otherwise by "park", a name. ROUTES_ARGS, if
# set, is passed on to routes (e.g. --no-known-bonus); so is --trails, for
# fixtures/expect.py, if the build has it. ONLY, if set, runs just those
# fixtures (keys, space-separated), e.g. ONLY="cape-split killarney".
set -uo pipefail
bin=$1 tiles=$2 out=$3 qrank=$4 dem=$5 limit=${6:-600}
here=$(dirname "$(readlink -f "$0")")
list=${7:-$here/parks.json}
mkdir -p "$out"
# ROUTES_ARGS options this build doesn't know (an older main) are dropped.
extra=()
usage=$("$bin" 2>&1 || true)
for a in ${ROUTES_ARGS:-}; do
  if grep -q -- "$a" <<< "$usage"; then extra+=("$a"); else echo "run.sh: $bin has no $a, running without it" >&2; fi
done
# Each park's named trails, for the named regressions (expect.py), where the
# build has them (routes#95).
if grep -q -- "--trails" <<< "$usage"; then extra+=(--trails); fi
keys=$(jq -r '.[].key' "$list")
for k in ${ONLY:-}; do
  grep -qx -- "$k" <<< "$keys" || { echo "run.sh: no fixture $k in $list" >&2; exit 2; }
done
for key in $keys; do
  if [ -n "${ONLY:-}" ] && [[ " $ONLY " != *" $key "* ]]; then continue; fi
  id=$(jq -r --arg k "$key" '.[] | select(.key == $k) | .id // ""' "$list")
  if [ -n "$id" ]; then
    pick=(--park-id "$id")
  else
    pick=(--park "$(jq -r --arg k "$key" '.[] | select(.key == $k) | .park' "$list")")
  fi
  args=()
  for f in "$tiles/$key"/*.pmtiles; do args+=(--tiles "$f"); done
  start=$(date +%s)
  timeout "$limit" "$bin" "${args[@]}" "${pick[@]}" "${extra[@]}" --qrank "$qrank" --dem-cache "$dem" \
    --out "$out/$key" 2> "$out/$key.log"
  status=$?
  echo "$(( $(date +%s) - start )) $status" > "$out/$key.secs"
  echo "$key: $(cat "$out/$key.secs") $(grep -c ' day hikes' "$out/$key.log") parks"
done
