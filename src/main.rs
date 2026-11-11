// Curate hiking routes for every park in a basemap build.
//
//   routes --tiles LAYER.pmtiles [--tiles ...] --out DIR [options]
//
// Reads the basemap's z14 tiles (path, road, land, water_poly, park, peak and
// poi layers; they may be spread over several archives) and writes
//   DIR/routes.geojsonl       one line per route, for the map
//   DIR/parks/<osm_id>.json   each park's routes in full, for its page
//   DIR/routes.bin, DIR/index.json
//                             the same, as one file for the website: each
//                             park's JSON gzipped, end to end, and where each
//                             one is ({"bundle": "routes.bin", "parks":
//                             {"<osm_id>": [offset, length]}}), so a build
//                             uploads 2 files, not one per park
//   DIR/listed.jsonl          every park found in the area and what became of
//                             it (written, a twin, inside a bigger park,
//                             no routes, under --min-area, over
//                             --max-tiles), so a check of a
//                             park's routes can tell one that isn't curated
//                             from one outside the area (fixtures/expect.py)
//
// A big area can be curated on several machines: each runs with --shard K/N,
// and then one runs --merge over their output directories.
mod area;
mod curate;
mod dem;
mod geo;
mod mvt;
mod pmtiles;
mod recall;

use area::Area;
use pmtiles::PmTiles;
use rayon::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

