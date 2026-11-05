// Curate hiking routes for every park in a basemap build.
//
//   routes --tiles LAYER.pmtiles [--tiles ...] --out DIR [options]
//
// Reads the basemap's z14 tiles (path, road, land, water_poly, park, peak and
// poi layers; they may be spread over several archives) and writes
//   DIR/routes.geojsonl       one line per route, for the map
//   DIR/parks/<osm_id>.json   each park's routes in full, for its page
mod area;
mod curate;
mod dem;
mod geo;
mod mvt;
mod pmtiles;

use area::Area;
use pmtiles::PmTiles;
use rayon::prelude::*;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

const USAGE: &str = "usage: routes --tiles FILE|URL [--tiles ...] --out DIR
  [--park NAME]          only parks whose name contains NAME
  [--bbox W,S,E,N]       only parks in this box (default: the tiles' bounds)
  [--min-area KM2]       smallest park to curate (default 5)
  [--max-tiles N]        skip parks needing more z14 tiles than this (default 50000)
  [--qrank qrank.db]     Wikidata popularity (hikeratlas/qrank)
  [--tile-cache DIR]     cache tiles fetched from a URL here
  [--dem-cache DIR]      cache DEM tiles here
  [--dem-zoom Z]         DEM zoom (default 12)";

struct Args {
    tiles: Vec<String>,
    out: PathBuf,
    park: Option<String>,
    bbox: Option<[f64; 4]>,
    min_area: f64,
    max_tiles: usize,
    qrank: Option<String>,
    tile_cache: Option<PathBuf>,
    dem_cache: Option<PathBuf>,
    dem_zoom: u8,
}

fn args() -> Args {
    let mut a = Args {
        tiles: vec![],
        out: PathBuf::new(),
        park: None,
        bbox: None,
        min_area: 5.0,
        max_tiles: 50000,
        qrank: None,
        tile_cache: None,
        dem_cache: None,
        dem_zoom: 12,
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
            _ => die(),
        }
        i += 2;
    }
    if a.tiles.is_empty() || a.out.as_os_str().is_empty() {
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
    let archives: Vec<PmTiles> = a
        .tiles
        .iter()
        .map(|t| PmTiles::open(t, a.tile_cache.clone()).unwrap_or_else(|e| panic!("{t}: {e}")))
        .collect();
    let bbox = a.bbox.unwrap_or(archives[0].bounds);
    let t0 = Instant::now();
    let parks = parks(&archives, bbox, &a);
    eprintln!(
        "{} parks over {} km² ({:.1}s)",
        parks.len(),
        a.min_area,
        t0.elapsed().as_secs_f64()
    );
    let dem = dem::Dem::new(a.dem_zoom, a.dem_cache.clone());
    let qrank = a.qrank.as_ref().map(|p| {
        Mutex::new(
            rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("qrank db"),
        )
    });
    std::fs::create_dir_all(a.out.join("parks")).unwrap();
    let mut lines =
        std::io::BufWriter::new(std::fs::File::create(a.out.join("routes.geojsonl")).unwrap());

    // Curate parks in parallel (biggest first), then write them out in order,
    // dropping routes a bigger park that overlaps this one already has.
    let docs: Vec<Option<serde_json::Value>> = parks
        .par_iter()
        .map(|p| {
            let t = Instant::now();
            // the park's z14 tiles, plus 5 km around it for trailheads outside
            let r = 5000.0 / geo::mpu((p.min.1 + p.max.1) / 2.0);
            let (x0, y0) = geo::tile_of((p.min.0 - r, p.min.1 - r));
            let (x1, y1) = geo::tile_of((p.max.0 + r, p.max.1 + r));
            let n_tiles = (x1 - x0 + 1) as usize * (y1 - y0 + 1) as usize;
            // A giant park (e.g. Wrangell-St. Elias) would need all its tiles
            // in memory at once; skip it, for now.
            if n_tiles > a.max_tiles {
                eprintln!(
                    "{} ({}): skipped, {n_tiles} tiles is over --max-tiles {}",
                    p.name, p.id, a.max_tiles
                );
                return None;
            }
            let tiles: Vec<(u32, u32)> = (x0..=x1)
                .flat_map(|x| (y0..=y1).map(move |y| (x, y)))
                .collect();
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
            let c = curate::curate(&area, &dem, n_day, p.trail_area);
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
            Some(doc)
        })
        .collect();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut written = 0;
    for (p, doc) in parks.iter().zip(docs) {
        let Some(mut doc) = doc else { continue };
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
        std::fs::write(
            a.out.join("parks").join(format!("{}.json", p.id)),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
        // one line per route
        let mut emit = |id: String,
                        typ: &str,
                        title: String,
                        route: &serde_json::Value,
                        extra: serde_json::Value| {
            let mut props = json!({
                "id": id, "park": p.name, "park_id": p.id, "type": typ, "title": title,
                "km": route["km"], "gain": route["gain"],
            });
            for (k, v) in extra.as_object().unwrap() {
                props[k] = v.clone();
            }
            let f = json!({"type": "Feature", "properties": props, "geometry": {"type": "LineString", "coordinates": route["line"]}});
            writeln!(lines, "{}", f).unwrap();
        };
        for (i, h) in doc["day_hikes"].as_array().unwrap().iter().enumerate() {
            emit(
                format!("{}/day/{i}", p.id),
                "day",
                format!("{} from {}", s(h, "dest"), s(h, "from")),
                &h["route"],
                json!({"kind": h["kind"]}),
            );
        }
        for (i, b) in doc["base_camps"].as_array().unwrap().iter().enumerate() {
            let camp = s(b, "camp");
            emit(
                format!("{}/base/{i}", p.id),
                "approach",
                format!("{camp} from {}", s(b, "from")),
                &b["approach"],
                json!({"kind": "approach"}),
            );
            for (j, h) in b["hikes"].as_array().unwrap().iter().enumerate() {
                emit(
                    format!("{}/base/{i}/{j}", p.id),
                    "from-camp",
                    format!("{} from {camp}", s(h, "dest")),
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
                format!("{}/multi/{i}", p.id),
                "multi-day",
                if s(t, "kind") == "loop" {
                    format!("{} loop", s(t, "from"))
                } else {
                    format!("{} to {}", s(t, "from"), s(t, "to"))
                },
                &t["route"],
                json!({"kind": s(t, "kind"), "stopovers": t["route"]["stopovers"].as_array().unwrap().len()}),
            );
        }
    }
    eprintln!("{written} parks with routes");
    lines.flush().unwrap();
    eprintln!("done in {:.1}s", t0.elapsed().as_secs_f64());
}
