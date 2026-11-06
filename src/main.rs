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
  [--bbox W,S,E,N]       only parks in this box (default: the tiles' bounds)
  [--min-area KM2]       smallest park to curate (default 5)
  [--max-tiles N]        skip parks needing more z14 tiles than this (default 50000)
  [--qrank qrank.db]     Wikidata popularity (hikeratlas/qrank)
  [--tile-cache DIR]     cache tiles fetched from a URL here
  [--dem-cache DIR]      cache DEM tiles here
  [--dem-zoom Z]         DEM zoom (default 12)
  [--park-timeout SECS]  stop starting new searches in a park after this
                         long, keeping what was found, and report it
  [--shard K/N]          curate only shard K (0-based) of N, balanced by tile
                         count, and write each park's routes to DIR/raw/ for
                         a later --merge, instead of the outputs below

   or: routes --merge DIR [--merge DIR...] --out DIR
  merge --shard runs' DIR/raw/ into the outputs below";

struct Args {
    tiles: Vec<String>,
    out: PathBuf,
    park: Option<String>,
    park_id: Option<String>,
    bbox: Option<[f64; 4]>,
    min_area: f64,
    max_tiles: usize,
    qrank: Option<String>,
    tile_cache: Option<PathBuf>,
    dem_cache: Option<PathBuf>,
    dem_zoom: u8,
    shard: Option<(usize, usize)>,
    merge: Vec<PathBuf>,
    park_timeout: Option<f64>,
}

fn args() -> Args {
    let mut a = Args {
        tiles: vec![],
        out: PathBuf::new(),
        park: None,
        park_id: None,
        bbox: None,
        min_area: 5.0,
        max_tiles: 50000,
        qrank: None,
        tile_cache: None,
        dem_cache: None,
        dem_zoom: 12,
        shard: None,
        merge: vec![],
        park_timeout: None,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let die = || -> ! {
        eprintln!("{USAGE}");
        std::process::exit(2)
    };
    while i < v.len() {
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
            "--max-tiles" => a.max_tiles = val.parse().unwrap_or_else(|_| die()),
            "--qrank" => a.qrank = Some(val),
            "--tile-cache" => a.tile_cache = Some(val.into()),
            "--dem-cache" => a.dem_cache = Some(val.into()),
            "--dem-zoom" => a.dem_zoom = val.parse().unwrap_or_else(|_| die()),
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
    area: f64,
    trail_area: bool,
    min: geo::P,
    max: geo::P,
}

impl Park {
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

/// Parks, from the z8 tiles' park layer (which has every park over ~0.5 km²).
fn parks(archives: &[PmTiles], bbox: [f64; 4], a: &Args) -> Vec<Park> {
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
    let scale = geo::EXT * 64.0; // world units per z8 tile
    tiles.par_iter().for_each(|&(x, y)| {
        for ar in archives {
            let Some(b) = ar.get(z, x, y).expect("reading tile") else {
                continue;
            };
            for l in mvt::decode(&b) {
                // Parks, and trail areas: trail networks outside parks that the
                // basemap picked out (bin/trail-areas.py), e.g. the Skerwink
                // Trail. Those are already filtered, so skip --min-area for them.
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
                    if (!trail_area && area < a.min_area * 1e6)
                        || name.is_empty()
                        || a.park
                            .as_ref()
                            .is_some_and(|p| !name.to_lowercase().contains(p))
                        || a.park_id
                            .as_ref()
                            .is_some_and(|id| l.str(f, "osm_id") != id)
                    {
                        continue;
                    }
                    let mut m = found.lock().unwrap();
                    let p = m
                        .entry(l.str(f, "osm_id").to_string())
                        .or_insert_with(|| Park {
                            id: l.str(f, "osm_id").to_string(),
                            name: name.to_string(),
                            area,
                            trail_area,
                            min: (f64::MAX, f64::MAX),
                            max: (f64::MIN, f64::MIN),
                        });
                    for ring in f.geometry() {
                        for (px, py) in ring {
                            let w = (
                                x as f64 * scale + px as f64 * scale / l.extent as f64,
                                y as f64 * scale + py as f64 * scale / l.extent as f64,
                            );
                            p.min = (p.min.0.min(w.0), p.min.1.min(w.1));
                            p.max = (p.max.0.max(w.0), p.max.1.max(w.1));
                        }
                    }
                }
            }
        }
    });
    let mut v: Vec<Park> = found.into_inner().unwrap().into_values().collect();
    v.sort_by(|a, b| b.area.partial_cmp(&a.area).unwrap());
    v
}

fn main() {
    let a = args();
    let t0 = Instant::now();
    if !a.merge.is_empty() {
        // Parks from every shard, in the order one run would write them.
        let mut docs: Vec<Curated> = vec![];
        for dir in &a.merge {
            for e in std::fs::read_dir(dir.join("raw"))
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            {
                let path = e.unwrap().path();
                let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap())
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                docs.push(Curated {
                    area: v["area"].as_f64().unwrap(),
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
        write_outputs(&a.out, docs);
        eprintln!("done in {:.1}s", t0.elapsed().as_secs_f64());
        return;
    }
    let archives: Vec<PmTiles> = a
        .tiles
        .iter()
        .map(|t| PmTiles::open(t, a.tile_cache.clone()).unwrap_or_else(|e| panic!("{t}: {e}")))
        .collect();
    let bbox = a.bbox.unwrap_or(archives[0].bounds);
    let mut parks = parks(&archives, bbox, &a);
    eprintln!(
        "{} parks over {} km² ({:.1}s)",
        parks.len(),
        a.min_area,
        t0.elapsed().as_secs_f64()
    );
    if let Some((k, n)) = a.shard {
        parks = shard(parks, k, n);
        eprintln!(
            "shard {k}/{n}: {} parks, {} tiles",
            parks.len(),
            parks.iter().map(|p| p.tiles().len()).sum::<usize>()
        );
    }
    let dem = dem::Dem::new(a.dem_zoom, a.dem_cache.clone());
    let qrank = a.qrank.as_ref().map(|p| {
        Mutex::new(
            rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("qrank db"),
        )
    });

    let timed_out: Mutex<Vec<String>> = Mutex::new(vec![]);
    // Curate parks in parallel (biggest first), then write them out in order,
    // dropping routes a bigger park that overlaps this one already has.
    let docs: Vec<Curated> = parks
        .par_iter()
        .filter_map(|p| {
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
            let n_day = (((p.area / 1e6).sqrt() / 2.0) as usize).clamp(3, 15);
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
            doc["park"] = json!({"id": p.id, "name": p.name});
            // Kept apart from the park's routes (write_outputs moves it to
            // known.jsonl), so the website's files don't grow.
            doc["known"] = json!(recall::check(&area, &doc));
            recall::name_routes(&area, &mut doc);
            Some(Curated { area: p.area, doc })
        })
        .collect();

    if a.shard.is_some() {
        let raw = a.out.join("raw");
        std::fs::create_dir_all(&raw).unwrap();
        for c in &docs {
            std::fs::write(
                raw.join(format!("{}.json", park_id(&c.doc))),
                serde_json::to_vec(&json!({"area": c.area, "doc": c.doc})).unwrap(),
            )
            .unwrap();
        }
        eprintln!("{} parks curated into {}", docs.len(), raw.display());
    } else {
        write_outputs(&a.out, docs);
    }
    eprintln!("{}", curate::profile());
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
    doc: serde_json::Value,
}

fn park_id(doc: &serde_json::Value) -> &str {
    doc["park"]["id"].as_str().unwrap()
}

/// Shard k of n: parks dealt out biggest (by tiles) first, each to the shard
/// with the fewest tiles so far, so the shards take about as long. Every shard
/// computes the same deal, from the same park list.
fn shard(parks: Vec<Park>, k: usize, n: usize) -> Vec<Park> {
    let mut order: Vec<(usize, &Park)> = parks.iter().map(|p| (p.tiles().len(), p)).collect();
    order.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.id.cmp(&y.1.id)));
    let mut load = vec![0usize; n];
    let mut mine: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (w, p) in order {
        let s = (0..n).min_by_key(|&i| (load[i], i)).unwrap();
        load[s] += w.max(1);
        if s == k {
            mine.insert(p.id.clone());
        }
    }
    parks.into_iter().filter(|p| mine.contains(&p.id)).collect()
}

/// Write DIR/routes.geojsonl and DIR/parks/, biggest park first, dropping
/// routes a bigger park that overlaps this one already has.
fn write_outputs(out: &std::path::Path, docs: Vec<Curated>) {
    std::fs::create_dir_all(out.join("parks")).unwrap();
    let mut lines =
        std::io::BufWriter::new(std::fs::File::create(out.join("routes.geojsonl")).unwrap());
    let mut known =
        std::io::BufWriter::new(std::fs::File::create(out.join("known.jsonl")).unwrap());
    let mut bundle =
        std::io::BufWriter::new(std::fs::File::create(out.join("routes.bin")).unwrap());
    let mut index = serde_json::Map::new();
    let mut offset: u64 = 0;
    // Each relation's best coverage over the parks it's checked in.
    let mut best: HashMap<String, f64> = HashMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut written = 0;
    for Curated { mut doc, .. } in docs {
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
        let mut keep = |list: &str, key: &dyn Fn(&serde_json::Value) -> String| {
            let v: Vec<serde_json::Value> = doc[list]
                .as_array()
                .unwrap()
                .iter()
                .filter(|x| seen.insert(key(x)))
                .cloned()
                .collect();
            doc[list] = json!(v);
            v.len()
        };
        let n = keep("day_hikes", &|h| {
            format!("day|{}|{}|{}", s(h, "from"), s(h, "dest"), s(h, "kind"))
        }) + keep("base_camps", &|b| format!("base|{}", s(b, "camp")))
            + keep("point_to_points", &|t| {
                let (a, b) = (s(t, "from"), s(t, "to"));
                if a < b {
                    format!("multi|{a}|{b}")
                } else {
                    format!("multi|{b}|{a}")
                }
            });
        if n == 0 {
            continue;
        }
        written += 1;
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
                        typ: &str,
                        title: String,
                        route: &serde_json::Value,
                        extra: serde_json::Value| {
            let mut props = json!({
                "id": id, "park": p_name, "park_id": p_id, "type": typ, "title": title,
                "km": route["km"], "gain": route["gain"],
            });
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
                "day",
                named(h, format!("{} from {}", s(h, "dest"), s(h, "from"))),
                &h["route"],
                json!({"kind": h["kind"]}),
            );
        }
        for (i, b) in doc["base_camps"].as_array().unwrap().iter().enumerate() {
            let camp = s(b, "camp");
            emit(
                format!("{p_id}/base/{i}"),
                "approach",
                format!("{camp} from {}", s(b, "from")),
                &b["approach"],
                json!({"kind": "approach"}),
            );
            for (j, h) in b["hikes"].as_array().unwrap().iter().enumerate() {
                emit(
                    format!("{p_id}/base/{i}/{j}"),
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
    eprintln!("{written} parks with routes");
    lines.flush().unwrap();
    known.flush().unwrap();
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