const USAGE: &str = "usage: routes --tiles FILE|URL [--tiles ...] --out DIR
  [--park NAME]          only parks whose name contains NAME
  [--park-id ID]         only the park (or trail area) with this osm_id
  [--bbox W,S,E,N]       only parks in this box (default: the bounds of the
                         archives with parks or trail areas)
  [--min-area KM2]       smallest park to curate (default 5)
  [--min-area-notable KM2]
                         smallest notable park to curate: one with a
                         popular Wikidata item or named a national,
                         provincial or state park (default 1)
  [--max-tiles N]        skip parks needing more z14 tiles than this (default 50000)
  [--qrank qrank.db]     Wikidata popularity (hikeratlas/qrank)
  [--tile-cache DIR]     cache tiles fetched from a URL here
  [--dem-cache DIR]      cache DEM tiles here
  [--dem-zoom Z]         DEM zoom (default 12)
  [--nl-dem URL|FILE]    nl-dem's PMTiles of Newfoundland's 5 m DEM, used
                         before Mapterhorn there (default the published
                         archive; none for Mapterhorn alone)
  [--no-known-bonus]     don't score up routes that follow an OSM hiking
                         route (to measure the proposer on its own)
  [--trails]             also write DIR/trails.jsonl: each park's named
                         trails and the hikes walking each, for the named
                         regressions (fixtures/expect.py); off by default
  [--candidates]         also write DIR/candidates.geojsonl: every day hike
                         each park considered, `picked` or not, for the
                         named regressions (why a hike isn't offered); off by default
  [--park-timeout SECS]  stop starting new searches in a park after this
                         long, keeping what was found, and report it
  [--shard K/N]          curate only shard K (0-based) of N, and write each
                         park's routes to DIR/raw/ for a later --merge,
                         instead of the outputs below
  [--shard-costs FILE]   balance --shard by these seconds per park (a
                         --merge's timings.json) instead of by tile count
  [--write-region FILE]  write the z14 tiles the parks (of --shard) need as
                         a GeoJSON MultiPolygon, and exit: for `pmtiles
                         extract --region`, so a shard can fetch just those

   or: routes --merge DIR [--merge DIR...] --out DIR
  merge --shard runs' DIR/raw/ into the outputs below, and DIR/timings.json:
  each park's curate seconds, for a later --shard-costs";

struct Args {
    tiles: Vec<String>,
    out: PathBuf,
    park: Option<String>,
    park_id: Option<String>,
    bbox: Option<[f64; 4]>,
    min_area: f64,
    min_area_notable: f64,
    max_tiles: usize,
    qrank: Option<String>,
    tile_cache: Option<PathBuf>,
    dem_cache: Option<PathBuf>,
    dem_zoom: u8,
    nl_dem: Option<String>,
    shard: Option<(usize, usize)>,
    shard_costs: Option<PathBuf>,
    merge: Vec<PathBuf>,
    park_timeout: Option<f64>,
    write_region: Option<PathBuf>,
}

fn args() -> Args {
    let mut a = Args {
        tiles: vec![],
        out: PathBuf::new(),
        park: None,
        park_id: None,
        bbox: None,
        min_area: 5.0,
        min_area_notable: 1.0,
        max_tiles: 50000,
        qrank: None,
        tile_cache: None,
        dem_cache: None,
        dem_zoom: 12,
        nl_dem: Some(dem::NL_DEM.to_string()),
        shard: None,
        shard_costs: None,
        merge: vec![],
        park_timeout: None,
        write_region: None,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let die = || -> ! {
        eprintln!("{USAGE}");
        std::process::exit(2)
    };
    while i < v.len() {
        if v[i] == "--no-known-bonus" {
            curate::KNOWN_BONUS.store(false, std::sync::atomic::Ordering::Relaxed);
            i += 1;
            continue;
        }
        if v[i] == "--candidates" {
            curate::CANDIDATES.store(true, std::sync::atomic::Ordering::Relaxed);
            i += 1;
            continue;
        }
        if v[i] == "--trails" {
            recall::TRAILS.store(true, std::sync::atomic::Ordering::Relaxed);
            i += 1;
            continue;
        }
        let val = v.get(i + 1).cloned().unwrap_or_else(|| die());
        match v[i].as_str() {
            "--tiles" => a.tiles.push(val),
            "--out" => a.out = val.into(),
            "--park" => a.park = Some(val.to_lowercase()),
            "--park-id" => a.park_id = Some(val),
            "--bbox" => {
                let b: Vec<f64> = val
                    .split(',')
                    .map(|x| x.parse().unwrap_or_else(|_| die()))
                    .collect();
                a.bbox = Some(b.try_into().unwrap_or_else(|_| die()));
            }
            "--min-area" => a.min_area = val.parse().unwrap_or_else(|_| die()),
            "--min-area-notable" => a.min_area_notable = val.parse().unwrap_or_else(|_| die()),
            "--max-tiles" => a.max_tiles = val.parse().unwrap_or_else(|_| die()),
            "--qrank" => a.qrank = Some(val),
            "--tile-cache" => a.tile_cache = Some(val.into()),
            "--write-region" => a.write_region = Some(val.into()),
            "--dem-cache" => a.dem_cache = Some(val.into()),
            "--dem-zoom" => a.dem_zoom = val.parse().unwrap_or_else(|_| die()),
            "--nl-dem" => a.nl_dem = if val == "none" { None } else { Some(val) },
            "--shard" => {
                let (k, n) = val.split_once('/').unwrap_or_else(|| die());
                let (k, n): (usize, usize) = (
                    k.parse().unwrap_or_else(|_| die()),
                    n.parse().unwrap_or_else(|_| die()),
                );
                if n == 0 || k >= n {
                    die();
                }
                a.shard = Some((k, n));
            }
            "--shard-costs" => a.shard_costs = Some(val.into()),
            "--merge" => a.merge.push(val.into()),
            "--park-timeout" => a.park_timeout = Some(val.parse().unwrap_or_else(|_| die())),
            _ => die(),
        }
        i += 2;
    }
    if a.out.as_os_str().is_empty()
        || (a.merge.is_empty() == a.tiles.is_empty())
        || (!a.merge.is_empty() && a.shard.is_some())
    {
        die();
    }
    a
}

struct Park {
    id: String,
    name: String,
    /// Its official website and operator ("Ontario Parks"), from its label
    /// in the basemap's park_name layer (hikeratlas/website#72), if any.
    website: String,
    operator: String,
    area: f64,
    trail_area: bool,
    /// Curated only as notable, under --min-area (see notable()).
    small_notable: bool,
    /// Under --min-area-notable too: curated only if an OSM hiking route
    /// runs through it (see ROUTE_IN_SMALL_PARK_M).
    needs_route: bool,
    min: geo::P,
    max: geo::P,
    /// Its outline in the z8 park layer, piece by piece (each tile's), for
    /// whether a small park is inside it.
    rings: Vec<Vec<geo::P>>,
}

/// A small notable park is another's when that one is at least this many
/// times bigger and has it inside: more than a twin (the same outline,
/// mapped twice, as Cape Split is).
const INSIDE_RATIO: f64 = 2.0;
/// How near a unit named for a bigger park (see Park::has_inside) must be.
const UNIT_NEAR_M: f64 = 10_000.0;

impl Park {
    /// Whether park q is inside this one: its box's centre, at least half of
    /// its outline's points (an irregular unit's centre can fall outside
    /// it), or it's a nearby unit named for this one.
    fn has_inside(&self, q: &Park) -> bool {
        let c = ((q.min.0 + q.max.0) / 2.0, (q.min.1 + q.max.1) / 2.0);
        if self.contains(c) {
            return true;
        }
        // A unit named for it, nearby though outside its outline:
        // "Patapsco Valley State Park Rockhaven Area" is a piece of the
        // state park that OSM maps apart from its main relation.
        let near = UNIT_NEAR_M / geo::mpu(c.1);
        if q.name.starts_with(&format!("{} ", self.name))
            && c.0 > self.min.0 - near
            && c.0 < self.max.0 + near
            && c.1 > self.min.1 - near
            && c.1 < self.max.1 + near
        {
            return true;
        }
        let pts: Vec<geo::P> = q.rings.iter().flatten().copied().collect();
        !pts.is_empty() && 2 * pts.iter().filter(|&&p| self.contains(p)).count() >= pts.len()
    }

    /// Whether p is inside the park's outline (even-odd over its pieces).
    fn contains(&self, p: geo::P) -> bool {
        if p.0 < self.min.0 || p.0 > self.max.0 || p.1 < self.min.1 || p.1 > self.max.1 {
            return false;
        }
        let mut inside = false;
        for r in &self.rings {
            for k in 0..r.len() {
                let (a, b) = (r[k], r[(k + 1) % r.len()]);
                if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// The park's z14 tiles, plus 5 km around it for trailheads outside.
    fn tiles(&self) -> Vec<(u32, u32)> {
        let r = 5000.0 / geo::mpu((self.min.1 + self.max.1) / 2.0);
        let (x0, y0) = geo::tile_of((self.min.0 - r, self.min.1 - r));
        let (x1, y1) = geo::tile_of((self.max.0 + r, self.max.1 + r));
        (x0..=x1)
            .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
            .collect()
    }
}

/// The z14 tiles PARKS need (Park::tiles; parks over MAX_TILES are skipped
/// when curating, so they're left out), as a GeoJSON MultiPolygon of one
/// box per park. Each box is pulled in a little from its tiles' edges, so a
/// tile cover of it (pmtiles extract) doesn't take in the neighbours.
fn write_region(path: &std::path::Path, parks: &[Park], max_tiles: usize) {
    let edge = |x: u32, y: u32| geo::lonlat((x as f64 * geo::EXT, y as f64 * geo::EXT));
    let inset = 1e-6;
    let mut boxes = vec![];
    for p in parks {
        let tiles = p.tiles();
        if tiles.is_empty() || tiles.len() > max_tiles {
            continue;
        }
        let (x0, y0) = tiles.iter().fold((u32::MAX, u32::MAX), |m, t| (m.0.min(t.0), m.1.min(t.1)));
        let (x1, y1) = tiles.iter().fold((0, 0), |m, t| (m.0.max(t.0), m.1.max(t.1)));
        let (w, n) = edge(x0, y0);
        let (e, s) = edge(x1 + 1, y1 + 1);
        let (w, s, e, n) = (w + inset, s + inset, e - inset, n - inset);
        boxes.push(json!([[[w, s], [e, s], [e, n], [w, n], [w, s]]]));
    }
    let doc = json!({"type": "MultiPolygon", "coordinates": boxes});
    std::fs::write(path, doc.to_string()).expect("writing --write-region");
    eprintln!("{} parks' tiles written to {}", boxes.len(), path.display());
}

/// Whether an archive may have parks or trail areas (one without metadata
/// may have anything).
fn has_parks(ar: &PmTiles) -> bool {
    ar.layers.is_empty() || ar.layers.iter().any(|l| l == "park" || l == "trail_area")
}

/// Parks, from the z8 tiles' park layer (which has every park over ~0.5 km²),
/// and those left out for being under --min-area, as listed.jsonl entries.
fn parks(archives: &[PmTiles], bbox: [f64; 4], a: &Args) -> (Vec<Park>, Vec<Value>) {
    let z = 8u8;
    let n = (1u32 << z) as f64;
    let tx = |lon: f64| ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0) as u32;
    let ty = |lat: f64| {
        ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n)
            .floor()
            .clamp(0.0, n - 1.0) as u32
    };
    let tiles: Vec<(u32, u32)> = (tx(bbox[0])..=tx(bbox[2]))
        .flat_map(|x| (ty(bbox[3])..=ty(bbox[1])).map(move |y| (x, y)))
        .collect();
    let found: Mutex<HashMap<String, Park>> = Mutex::new(HashMap::new());
    // Parks under --min-area: id -> (name, area).
    let small: Mutex<HashMap<String, (String, f64)>> = Mutex::new(HashMap::new());
    // Each park's label: its website and operator, by osm_id.
    let labels: Mutex<HashMap<String, (String, String)>> = Mutex::new(HashMap::new());
    let scale = geo::EXT * 64.0; // world units per z8 tile
    // Only the archives with parks or trail areas: the others' z8 tiles
    // needn't be read (nor fetched, for a --write-region extract).
    let listing: Vec<&PmTiles> = archives
        .iter()
        .filter(|ar| has_parks(ar))
        .collect();
    // OSM hiking routes, for small parks (needs_route): at z14, as the z8
    // tiles leave short local routes out (the Grouse Grind).
    let route_archives: Vec<&PmTiles> = archives
        .iter()
        .filter(|ar| ar.layers.iter().any(|l| l == "hiking_route"))
        .collect();
    tiles.par_iter().for_each(|&(x, y)| {
        for ar in &listing {
            let Some(b) = ar.get(z, x, y).expect("reading tile") else {
                continue;
            };
            for l in mvt::decode(&b) {
                // Parks, and trail areas: trail networks outside parks that the
                // basemap picked out (bin/trail-areas.py), e.g. the Skerwink
                // Trail. Those are already filtered, so skip --min-area for them.
                if l.name == "park_name" {
                    let mut m = labels.lock().unwrap();
                    for f in &l.features {
                        let (w, o) = (l.str(f, "website"), l.str(f, "operator"));
                        if !w.is_empty() || !o.is_empty() {
                            m.insert(l.str(f, "osm_id").to_string(), (w.to_string(), o.to_string()));
                        }
                    }
                    continue;
                }
                let trail_area = l.name == "trail_area";
                if l.name != "park" && !trail_area {
                    continue;
                }
                for f in &l.features {
                    let boundary = l.str(f, "boundary");
                    if f.typ != 3
                        || !(trail_area
                            || boundary == "national_park"
                            || boundary == "protected_area"
                            || l.str(f, "leisure") == "nature_reserve")
                    {
                        continue;
                    }
                    // Closed to the public, e.g. Vancouver's watersheds.
                    let access = l.str(f, "access");
                    if access == "private" || access == "no" {
                        continue;
                    }
                    let area = l.get(f, "area").map(|v| v.as_f64()).unwrap_or(0.0);
                    let name = l.str(f, "name");
                    if name.is_empty()
                        || a.park
                            .as_ref()
                            .is_some_and(|p| !name.to_lowercase().contains(p))
                        || a.park_id
                            .as_ref()
                            .is_some_and(|id| l.str(f, "osm_id") != id)
                    {
                        continue;
                    }
                    let rank = l.get(f, "rank").map(|v| v.as_f64()).unwrap_or(1.0);
                    let floor = if notable(name, rank) { a.min_area_notable } else { a.min_area };
                    let small_notable = !trail_area && area < a.min_area * 1e6;
                    let needs_route = !trail_area && area < floor * 1e6;
                    let mut m = found.lock().unwrap();
                    let p = m
                        .entry(l.str(f, "osm_id").to_string())
                        .or_insert_with(|| Park {
                            id: l.str(f, "osm_id").to_string(),
                            name: name.to_string(),
                            website: String::new(),
                            operator: String::new(),
                            area,
                            trail_area,
                            small_notable,
                            needs_route,
                            min: (f64::MAX, f64::MAX),
                            max: (f64::MIN, f64::MIN),
                            rings: vec![],
                        });
                    for ring in f.geometry() {
                        let mut r = vec![];
                        for (px, py) in ring {
                            let w = (
                                x as f64 * scale + px as f64 * scale / l.extent as f64,
                                y as f64 * scale + py as f64 * scale / l.extent as f64,
                            );
                            p.min = (p.min.0.min(w.0), p.min.1.min(w.1));
                            p.max = (p.max.0.max(w.0), p.max.1.max(w.1));
                            r.push(w);
                        }
                        p.rings.push(r);
                    }
                }
            }
        }
    });
    let labels = labels.into_inner().unwrap();
    let mut v: Vec<Park> = found
        .into_inner()
        .unwrap()
        .into_values()
        .map(|mut p| {
            if let Some((w, o)) = labels.get(&p.id) {
                (p.website, p.operator) = (w.clone(), o.clone());
            }
            p
        })
        .collect();
    v.sort_by(|a, b| b.area.partial_cmp(&a.area).unwrap());
    // A park under the minimums is curated if an OSM hiking route runs
    // through it: a trail is evidence the park is for walking, whatever its
    // size. Grouse Mountain Regional Park (0.73 km², routes#163) is the
    // Grouse Grind's.
    let (v, too_small): (Vec<Park>, Vec<Park>) = v
        .into_iter()
        .partition(|p| {
            !p.needs_route || route_m_inside(p, &route_lines(p, &route_archives)) >= ROUTE_IN_SMALL_PARK_M
        });
    {
        let mut m = small.lock().unwrap();
        for p in too_small {
            m.insert(p.id.clone(), (p.name.clone(), p.area));
        }
    }
    let mut small: Vec<Value> = small
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|(id, (name, area))| listed(&id, &name, area, "under --min-area"))
        .collect();
    // A small notable park inside a bigger one is that park's to offer:
    // Patapsco Valley State Park's units (McKeldin Area, 1.6 km²) are each
    // named a state park, and would repeat its hikes.
    let inside: Vec<(usize, usize)> = (0..v.len())
        .filter(|&i| v[i].small_notable)
        .filter_map(|i| {
            (0..v.len())
                .find(|&j| j != i && v[j].area > INSIDE_RATIO * v[i].area && v[j].has_inside(&v[i]))
                .map(|j| (i, j))
        })
        .collect();
    for &(i, j) in &inside {
        let mut l = listed(&v[i].id, &v[i].name, v[i].area, "inside");
        l["in"] = json!(v[j].id);
        small.push(l);
    }
    let drop: std::collections::HashSet<usize> = inside.iter().map(|&(i, _)| i).collect();
    let v: Vec<Park> = v
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop.contains(i))
        .map(|(_, p)| p)
        .collect();
    small.sort_by(|x, y| x["id"].as_str().cmp(&y["id"].as_str()));
    (v, small)
}

/// The OSM hiking routes' lines over a (small) park, from its z14 tiles.
fn route_lines(p: &Park, archives: &[&PmTiles]) -> Vec<Vec<geo::P>> {
    let ext = geo::EXT;
    let (x0, y0) = ((p.min.0 / ext) as u32, (p.min.1 / ext) as u32);
    let (x1, y1) = ((p.max.0 / ext) as u32, (p.max.1 / ext) as u32);
    let mut out = vec![];
    if (x1 - x0 + 1) * (y1 - y0 + 1) > 400 {
        return out; // not a small park
    }
    for ar in archives {
        for x in x0..=x1 {
            for y in y0..=y1 {
                let Some(b) = ar.get(14, x, y).expect("reading tile") else {
                    continue;
                };
                for l in mvt::decode(&b) {
                    if l.name != "hiking_route" {
                        continue;
                    }
                    for f in l.features.iter().filter(|f| f.typ == 2) {
                        for line in f.geometry() {
                            out.push(
                                line.into_iter()
                                    .map(|(px, py)| {
                                        (
                                            x as f64 * ext + px as f64 * ext / l.extent as f64,
                                            y as f64 * ext + py as f64 * ext / l.extent as f64,
                                        )
                                    })
                                    .collect(),
                            );
                        }
                    }
                }
            }
        }
    }
    out
}

/// Metres of the hiking routes inside a park: segments whose midpoint is.
fn route_m_inside(p: &Park, lines: &[Vec<geo::P>]) -> f64 {
    lines
        .iter()
        .flat_map(|l| l.windows(2))
        .filter(|w| p.contains(((w[0].0 + w[1].0) / 2.0, (w[0].1 + w[1].1) / 2.0)))
        .map(|w| geo::metres(w[0], w[1]))
        .sum()
}

/// A park under --min-area-notable is curated with this much of an OSM hiking
/// route inside it (see parks()).
const ROUTE_IN_SMALL_PARK_M: f64 = 500.0;

/// QRank (the park layer's rank) over which a park's Wikidata item is
/// popular enough for --min-area-notable.
const NOTABLE_RANK: f64 = 100.0;

/// A park people go to even if it's small (#109): Cape Split Provincial
/// Park, 4.46 km², is one of Nova Scotia's best-known hikes. Its name says
/// what it is, or its Wikidata item is popular. Of North America's 9,524
/// parks of 1-5 km², 1,436 are notable (8,861 are over 5 km²); the rest are
/// mostly small wildlife areas and nature reserves.
fn notable(name: &str, rank: f64) -> bool {
    let n = name.to_lowercase();
    rank > NOTABLE_RANK
        || [
            "national park",
            "provincial park",
            "state park",
            "parc national",
            "parc provincial",
            "parque nacional",
            "parque estatal",
        ]
        .iter()
        .any(|k| n.contains(k))
}

/// A listed.jsonl entry: a park routes found in its area, and what became of
/// it, so a check of a park's routes (fixtures/expect.py) can tell a park
/// that isn't curated from one outside the area (routes#109).
fn listed(id: &str, name: &str, area: f64, status: &str) -> Value {
    json!({"id": id, "name": name, "km2": (area / 1e4).round() / 100.0, "status": status})
}

/// Day hikes per square root of a park's km of trail (#38): 100 km of
/// trail gets 10, 9 km gets 3.
const DAY_HIKES_PER_SQRT_TRAIL_KM: f64 = 1.0;

fn main() {
    let a = args();
    let t0 = Instant::now();
    if !a.merge.is_empty() {
        // Parks from every shard, in the order one run would write them.
        let mut docs: Vec<Curated> = vec![];
        let mut skipped: Vec<Value> = vec![];
        for dir in &a.merge {
            for e in std::fs::read_dir(dir.join("raw"))
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            {
                let path = e.unwrap().path();
                if path.file_name().is_some_and(|n| n == "listed.jsonl") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    skipped.extend(
                        text.lines()
                            .filter(|l| !l.trim().is_empty())
                            .map(|l| serde_json::from_str::<Value>(l).unwrap()),
                    );
                    continue;
                }
                let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap())
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                docs.push(Curated {
                    area: v["area"].as_f64().unwrap(),
                    secs: v["secs"].as_f64().unwrap_or(0.0),
                    doc: v["doc"].clone(),
                });
            }
        }
        docs.sort_by(|x, y| {
            y.area
                .partial_cmp(&x.area)
                .unwrap()
                .then_with(|| park_id(&x.doc).cmp(park_id(&y.doc)))
        });
        eprintln!("merging {} parks from {} shards", docs.len(), a.merge.len());
        write_timings(&a.out, &docs);
        write_outputs(&a.out, docs, skipped);
        eprintln!("done in {:.1}s", t0.elapsed().as_secs_f64());
        return;
    }
    let archives: Vec<PmTiles> = a
        .tiles
        .iter()
        .map(|t| PmTiles::open(t, a.tile_cache.clone()).unwrap_or_else(|e| panic!("{t}: {e}")))
        .collect();
    // By default, the bounds of the archives with parks, which are the ones
    // listed: so a --write-region run with just those picks the same parks
    // as the curate run that has every layer.
    let bbox = a.bbox.unwrap_or_else(|| {
        archives
            .iter()
            .filter(|ar| has_parks(ar))
            .map(|ar| ar.bounds)
            .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
            .unwrap_or(archives[0].bounds)
    });
    let (mut parks, small) = parks(&archives, bbox, &a);
    eprintln!(
        "{} parks over {} km², or {} km² if notable ({:.1}s)",
        parks.len(),
        a.min_area,
        a.min_area_notable,
        t0.elapsed().as_secs_f64()
    );
    if let Some((k, n)) = a.shard {
        let costs = a.shard_costs.as_ref().map(|f| read_costs(f));
        let (mine, load) = shard(parks, k, n, costs.as_ref());
        parks = mine;
        eprintln!(
            "shard {k}/{n}: {} parks, {} tiles, {}",
            parks.len(),
            parks.iter().map(|p| p.tiles().len()).sum::<usize>(),
            if costs.is_some() {
                format!("expected {load:.0}s of curating")
            } else {
                "balanced by tiles".to_string()
            }
        );
    }
    if let Some(path) = &a.write_region {
        write_region(path, &parks, a.max_tiles);
        return;
    }
    let dem = dem::Dem::new(a.dem_zoom, a.dem_cache.clone(), a.nl_dem.as_deref());
    let qrank = a.qrank.as_ref().map(|p| {
        Mutex::new(
            rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("qrank db"),
        )
    });

    let timed_out: Mutex<Vec<String>> = Mutex::new(vec![]);
    // Curate parks in parallel (biggest first), then write them out in order,
    // dropping routes a bigger park that overlaps this one already has.
    let curate_park = |p: &Park| -> Option<Curated> {
            let t = Instant::now();
            let tiles = p.tiles();
            // A giant park (e.g. Wrangell-St. Elias) would need all its tiles
            // in memory at once; skip it, for now.
            if tiles.len() > a.max_tiles {
                eprintln!(
                    "{} ({}): skipped, {} tiles is over --max-tiles {}",
                    p.name,
                    p.id,
                    tiles.len(),
                    a.max_tiles
                );
                return None;
            }
            let mut area = Area::load(&archives, &p.id, &tiles);
            if let Some(q) = &qrank {
                let q = q.lock().unwrap();
                for d in &mut area.dests {
                    if let Some(id) = d
                        .wikidata
                        .strip_prefix('Q')
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        d.qrank = q
                            .query_row("SELECT qrank FROM qrank WHERE id = ?", [id], |r| {
                                r.get::<_, f64>(0)
                            })
                            .unwrap_or(1.0);
                    }
                }
            }
            // Day hikes: by the park's size, or by how much trail it has if
            // that's more (#38). Small parks with dense networks got 3:
            // Letchworth, 149 km of trail and 12 OSM hiking routes, found 1.
            // On the oracle sample, recall rose from 28.9% to 41.9% (tune)
            // and 23.6% to 45.0% (holdout); day hikes from 501 to 971.
            let by_area = ((p.area / 1e6).sqrt() / 2.0) as usize;
            let by_trail =
                (DAY_HIKES_PER_SQRT_TRAIL_KM * area.trail_km_in_park().sqrt()).round() as usize;
            let n_day = by_area.max(by_trail).clamp(3, 15);
            let deadline = a
                .park_timeout
                .map(|secs| t + std::time::Duration::from_secs_f64(secs));
            let c = curate::curate(&area, &dem, n_day, p.trail_area, deadline);
            if c.timed_out {
                timed_out
                    .lock()
                    .unwrap()
                    .push(format!("{} ({})", p.name, p.id));
            }
            eprintln!(
                "{} ({}): {} tiles, {}: {} day hikes, {} base camps, {} point-to-points ({:.1}s)",
                p.name,
                p.id,
                tiles.len(),
                c.stats,
                c.day.len(),
                c.bases.len(),
                c.through.len(),
                t.elapsed().as_secs_f64()
            );
            let mut doc = c.to_json();
            // Moved to candidates.geojsonl, like "trails" (--candidates).
            if !c.candidates.is_empty() {
                doc["candidates"] = json!(c.candidates);
            }
            doc["park"] = json!({"id": p.id, "name": p.name});
            // For the park panel (hikeratlas/website#72), when the park has them.
            if !p.website.is_empty() {
                doc["park"]["website"] = json!(p.website);
            }
            if !p.operator.is_empty() {
                doc["park"]["operator"] = json!(p.operator);
            }
            // Kept apart from the park's routes (write_outputs moves it to
            // known.jsonl), so the website's files don't grow.
            doc["known"] = json!(recall::check(&area, &doc));
            recall::name_routes(&area, &mut doc);
            recall::name_trails(&area, &mut doc);
            recall::name_loops(&mut doc);
            recall::drop_redundant_from(&mut doc);
            // After naming, so it reports the names hikes are offered under;
            // moved to trails.jsonl like "known". Only with --trails: it's
            // for testing, and a build's production files don't need it.
            if recall::TRAILS.load(std::sync::atomic::Ordering::Relaxed) {
                doc["trails"] = json!(recall::trails(&area, &doc));
            }
            Some(Curated { area: p.area, secs: t.elapsed().as_secs_f64(), doc })
    };
    // Not par_bridge, though it would start a shard's costliest parks first:
    // with each park's own searches in parallel inside it, a shard's CPU fell
    // from 58% to 22% busy and its parks took 2.4x as long (basemap run
    // 42454961538, routes#81).
    let docs: Vec<Curated> = parks.par_iter().filter_map(curate_park).collect();
    // Parks not curated, for listed.jsonl: under --min-area (every shard
    // lists the same ones, so only shard 0 passes them on), over --max-tiles.
    let mut skipped: Vec<Value> = if a.shard.is_none_or(|(k, _)| k == 0) { small } else { vec![] };
    for p in &parks {
        if p.tiles().len() > a.max_tiles {
            skipped.push(listed(&p.id, &p.name, p.area, "over --max-tiles"));
        }
    }

    if a.shard.is_some() {
        let raw = a.out.join("raw");
        std::fs::create_dir_all(&raw).unwrap();
        for c in &docs {
            std::fs::write(
                raw.join(format!("{}.json", park_id(&c.doc))),
                serde_json::to_vec(&json!({"area": c.area, "secs": c.secs, "doc": c.doc})).unwrap(),
            )
            .unwrap();
        }
        let lines: String = skipped.iter().map(|v| format!("{v}\n")).collect();
        std::fs::write(raw.join("listed.jsonl"), lines).unwrap();
        eprintln!("{} parks curated into {}", docs.len(), raw.display());
    } else {
        write_outputs(&a.out, docs, skipped);
    }
    eprintln!("{}", curate::profile());
    eprintln!("{}", dem::stats());
    let timed_out = timed_out.into_inner().unwrap();
    if !timed_out.is_empty() {
        eprintln!(
            "TIMEOUT: {} parks went over --park-timeout and were cut short: {}",
            timed_out.len(),
            timed_out.join(", ")
        );
    }
    eprintln!("done in {:.1}s", t0.elapsed().as_secs_f64());
}

