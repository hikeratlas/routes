// How many of a park's known hikes the curated routes find: OSM hiking route
// relations (the basemap's hiking_route layer, e.g. the West Coast Trail,
// Lake O'Hara's Alpine Circuit) checked against the routes, as a measure that
// doesn't depend on our own scoring. Only reported, never used to pick routes.
use crate::area::Area;
use crate::geo::{self, P, metres, mpu};
use rustc_hash::FxHashMap as HashMap;
use serde_json::{Value, json};

/// A route relation's segments in the loaded tiles.
pub struct Known {
    pub id: String,
    pub name: String,
    pub network: String,
    pub segs: Vec<(P, P)>,
}

/// Within this of the park (or in it) counts as the park's.
const NEAR_PARK_M: f64 = 5000.0;
/// A route point this close to the relation covers it.
const COVER_M: f64 = 50.0;
/// Sample the relation every this many metres.
const STEP_M: f64 = 25.0;
/// Longest stretch checked: our longest routes are ~100 km.
const MAX_KM: f64 = 100.0;
const MIN_KM: f64 = 1.0;
pub const FOUND: f64 = 0.7;
pub const PARTIAL: f64 = 0.3;

fn world(lon: f64, lat: f64) -> P {
    let x = (lon + 180.0) / 360.0 * geo::WORLD;
    let y = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * geo::WORLD;
    (x, y)
}

/// The routes in a park's document, as (title, points in world units).
fn routes(doc: &Value) -> Vec<(String, Vec<P>)> {
    let line = |r: &Value| -> Vec<P> {
        r["line"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| Some(world(c[0].as_f64()?, c[1].as_f64()?)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut out = vec![];
    for h in doc["day_hikes"].as_array().into_iter().flatten() {
        out.push((
            format!("{} from {}", s(h, "dest"), s(h, "from")),
            line(&h["route"]),
        ));
    }
    for b in doc["base_camps"].as_array().into_iter().flatten() {
        out.push((
            format!("{} from {}", s(b, "camp"), s(b, "from")),
            line(&b["approach"]),
        ));
        for h in b["hikes"].as_array().into_iter().flatten() {
            out.push((
                format!("{} from {}", s(h, "dest"), s(b, "camp")),
                line(&h["route"]),
            ));
        }
    }
    for t in doc["point_to_points"].as_array().into_iter().flatten() {
        out.push((
            format!("{} to {}", s(t, "from"), s(t, "to")),
            line(&t["route"]),
        ));
    }
    out
}

/// Each known hike in the park: how much of it the best single route covers.
pub fn check(area: &Area, doc: &Value) -> Vec<Value> {
    let rs = routes(doc);
    let mut out = vec![];
    for k in &area.known {
        // The relation's stretch in or near the park, sampled every STEP_M.
        let mut pts: Vec<P> = vec![];
        let (mut near_m, mut all_m) = (0.0, 0.0);
        for &(a, b) in &k.segs {
            let m = metres(a, b);
            all_m += m;
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            if area.park_dist(mid, NEAR_PARK_M).is_none() {
                continue;
            }
            near_m += m;
            let n = (m / STEP_M).ceil().max(1.0) as usize;
            for i in 0..n {
                let t = (i as f64 + 0.5) / n as f64;
                pts.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
            }
        }
        let km = near_m / 1000.0;
        // Mostly elsewhere (a long-distance trail passing by), or too short
        // or long to be one of our routes.
        if near_m < all_m * 0.5 || !(MIN_KM..=MAX_KM).contains(&km) {
            continue;
        }
        let mut best: Option<(f64, &str)> = None;
        for (title, line) in &rs {
            let cov = coverage(&pts, line);
            if best.is_none_or(|b| cov > b.0) {
                best = Some((cov, title));
            }
        }
        let (cov, title) = best.unwrap_or((0.0, ""));
        out.push(json!({
            "id": k.id, "name": k.name, "network": k.network,
            "km": (km * 10.0).round() / 10.0,
            "coverage": (cov * 100.0).round() / 100.0,
            "best": title,
        }));
    }
    out
}

/// The share of `pts` within COVER_M of `line`.
fn coverage(pts: &[P], line: &[P]) -> f64 {
    if pts.is_empty() || line.len() < 2 {
        return 0.0;
    }
    // The line's points every few metres, bucketed by a grid of COVER_M cells.
    let cell = COVER_M / mpu(line[0].1);
    let mut grid: HashMap<(i64, i64), Vec<P>> = HashMap::default();
    for w in line.windows(2) {
        let n = (metres(w[0], w[1]) / (COVER_M / 4.0)).ceil().max(1.0) as usize;
        for i in 0..=n {
            let t = i as f64 / n as f64;
            let p = (
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            );
            grid.entry(((p.0 / cell).floor() as i64, (p.1 / cell).floor() as i64))
                .or_default()
                .push(p);
        }
    }
    let hit = pts
        .iter()
        .filter(|&&p| {
            let (cx, cy) = ((p.0 / cell).floor() as i64, (p.1 / cell).floor() as i64);
            (-1..=1).any(|dx| {
                (-1..=1).any(|dy| {
                    grid.get(&(cx + dx, cy + dy))
                        .is_some_and(|v| v.iter().any(|&q| metres(p, q) <= COVER_M))
                })
            })
        })
        .count();
    hit as f64 / pts.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_counts_points_near_the_line() {
        let u = 1.0 / mpu(world(0.0, 45.0).1); // world units per metre
        let o = world(0.0, 45.0);
        let at = |e: f64, n: f64| (o.0 + e * u, o.1 - n * u);
        // A relation 1 km east; a route along its first 600 m, 20 m north,
        // which also covers 46 m past its end: 26 of the 40 points.
        let pts: Vec<P> = (0..40).map(|i| at(i as f64 * 25.0 + 12.5, 0.0)).collect();
        let route = vec![at(0.0, 20.0), at(600.0, 20.0)];
        let c = coverage(&pts, &route);
        assert!((c - 0.65).abs() < 1e-9, "{c}");
        // 100 m away covers nothing.
        assert_eq!(coverage(&pts, &[at(0.0, 100.0), at(1000.0, 100.0)]), 0.0);
    }
}
