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
    pub wikidata: String,
    pub roundtrip: String,
    /// The whole relation's length, from the basemap's export (0 if the
    /// build predates it): the tiles around a park only show part of it.
    pub total_km: f64,
    pub segs: Vec<(P, P)>,
}

/// Within this of the park (or in it) counts as the park's, for sampling a
/// relation's stretch that a park's routes could cover.
const NEAR_PARK_M: f64 = 5000.0;
/// A relation is checked only if at least this share of it is inside the
/// park: routes stay mostly in the park, and in the oracle sample most
/// relations within 5 km of a park were outside it (534 of 843 under 10%
/// inside), so routes were being graded on trails they never aim to cover.
const MIN_INSIDE: f64 = 0.5;
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
        let (mut near_m, mut all_m, mut in_m) = (0.0, 0.0, 0.0);
        for &(a, b) in &k.segs {
            let m = metres(a, b);
            all_m += m;
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            if area.park_dist(mid, NEAR_PARK_M).is_none() {
                continue;
            }
            near_m += m;
            if area.in_park(mid) {
                in_m += m;
            }
            let n = (m / STEP_M).ceil().max(1.0) as usize;
            for i in 0..n {
                let t = (i as f64 + 0.5) / n as f64;
                pts.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
            }
        }
        let km = near_m / 1000.0;
        // Mostly elsewhere (a long-distance trail passing by), or too short
        // or long to be one of our routes. The whole relation's length, where
        // the build has it: the loaded tiles only reach 5 km past the park,
        // so measured from them, a long trail looks mostly local.
        let whole_m = all_m.max(k.total_km * 1000.0);
        if near_m < whole_m * 0.5
            || in_m < whole_m * MIN_INSIDE
            || whole_m > MAX_KM * 1000.0
            || !(MIN_KM..=MAX_KM).contains(&km)
        {
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
            "wikidata": k.wikidata, "roundtrip": k.roundtrip,
            "total_km": (k.total_km * 10.0).round() / 10.0,
            "km": (km * 10.0).round() / 10.0,
            "coverage": (cov * 100.0).round() / 100.0,
            "best": title,
        }));
    }
    out
}

/// The share of `pts` within COVER_M of `line`.
fn coverage(pts: &[P], line: &[P]) -> f64 {
    let segs: Vec<(P, P)> = line.windows(2).map(|w| (w[0], w[1])).collect();
    coverage_segs(pts, &segs)
}

/// The share of `pts` within COVER_M of any of `segs`.
fn coverage_segs(pts: &[P], segs: &[(P, P)]) -> f64 {
    if pts.is_empty() || segs.is_empty() {
        return 0.0;
    }
    // The segments' points every few metres, bucketed by a grid of COVER_M
    // cells.
    let cell = COVER_M / mpu(segs[0].0.1);
    let mut grid: HashMap<(i64, i64), Vec<P>> = HashMap::default();
    for &(a, b) in segs {
        let n = (metres(a, b) / (COVER_M / 4.0)).ceil().max(1.0) as usize;
        for i in 0..=n {
            let t = i as f64 / n as f64;
            let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
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

/// Points every STEP_M along a line.
fn sample(line: &[P]) -> Vec<P> {
    let mut pts = vec![];
    for w in line.windows(2) {
        let n = (metres(w[0], w[1]) / STEP_M).ceil().max(1.0) as usize;
        for i in 0..n {
            let t = (i as f64 + 0.5) / n as f64;
            pts.push((
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            ));
        }
    }
    pts
}

/// A route takes a relation's name when it covers this share of the
/// relation (as "found") and this share of the route is on it: a hike that
/// follows a named trail most of the way and then goes on (Colchuck Lake
/// Trail, then up Dragontail Peak) is still the summit's hike.
const NAME_RELATION: f64 = FOUND;
const NAME_ROUTE: f64 = 0.85;

/// Name the park's chosen routes after the OSM hiking route they follow
/// ("Lake O'Hara Circuit Trail", not "Mount Lefroy from Lake O'Hara"): each
/// day hike, hike from a base camp and multi-day route gets `name` when one
/// relation qualifies (the best, if several). Only routes already chosen, so
/// the relations don't change which routes are picked, and the known-hikes
/// check stays a measure.
pub fn name_routes(area: &Area, doc: &mut Value) {
    if area.known.is_empty() {
        return;
    }
    let rel_pts: Vec<Vec<P>> = area
        .known
        .iter()
        .map(|k| k.segs.iter().flat_map(|&(a, b)| sample(&[a, b])).collect())
        .collect();
    let rel_box: Vec<(P, P)> = area
        .known
        .iter()
        .map(|k| {
            k.segs.iter().fold(
                ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN)),
                |(lo, hi), &(a, b)| {
                    (
                        (lo.0.min(a.0).min(b.0), lo.1.min(a.1).min(b.1)),
                        (hi.0.max(a.0).max(b.0), hi.1.max(a.1).max(b.1)),
                    )
                },
            )
        })
        .collect();
    let name = |r: &mut Value| {
        let line: Vec<P> = r["route"]["line"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| Some(world(c[0].as_f64()?, c[1].as_f64()?)))
                    .collect()
            })
            .unwrap_or_default();
        if line.len() < 2 {
            return;
        }
        let route_pts = sample(&line);
        // Relations whose box can't touch the route's are skipped.
        let pad = 100.0 / mpu(line[0].1);
        let (mut lo, mut hi) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
        for &p in &line {
            lo = (lo.0.min(p.0 - pad), lo.1.min(p.1 - pad));
            hi = (hi.0.max(p.0 + pad), hi.1.max(p.1 + pad));
        }
        let best = area
            .known
            .iter()
            .zip(&rel_pts)
            .zip(&rel_box)
            .filter(|(_, b)| b.0.0 <= hi.0 && b.1.0 >= lo.0 && b.0.1 <= hi.1 && b.1.1 >= lo.1)
            .map(|(k, _)| k)
            .filter(|(k, _)| !k.name.is_empty())
            .map(|(k, rp)| (k, coverage(rp, &line), coverage_segs(&route_pts, &k.segs)))
            .filter(|&(_, rel, route)| rel >= NAME_RELATION && route >= NAME_ROUTE)
            .max_by(|a, b| (a.1 * a.2).partial_cmp(&(b.1 * b.2)).unwrap());
        if let Some((k, _, _)) = best {
            r["name"] = json!(k.name);
        }
    };
    for h in doc["day_hikes"].as_array_mut().into_iter().flatten() {
        name(h);
    }
    for b in doc["base_camps"].as_array_mut().into_iter().flatten() {
        for h in b["hikes"].as_array_mut().into_iter().flatten() {
            name(h);
        }
    }
    for t in doc["point_to_points"].as_array_mut().into_iter().flatten() {
        name(t);
    }
    disambiguate(doc);
}