/// A park's curated routes, before routes other parks have are dropped.
struct Curated {
    area: f64,
    /// Seconds it took to curate (for --merge's timings.json).
    secs: f64,
    doc: serde_json::Value,
}

fn park_id(doc: &serde_json::Value) -> &str {
    doc["park"]["id"].as_str().unwrap()
}

/// Shard k of n, and its expected seconds (with costs): parks dealt out
/// costliest first, each to the shard with the least so far, so the shards
/// take about as long. Every shard computes the same deal, from the same park
/// list. A park's cost is its seconds in `costs` (an earlier build's
/// timings.json): tile counts predicted poorly, a shard of big empty northern
/// parks taking 143 s and another 1,509 s (routes#81). Parks `costs` lacks
/// (new, or skipped last time) cost the mean of those it has. Without costs,
/// a park costs its tile count.
///
/// The shard's parks come back costliest first.
fn shard(parks: Vec<Park>, k: usize, n: usize, costs: Option<&HashMap<String, f64>>) -> (Vec<Park>, f64) {
    let mean = costs
        .filter(|c| !c.is_empty())
        .map(|c| c.values().sum::<f64>() / c.len() as f64)
        .unwrap_or(1.0);
    let cost = |p: &Park| -> f64 {
        match costs {
            Some(c) => c.get(&p.id).copied().unwrap_or(mean).max(1e-3),
            None => p.tiles().len().max(1) as f64,
        }
    };
    let mut order: Vec<(f64, Park)> = parks.into_iter().map(|p| (cost(&p), p)).collect();
    order.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.id.cmp(&y.1.id)));
    let mut load = vec![0f64; n];
    let mut mine = vec![];
    for (w, p) in order {
        let s = (0..n).min_by(|&i, &j| load[i].total_cmp(&load[j]).then(i.cmp(&j))).unwrap();
        load[s] += w;
        if s == k {
            mine.push(p);
        }
    }
    (mine, load[k])
}