/// Two routes in a park named after the same relation (Joffre Lakes Trail,
/// out to Upper Joffre Lake from the trailhead, and to Lower Joffre Lake
/// from camp) each add where they go: "Joffre Lakes Trail to Upper Joffre
/// Lake". A multi-day route adds its far end, or for a loop, where it starts.
fn disambiguate(doc: &mut Value) {
    let mut routes: Vec<&mut Value> = vec![];
    let obj = doc.as_object_mut().unwrap();
    let mut base = obj.remove("base_camps");
    for (k, v) in obj.iter_mut() {
        if k == "day_hikes" || k == "point_to_points" {
            routes.extend(v.as_array_mut().into_iter().flatten());
        }
    }
    for b in base
        .as_mut()
        .and_then(|b| b.as_array_mut())
        .into_iter()
        .flatten()
    {
        routes.extend(b["hikes"].as_array_mut().into_iter().flatten());
    }
    let mut count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in routes.iter() {
        if let Some(n) = r["name"].as_str() {
            *count.entry(n.to_string()).or_default() += 1;
        }
    }
    for r in routes.iter_mut() {
        let Some(n) = r["name"].as_str().map(str::to_string) else {
            continue;
        };
        if count[&n] < 2 {
            continue;
        }
        let to = r["dest"]
            .as_str()
            .or_else(|| r["to"].as_str())
            .unwrap_or("");
        let from = r["from"].as_str().unwrap_or("");
        if r["dest"].is_null() && to == from && !from.is_empty() {
            // A multi-day loop: back where it started.
            r["name"] = json!(format!("{n} from {from}"));
        } else if !to.is_empty() && to != n {
            r["name"] = json!(format!("{n} to {to}"));
        }
    }
    if let Some(b) = base {
        obj.insert("base_camps".to_string(), b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_names_add_where_each_goes() {
        let mut doc = json!({
            "day_hikes": [
                {"dest": "Upper Joffre Lake", "name": "Joffre Lakes Trail"},
                {"dest": "Nairn Falls", "name": "Nairn Falls Trail"},
                {"dest": "Cerise Creek"},
            ],
            "base_camps": [{"hikes": [{"dest": "Lower Joffre Lake", "name": "Joffre Lakes Trail"}]}],
            "point_to_points": [],
        });
        disambiguate(&mut doc);
        assert_eq!(
            doc["day_hikes"][0]["name"],
            "Joffre Lakes Trail to Upper Joffre Lake"
        );
        assert_eq!(
            doc["base_camps"][0]["hikes"][0]["name"],
            "Joffre Lakes Trail to Lower Joffre Lake"
        );
        assert_eq!(doc["day_hikes"][1]["name"], "Nairn Falls Trail");
        assert!(doc["day_hikes"][2].get("name").is_none());
    }

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