/// A timings.json: park id to curate seconds.
fn read_costs(path: &std::path::Path) -> HashMap<String, f64> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// DIR/timings.json: each merged park's curate seconds, for --shard-costs.
fn write_timings(out: &std::path::Path, docs: &[Curated]) {
    std::fs::create_dir_all(out).unwrap();
    let t: std::collections::BTreeMap<&str, f64> =
        docs.iter().map(|c| (park_id(&c.doc), (c.secs * 10.0).round() / 10.0)).collect();
    std::fs::write(out.join("timings.json"), serde_json::to_vec(&t).unwrap()).unwrap();
}

/// Write DIR/routes.geojsonl and DIR/parks/, biggest park first, dropping
/// routes a bigger park that overlaps this one already has.
fn write_outputs(out: &std::path::Path, docs: Vec<Curated>, skipped: Vec<Value>) {
    std::fs::create_dir_all(out.join("parks")).unwrap();
    // Every park found, and what became of it (see listed()): the parks not
    // curated, then each curated one as it's written or dropped below.
    let mut listing =
        std::io::BufWriter::new(std::fs::File::create(out.join("listed.jsonl")).unwrap());
    for v in &skipped {
        writeln!(listing, "{v}").unwrap();
    }
    let mut lines =
        std::io::BufWriter::new(std::fs::File::create(out.join("routes.geojsonl")).unwrap());
    let mut known =
        std::io::BufWriter::new(std::fs::File::create(out.join("known.jsonl")).unwrap());
    // Only when the parks have trails (--trails, routes#95).
    let mut trails: Option<std::io::BufWriter<std::fs::File>> = None;
    // Only with --candidates.
    let mut cands: Option<std::io::BufWriter<std::fs::File>> = None;
    let mut bundle =
        std::io::BufWriter::new(std::fs::File::create(out.join("routes.bin")).unwrap());
    let mut index = serde_json::Map::new();
    let mut offset: u64 = 0;
    // Each relation's best coverage over the parks it's checked in.
    let mut best: HashMap<String, f64> = HashMap::new();
    // Each route key, and the park that has it (the first, biggest, to).
    let mut seen: HashMap<String, String> = HashMap::new();
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let multi_key = |t: &serde_json::Value| {
        let (a, b) = (s(t, "from"), s(t, "to"));
        if a < b {
            format!("multi|{a}|{b}")
        } else {
            format!("multi|{b}|{a}")
        }
    };
    let camps = |t: &serde_json::Value| -> std::collections::HashSet<String> {
        t["route"]["stopovers"]
            .as_array()
            .into_iter()
            .flatten()
            // By place, not just name: "Campsite 1" is in many parks.
            .filter_map(|c| {
                let (x, y) = (c["at"][0].as_f64()?, c["at"][1].as_f64()?);
                Some(format!("{}@{:.3},{:.3}", s(c, "name"), x, y))
            })
            .collect()
    };
    // Each park's multi-day trips (key, campsites), to find the same trip in
    // a smaller park that overlaps this one.
    let multis: Vec<Vec<(String, std::collections::HashSet<String>)>> = docs
        .iter()
        .map(|c| {
            c.doc["point_to_points"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| (multi_key(t), camps(t)))
                .collect()
        })
        .collect();
    let ids: Vec<String> = docs.iter().map(|c| s(&c.doc["park"], "id")).collect();
    // The parks with routes of their own, after the de-duplication below, in
    // order; parks whose routes are all another's (twin); and multi-day trips
    // given to a smaller park: (bigger park, smaller park's position, trip key).
    let mut kept: Vec<serde_json::Value> = vec![];
    let mut twins: Vec<(String, String)> = vec![];
    let mut moves: Vec<(String, usize, String)> = vec![];
    for (i, Curated { mut doc, area, .. }) in docs.into_iter().enumerate() {
        // A multi-day trip a smaller park also has, between other ends, with
        // half or more of the same campsites, is that park's (#53): Pacific
        // Rim's whole-reserve relation had the West Coast Trail to a logging
        // road 6.5 km past its end, and its West Coast Trail Unit had it
        // trailhead to trailhead. Trips with the same ends are left to the
        // first (biggest) park below, so a park mapped twice stays one.
        if let Some(Value::Array(ts)) = doc.get_mut("point_to_points") {
            ts.retain(|t| {
                let (k, c) = (multi_key(t), camps(t));
                if c.is_empty() {
                    return true;
                }
                for (j, ms) in multis.iter().enumerate().skip(i + 1) {
                    for (uk, uc) in ms {
                        if *uk != k
                            && !uc.is_empty()
                            && c.intersection(uc).count() as f64 / c.len().min(uc.len()) as f64
                                >= 0.5
                        {
                            moves.push((ids[i].clone(), j, uk.clone()));
                            return false;
                        }
                    }
                }
                true
            });
        }
        let (p_id, p_name) = (s(&doc["park"], "id"), s(&doc["park"], "name"));
        if let Some(Value::Array(ks)) = doc.as_object_mut().and_then(|o| o.remove("known")) {
            for mut k in ks {
                let id = s(&k, "id");
                let c = k["coverage"].as_f64().unwrap_or(0.0);
                let e = best.entry(id).or_insert(0.0);
                *e = e.max(c);
                k["park_id"] = json!(p_id);
                k["park"] = json!(p_name);
                writeln!(known, "{k}").unwrap();
            }
        }
        if let Some(Value::Array(cs)) = doc.as_object_mut().and_then(|o| o.remove("candidates")) {
            let w = cands.get_or_insert_with(|| {
                std::io::BufWriter::new(std::fs::File::create(out.join("candidates.geojsonl")).unwrap())
            });
            for mut c in cs {
                let line = c["route"].as_object_mut().and_then(|r| r.remove("line")).unwrap_or(json!([]));
                c["park_id"] = json!(p_id);
                c["park"] = json!(p_name);
                let f = json!({"type": "Feature", "properties": c,
                               "geometry": {"type": "LineString", "coordinates": line}});
                writeln!(w, "{f}").unwrap();
            }
        }
        if let Some(Value::Array(ts)) = doc.as_object_mut().and_then(|o| o.remove("trails")) {
            let w = trails.get_or_insert_with(|| {
                std::io::BufWriter::new(std::fs::File::create(out.join("trails.jsonl")).unwrap())
            });
            for mut t in ts {
                t["park_id"] = json!(p_id);
                t["park"] = json!(p_name);
                writeln!(w, "{t}").unwrap();
            }
        }
        // Documents from before stops (#70) have none.
        if doc.get("stops").is_none() {
            doc["stops"] = json!([]);
        }
        // The park that already has one of this park's routes, if any.
        let mut twin: Option<String> = None;
        let mut keep = |list: &str, key: &dyn Fn(&serde_json::Value) -> String| {
            let v: Vec<serde_json::Value> = doc[list]
                .as_array()
                .unwrap()
                .iter()
                .filter(|x| match seen.get(&key(x)) {
                    Some(owner) => {
                        twin.get_or_insert_with(|| owner.clone());
                        false
                    }
                    None => {
                        seen.insert(key(x), p_id.clone());
                        true
                    }
                })
                .cloned()
                .collect();
            doc[list] = json!(v);
            v.len()
        };
        let n = keep("day_hikes", &|h| {
            format!("day|{}|{}|{}", s(h, "from"), s(h, "dest"), s(h, "kind"))
        }) + keep("stops", &|h| format!("stop|{}", s(h, "dest")))
            + keep("base_camps", &|b| format!("base|{}", s(b, "camp")))
            + keep("point_to_points", &multi_key);
        let km2 = area;
        if n == 0 {
            // Every route is already another park's: the same park mapped
            // twice (Cape Breton Highlands is relations 4855583 and
            // 17879029). Point this park's entry at that one's routes, so the
            // website finds them under either id.
            let mut l = listed(&p_id, &p_name, km2, if twin.is_some() { "twin" } else { "no routes" });
            if let Some(t) = twin {
                l["twin"] = json!(t);
                twins.push((p_id.clone(), t));
            }
            writeln!(listing, "{l}").unwrap();
            continue;
        }
        writeln!(listing, "{}", listed(&p_id, &p_name, km2, "written")).unwrap();
        kept.push(doc);
    }

    // A park whose multi-day trip went to a smaller park inside it lists it
    // too, as see_also (#53's follow-up): Guadalupe Mountains National
    // Park's trips are its Wilderness's, and its page should still show
    // them. They're references, not copies: the map and search have each
    // trip once, under the smaller park.
    let at: HashMap<String, usize> = kept
        .iter()
        .enumerate()
        .map(|(k, d)| (s(&d["park"], "id"), k))
        .collect();
    // A smaller park that took one of a park's trips lies in it: the park
    // lists all of the smaller park's multi-day trips (Guadalupe Mountains
    // Wilderness has a loop that replaced the national park's own, and the
    // McKittrick Ridge traverse besides).
    let mut pairs: Vec<(String, usize)> = moves.iter().map(|(b, j, _)| (b.clone(), *j)).collect();
    pairs.dedup();
    for (bigger, j) in &pairs {
        let smaller = &ids[*j];
        let (Some(&b), Some(&sm)) = (at.get(bigger), at.get(smaller)) else {
            continue;
        };
        let entries: Vec<Value> = kept[sm]["point_to_points"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(k, t)| {
                json!({
                    "id": format!("{smaller}/multi/{k}"), "park": smaller,
                    "park_name": kept[sm]["park"]["name"],
                    "name": t["name"], "from": t["from"], "to": t["to"], "kind": t["kind"],
                    "km": t["route"]["km"], "gain": t["route"]["gain"],
                })
            })
            .collect();
        let list = kept[b]
            .as_object_mut()
            .unwrap()
            .entry("see_also")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap();
        for e in entries {
            if !list.iter().any(|x| x["id"] == e["id"]) {
                list.push(e);
            }
        }
    }

    let mut written = 0;
    for mut doc in kept {
        let (p_id, p_name) = (s(&doc["park"], "id"), s(&doc["park"], "name"));
        written += 1;
        add_keys(&p_id, &mut doc);
        let body = serde_json::to_vec(&doc).unwrap();
        std::fs::write(out.join("parks").join(format!("{p_id}.json")), &body).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&body).unwrap();
        let gz = gz.finish().unwrap();
        bundle.write_all(&gz).unwrap();
        index.insert(p_id.clone(), json!([offset, gz.len()]));
        offset += gz.len() as u64;
        // one line per route
        let mut emit = |id: String,
                        key: Option<&str>,
                        typ: &str,
                        title: String,
                        route: &serde_json::Value,
                        extra: serde_json::Value| {
            let mut props = json!({
                "id": id, "park": p_name, "park_id": p_id, "type": typ, "title": title,
                "km": route["km"], "gain": route["gain"],
            });
            if let Some(k) = key {
                props["key"] = json!(k);
            }
            for (k, v) in extra.as_object().unwrap() {
                props[k] = v.clone();
            }
            let f = json!({"type": "Feature", "properties": props, "geometry": {"type": "LineString", "coordinates": route["line"]}});
            writeln!(lines, "{}", f).unwrap();
        };
        // A route that follows an OSM hiking route has its name (see
        // recall::name_routes).
        let named = |h: &serde_json::Value, otherwise: String| match h["name"].as_str() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => otherwise,
        };
        for (i, h) in doc["day_hikes"].as_array().unwrap().iter().enumerate() {
            emit(
                format!("{p_id}/day/{i}"),
                h["key"].as_str(),
                "day",
                named(h, format!("{} from {}", s(h, "dest"), s(h, "from"))),
                &h["route"],
                json!({"kind": h["kind"]}),
            );
        }
        // Stops (#70): "<sight> from <where the car is>".
        for (i, h) in doc["stops"].as_array().into_iter().flatten().enumerate() {
            emit(
                format!("{p_id}/stop/{i}"),
                h["key"].as_str(),
                "stop",
                format!("{} from {}", s(h, "dest"), s(h, "from")),
                &h["route"],
                json!({"kind": "stop"}),
            );
        }
        for (i, b) in doc["base_camps"].as_array().unwrap().iter().enumerate() {
            let camp = s(b, "camp");
            emit(
                format!("{p_id}/base/{i}"),
                b["key"].as_str(),
                "approach",
                format!("{camp} from {}", s(b, "from")),
                &b["approach"],
                json!({"kind": "approach"}),
            );
            for (j, h) in b["hikes"].as_array().unwrap().iter().enumerate() {
                emit(
                    format!("{p_id}/base/{i}/{j}"),
                    h["key"].as_str(),
                    "from-camp",
                    named(h, format!("{} from {camp}", s(h, "dest"))),
                    &h["route"],
                    json!({"kind": h["kind"]}),
                );
            }
        }
        for (i, t) in doc["point_to_points"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            emit(
                format!("{p_id}/multi/{i}"),
                t["key"].as_str(),
                "multi-day",
                named(
                    t,
                    if s(t, "kind") == "loop" {
                        format!("{} loop", s(t, "from"))
                    } else {
                        format!("{} to {}", s(t, "from"), s(t, "to"))
                    },
                ),
                &t["route"],
                json!({"kind": s(t, "kind"), "stopovers": t["route"]["stopovers"].as_array().unwrap().len()}),
            );
        }
    }
    for (p, t) in twins {
        if let Some(i) = index.get(&t).cloned() {
            index.insert(p, i);
        }
    }
    eprintln!("{written} parks with routes");
    lines.flush().unwrap();
    known.flush().unwrap();
    listing.flush().unwrap();
    if let Some(mut w) = trails {
        w.flush().unwrap();
    }
    if let Some(mut w) = cands {
        w.flush().unwrap();
    }
    bundle.flush().unwrap();
    std::fs::write(
        out.join("index.json"),
        serde_json::to_vec(&json!({"bundle": "routes.bin", "parks": index})).unwrap(),
    )
    .unwrap();
    let n = best.len();
    let found = best.values().filter(|&&c| c >= recall::FOUND).count();
    let partial = best
        .values()
        .filter(|&&c| (recall::PARTIAL..recall::FOUND).contains(&c))
        .count();
    eprintln!(
        "known hikes (OSM hiking routes): {n}, found {found}, partly {partial}, missed {} (details in known.jsonl)",
        n - found - partial
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shard_park(id: &str) -> Park {
        Park {
            id: id.to_string(),
            name: id.to_string(),
            website: String::new(),
            operator: String::new(),
            area: 1.0,
            trail_area: false,
            small_notable: false,
            needs_route: false,
            min: (0.0, 0.0),
            max: (0.0, 0.0),
            rings: vec![],
        }
    }

    // A park's outline, in pieces as the z8 tiles cut it: an L of two
    // squares. A point in the L is inside; one in its box but not the L isn't.
    #[test]
    fn a_park_contains_what_its_outline_does() {
        let mut p = shard_park("big");
        let sq = |x: f64, y: f64| vec![(x, y), (x + 1.0, y), (x + 1.0, y + 1.0), (x, y + 1.0)];
        p.rings = vec![sq(0.0, 0.0), sq(1.0, 0.0), sq(0.0, 1.0)];
        (p.min, p.max) = ((0.0, 0.0), (2.0, 2.0));
        assert!(p.contains((0.5, 0.5)));
        assert!(p.contains((1.5, 0.5)));
        assert!(p.contains((0.5, 1.5)));
        assert!(!p.contains((1.5, 1.5))); // the L's notch, in its box
        assert!(!p.contains((2.5, 0.5)));
    }

    // A small unit named for a bigger park, nearby but outside its outline,
    // is the bigger park's (Patapsco's Rockhaven Area); a far one isn't.
    #[test]
    fn a_unit_named_for_a_park_is_inside_it() {
        let mut big = shard_park("big");
        big.name = "Patapsco Valley State Park".into();
        let sq = |x: f64, y: f64, d: f64| vec![(x, y), (x + d, y), (x + d, y + d), (x, y + d)];
        let d = 5000.0 / geo::mpu(0.0); // 5 km
        big.rings = vec![sq(0.0, 0.0, d)];
        (big.min, big.max) = ((0.0, 0.0), (d, d));
        let unit = |x: f64| {
            let mut u = shard_park("unit");
            u.name = "Patapsco Valley State Park Rockhaven Area".into();
            u.rings = vec![sq(x, 0.0, d / 10.0)];
            (u.min, u.max) = ((x, 0.0), (x + d / 10.0, d / 10.0));
            u
        };
        assert!(big.has_inside(&unit(1.2 * d))); // 1 km past its edge
        assert!(!big.has_inside(&unit(4.0 * d))); // 15 km past it
        let mut other = unit(1.2 * d);
        other.name = "Rockhaven Park".into();
        assert!(!big.has_inside(&other));
    }

    // Parks go costliest first to the least-loaded shard; every shard computes
    // the same deal, so they split the parks between them (routes#81).
    #[test]
    fn shard_by_costs() {
        let ids = ["a", "b", "c", "d", "e", "new"];
        let costs: HashMap<String, f64> =
            [("a", 100.0), ("b", 60.0), ("c", 50.0), ("d", 10.0), ("e", 0.0)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let deal = |k| {
            let (mine, load) = shard(ids.iter().map(|i| shard_park(i)).collect(), k, 2, Some(&costs));
            (mine.into_iter().map(|p| p.id).collect::<Vec<_>>(), load)
        };
        // a (100) | b (60) + c (50) = 110 | new (the mean, 44) to a: 144 |
        // d (10) to b+c: 120 | e to b+c.
        assert_eq!(deal(0), (vec!["a".to_string(), "new".to_string()], 144.0));
        assert_eq!(deal(1).0, ["b", "c", "d", "e"]);
        assert!((deal(1).1 - 120.0).abs() < 0.01);
    }

    fn trip(from: &str, to: &str, camps: &[(&str, f64)]) -> Value {
        json!({"from": from, "to": to, "kind": "point-to-point", "route": {
            "km": 1.0, "gain": 0.0, "line": [[0.0, 0.0], [1.0, 1.0]],
            "stopovers": camps.iter().map(|(n, x)| json!({"name": n, "km": 0.0, "at": [x, 48.0]})).collect::<Vec<_>>(),
        }})
    }

    fn park(id: &str, area: f64, day: &[&str], multi: Vec<Value>) -> Curated {
        Curated {
            area,
            secs: 0.0,
            doc: json!({
                "park": {"id": id, "name": id},
                "day_hikes": day.iter().map(|d| json!({"from": "T", "dest": d, "kind": "out-and-back",
                    "route": {"km": 1.0, "gain": 0.0, "line": [[0.0, 0.0], [1.0, 1.0]]}})).collect::<Vec<_>>(),
                "base_camps": [],
                "point_to_points": multi,
            }),
        }
    }

    fn written(dir: &std::path::Path, id: &str) -> Option<Value> {
        std::fs::read(dir.join("parks").join(format!("{id}.json")))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("routes-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    // #53: the whole Pacific Rim reserve had the West Coast Trail to a
    // logging road; its West Coast Trail Unit, trailhead to trailhead.
    #[test]
    fn same_trip_between_other_ends_goes_to_the_smaller_park() {
        let camps = [("Tsusiat Falls", -124.9), ("Cribs Creek", -124.7), ("Walbran Creek", -124.6)];
        let dir = tmp("smaller");
        write_outputs(
            &dir,
            vec![
                park("reserve", 2.0, &["Long Beach"], vec![trip("WCT", "F-0100 trailhead", &camps)]),
                park("unit", 1.0, &[], vec![trip("WCT", "WCT", &camps)]),
            ],
            vec![],
        );
        let big = written(&dir, "reserve").unwrap();
        assert_eq!(big["point_to_points"].as_array().unwrap().len(), 0);
        assert_eq!(big["day_hikes"].as_array().unwrap().len(), 1);
        let small = written(&dir, "unit").unwrap();
        assert_eq!(small["point_to_points"][0]["to"], "WCT");
        // The bigger park still lists it, pointing at the smaller park's.
        assert_eq!(big["see_also"][0]["id"], "unit/multi/0");
        assert_eq!(big["see_also"][0]["park"], "unit");
        assert!(small.get("see_also").is_none());
    }

    // A park mapped twice (Cape Breton Highlands): the same trips, the same
    // ends, stay in the bigger one, and the smaller points at it.
    #[test]
    fn a_park_mapped_twice_stays_one() {
        let camps = [("A", -60.0), ("B", -60.1)];
        let dir = tmp("twins");
        write_outputs(
            &dir,
            vec![
                park("big", 2.0, &["Lake"], vec![trip("X", "Y", &camps)]),
                park("small", 1.0, &["Lake"], vec![trip("X", "Y", &camps)]),
            ],
            vec![listed("tiny", "Tiny Park", 3.2e6, "under --min-area")],
        );
        assert_eq!(written(&dir, "big").unwrap()["point_to_points"].as_array().unwrap().len(), 1);
        assert!(written(&dir, "small").is_none());
        // listed.jsonl says what became of each, the small one's twin included.
        let listed: Vec<Value> = std::fs::read_to_string(dir.join("listed.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let status = |id: &str| listed.iter().find(|v| v["id"] == id).map(|v| v["status"].clone());
        assert_eq!(status("tiny"), Some(json!("under --min-area")));
        assert_eq!(listed.iter().find(|v| v["id"] == "tiny").unwrap()["km2"], 3.2);
        assert_eq!(status("big"), Some(json!("written")));
        assert_eq!(status("small"), Some(json!("twin")));
        assert_eq!(listed.iter().find(|v| v["id"] == "small").unwrap()["twin"], "big");
    }

    // Same-named campsites elsewhere aren't the same trip.
    #[test]
    fn campsites_match_by_place_not_just_name() {
        let dir = tmp("names");
        write_outputs(
            &dir,
            vec![
                park("a", 2.0, &[], vec![trip("X", "Y", &[("Campsite 1", -60.0), ("Campsite 2", -60.1)])]),
                park("b", 1.0, &[], vec![trip("P", "Q", &[("Campsite 1", -120.0), ("Campsite 2", -120.1)])]),
            ],
            vec![],
        );
        assert_eq!(written(&dir, "a").unwrap()["point_to_points"].as_array().unwrap().len(), 1);
        assert_eq!(written(&dir, "b").unwrap()["point_to_points"].as_array().unwrap().len(), 1);
    }
}

/// A route's stable key: what defines it, so it survives rebuilds that add,
/// drop or reorder a park's routes (its `id`, "<park>/day/<i>", is a
/// position and doesn't). Names are lower-cased with spaces collapsed; a
/// multi-day trip's ends are sorted, as either way round is the same trip.
///   day:       <park>|day|<trailhead>|<destination>|<kind>
///   stop:      <park>|stop|<where the car is>|<sight>
///   approach:  <park>|approach|<trailhead>|<camp>
///   from-camp: <park>|camp|<camp>|<destination>|<kind>
///   multi-day: <park>|multi|<end>|<end>|<kind>
fn route_key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
                .replace('|', "/")
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Sets `key` (see route_key) on each of a park document's routes.
fn add_keys(p_id: &str, doc: &mut Value) {
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    for h in doc["day_hikes"].as_array_mut().into_iter().flatten() {
        h["key"] = json!(route_key(&[p_id, "day", &s(h, "from"), &s(h, "dest"), &s(h, "kind")]));
    }
    for h in doc["stops"].as_array_mut().into_iter().flatten() {
        h["key"] = json!(route_key(&[p_id, "stop", &s(h, "from"), &s(h, "dest")]));
    }
    for b in doc["base_camps"].as_array_mut().into_iter().flatten() {
        let camp = s(b, "camp");
        b["key"] = json!(route_key(&[p_id, "approach", &s(b, "from"), &camp]));
        for h in b["hikes"].as_array_mut().into_iter().flatten() {
            h["key"] = json!(route_key(&[p_id, "camp", &camp, &s(h, "dest"), &s(h, "kind")]));
        }
    }
    for t in doc["point_to_points"].as_array_mut().into_iter().flatten() {
        let (mut a, mut b) = (s(t, "from"), s(t, "to"));
        if s(t, "kind") == "loop" {
            b = a.clone();
        } else if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        t["key"] = json!(route_key(&[p_id, "multi", &a, &b, &s(t, "kind")]));
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    // Keys are what define a route, not its position (cc-crawler links trip
    // reports to routes by key, across rebuilds).
    #[test]
    fn keys_name_the_route_and_ignore_order_and_case() {
        let mut doc = json!({
            "day_hikes": [{"from": "Rubble Creek  Trailhead", "dest": "The Black Tusk", "kind": "out-and-back"}],
            "stops": [],
            "base_camps": [{"camp": "Garibaldi Lake", "from": "Rubble Creek Trailhead",
                            "hikes": [{"dest": "Panorama Ridge", "kind": "out-and-back"}]}],
            "point_to_points": [{"from": "Rubble Creek Trailhead", "to": "Cheakamus Lake", "kind": "point-to-point"},
                                {"from": "A|B", "to": "", "kind": "loop"}],
        });
        add_keys("2226817", &mut doc);
        assert_eq!(doc["day_hikes"][0]["key"], "2226817|day|rubble creek trailhead|the black tusk|out-and-back");
        assert_eq!(doc["base_camps"][0]["key"], "2226817|approach|rubble creek trailhead|garibaldi lake");
        assert_eq!(doc["base_camps"][0]["hikes"][0]["key"], "2226817|camp|garibaldi lake|panorama ridge|out-and-back");
        assert_eq!(doc["point_to_points"][0]["key"], "2226817|multi|cheakamus lake|rubble creek trailhead|point-to-point");
        assert_eq!(doc["point_to_points"][1]["key"], "2226817|multi|a/b|a/b|loop");
    }
}
