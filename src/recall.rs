// How many of a park's known hikes the curated routes find: OSM hiking route
// relations (the basemap's hiking_route layer, e.g. the West Coast Trail,
// Lake O'Hara's Alpine Circuit) checked against the routes, as a measure that
// doesn't depend on our own scoring. Only reported, never used to pick routes.
use crate::area::{Area, Class};
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

/// A relation tagged as a hiking route whose name says it's for bikes:
/// Desoto State Park's "Never Never Land Mountain Bike Trail" and "Family
/// Bike Loop" (#42). Routes rightly avoid them, so they're neither checked
/// nor used as names.
pub fn is_bike(k: &Known) -> bool {
    let n = k.name.to_lowercase();
    let words: Vec<&str> = n
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    ["bike", "bikes", "mtb", "bicycle", "biking", "cycling"]
        .iter()
        .any(|w| words.contains(w))
}

/// Each known hike in the park: how much of it the best single route covers.
pub fn check(area: &Area, doc: &Value) -> Vec<Value> {
    let rs = routes(doc);
    let mut out = vec![];
    for k in area.known.iter().filter(|k| !is_bike(k)) {
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
    Near::new(segs).share(pts)
}

/// Segments' points every few metres, bucketed by a grid of COVER_M cells,
/// to ask how many of some points are within COVER_M of them.
struct Near {
    cell: f64,
    grid: HashMap<(i64, i64), Vec<P>>,
}

impl Near {
    fn new(segs: &[(P, P)]) -> Near {
        let cell = COVER_M / mpu(segs.first().map_or(0.0, |s| s.0.1));
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
        Near { cell, grid }
    }

    fn share(&self, pts: &[P]) -> f64 {
        if pts.is_empty() || self.grid.is_empty() {
            return 0.0;
        }
        let cell = self.cell;
        let hit = pts
            .iter()
            .filter(|&&p| {
                let (cx, cy) = ((p.0 / cell).floor() as i64, (p.1 / cell).floor() as i64);
                (-1..=1).any(|dx| {
                    (-1..=1).any(|dy| {
                        self.grid
                            .get(&(cx + dx, cy + dy))
                            .is_some_and(|v| v.iter().any(|&q| metres(p, q) <= COVER_M))
                    })
                })
            })
            .count();
        hit as f64 / pts.len() as f64
    }
}

/// Whether to report each park's named trails (`trails`): --trails.
pub static TRAILS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A named trail is reported when it has at least this much length.
const TRAIL_MIN_M: f64 = 200.0;

/// The hikes a park offers, as (summary, line): day hikes, hikes from base
/// camps and multi-day routes; not base camps' approaches, which aren't
/// offered as hikes in themselves.
fn offered(doc: &Value) -> Vec<(Value, Vec<P>)> {
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
    let summary = |h: &Value, ty: &str, from: &Value, dest: &Value| {
        json!({
            "type": ty, "name": h["name"], "from": from, "dest": dest,
            "kind": h["kind"], "km": h["route"]["km"],
        })
    };
    let mut out = vec![];
    for h in doc["day_hikes"].as_array().into_iter().flatten() {
        out.push((summary(h, "day", &h["from"], &h["dest"]), line(&h["route"])));
    }
    for b in doc["base_camps"].as_array().into_iter().flatten() {
        for h in b["hikes"].as_array().into_iter().flatten() {
            out.push((
                summary(h, "from-camp", &b["camp"], &h["dest"]),
                line(&h["route"]),
            ));
        }
    }
    for t in doc["point_to_points"].as_array().into_iter().flatten() {
        out.push((
            summary(t, "multi-day", &t["from"], &t["to"]),
            line(&t["route"]),
        ));
    }
    out
}

/// Each named trail in the area loaded for the park (its path and track ways
/// of one name, e.g. Killarney's "Granite Ridge Trail"), its share inside the
/// park, and the offered hikes that walk at least PARTIAL of it, best first.
/// Trails outside count too: a park's own list of its trails can cross its
/// boundary (Killarney's Lake of The Woods Trail is in the neighbouring
/// Killarney Lakelands park). For checking well-known trails by name and
/// length as well as by where they go (fixtures/expect.py, routes#95): only
/// reported, never used to pick or name routes. Ways of the same name in
/// different places are one trail. Only with --trails (TRAILS).
pub fn trails(area: &Area, doc: &Value) -> Vec<Value> {
    let g = &area.graph;
    // name -> (segments, metres, metres inside the park)
    let mut by_name: HashMap<&str, (Vec<(P, P)>, f64, f64)> = HashMap::default();
    for (a, es) in g.adj.iter().enumerate() {
        for e in es {
            // Each edge is in both nodes' lists: take it once.
            if (e.to as usize) < a {
                continue;
            }
            let w = &g.ways[e.way as usize];
            if w.name.is_empty() || !matches!(w.class, Class::Trail | Class::Track) {
                continue;
            }
            let (p, q) = (g.pts[a], g.pts[e.to as usize]);
            let t = by_name.entry(w.name.as_str()).or_default();
            let m = metres(p, q);
            t.0.push((p, q));
            t.1 += m;
            if area.in_park(((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0)) {
                t.2 += m;
            }
        }
    }
    let hikes: Vec<(Value, Near, [f64; 4])> = offered(doc)
        .into_iter()
        .filter(|(_, l)| l.len() > 1)
        .map(|(v, l)| {
            let segs: Vec<(P, P)> = l.windows(2).map(|w| (w[0], w[1])).collect();
            let b = bounds(&l);
            (v, Near::new(&segs), b)
        })
        .collect();
    let mut names: Vec<&&str> = by_name.keys().collect();
    names.sort();
    let mut out = vec![];
    for name in names {
        let (segs, m, in_m) = &by_name[*name];
        if *m < TRAIL_MIN_M {
            continue;
        }
        let pts: Vec<P> = segs.iter().flat_map(|&(a, b)| sample(&[a, b])).collect();
        let tb = bounds(&pts);
        let pad = COVER_M / mpu(tb[1]);
        let mut walked: Vec<(f64, &Value)> = hikes
            .iter()
            .filter(|(_, _, hb)| {
                hb[0] <= tb[2] + pad
                    && tb[0] <= hb[2] + pad
                    && hb[1] <= tb[3] + pad
                    && tb[1] <= hb[3] + pad
            })
            .map(|(v, near, _)| (near.share(&pts), v))
            .filter(|(c, _)| *c >= PARTIAL)
            .collect();
        walked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let routes: Vec<Value> = walked
            .into_iter()
            .map(|(c, v)| {
                let mut v = v.clone();
                v["coverage"] = json!((c * 100.0).round() / 100.0);
                v
            })
            .collect();
        out.push(json!({
            "name": name,
            "km": (m / 100.0).round() / 10.0,
            "inside": (in_m / m * 100.0).round() / 100.0,
            "routes": routes,
        }));
    }
    out
}

/// [min x, min y, max x, max y] of some points.
fn bounds(pts: &[P]) -> [f64; 4] {
    pts.iter()
        .fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |b, p| {
            [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
        })
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
            .filter(|(k, _)| !k.name.is_empty() && !is_bike(k))
            .map(|(k, rp)| (k, coverage(rp, &line), coverage_segs(&route_pts, &k.segs)))
            .filter(|&(_, rel, route)| rel >= NAME_RELATION && route >= NAME_ROUTE)
            .max_by(|a, b| (a.1 * a.2).partial_cmp(&(b.1 * b.2)).unwrap());
        if let Some((k, _, _)) = best {
            r["name"] = json!(k.name);
            // The relation's segments run in its member order (basemap's
            // hiking-routes export), which says which way the route goes
            // (#73): a loop or point-to-point that follows it goes that way.
            if runs_against(r["kind"].as_str() == Some("loop"), &line, &k.segs) {
                reverse(r);
            }
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

/// How far along a line a point is (metres), and how far from it.
fn along(line: &[P], p: P) -> (f64, f64) {
    let (mut best, mut at, mut acc) = (f64::MAX, 0.0, 0.0);
    for w in line.windows(2) {
        let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
        let l2 = dx * dx + dy * dy;
        let t = if l2 == 0.0 { 0.0 } else { (((p.0 - w[0].0) * dx + (p.1 - w[0].1) * dy) / l2).clamp(0.0, 1.0) };
        let q = (w[0].0 + t * dx, w[0].1 + t * dy);
        let d = metres(p, q);
        if d < best {
            best = d;
            at = acc + t * metres(w[0], w[1]);
        }
        acc += metres(w[0], w[1]);
    }
    (at, best)
}

/// A relation's segment this close to a route (both ends) is on it.
const ORDER_NEAR_M: f64 = 150.0;

/// Whether a route runs against a relation: the relation's segments, which
/// run in its member order, mostly point backwards along the route. For a
/// loop, along is cyclic: a segment points ahead if its end is less than half
/// the loop on from its start. Needs 3 segments on the route, and a clear
/// majority.
fn runs_against(is_loop: bool, line: &[P], segs: &[(P, P)]) -> bool {
    let len: f64 = line.windows(2).map(|w| metres(w[0], w[1])).sum();
    if len == 0.0 {
        return false;
    }
    let (mut fwd, mut back, mut on) = (0usize, 0usize, 0usize);
    for &(a, b) in segs {
        let ((at_a, da), (at_b, db)) = (along(line, a), along(line, b));
        if da > ORDER_NEAR_M || db > ORDER_NEAR_M {
            continue;
        }
        on += 1;
        let d = if is_loop { (at_b - at_a).rem_euclid(len) } else { at_b - at_a };
        if d == 0.0 {
            continue;
        }
        let ahead = if is_loop { d < len / 2.0 } else { d > 0.0 };
        if ahead { fwd += 1 } else { back += 1 }
    }
    on >= 3 && back > 2 * fwd
}

/// Turn a route round: its line, stopovers (and their distances), gain and
/// loss, the trails and sights it passes in order, and a point-to-point's
/// ends. A loop keeps its start.
fn reverse(r: &mut Value) {
    let is_loop = r["kind"].as_str() == Some("loop");
    let route = &mut r["route"];
    let km = route["km"].as_f64().unwrap_or(0.0);
    if let Some(l) = route["line"].as_array_mut() {
        l.reverse();
    }
    if let Some(st) = route["stopovers"].as_array_mut() {
        for s in st.iter_mut() {
            if let Some(k) = s["km"].as_f64() {
                s["km"] = json!(((km - k) * 10.0).round() / 10.0);
            }
        }
        st.reverse();
    }
    let (g, l) = (route["gain"].clone(), route["loss"].clone());
    route["gain"] = l;
    route["loss"] = g;
    if let Some(v) = route["via"].as_array_mut() {
        v.reverse();
    }
    for p in route["passes"].as_array_mut().into_iter().flatten() {
        if let Some(names) = p[1].as_array_mut() {
            names.reverse();
        }
    }
    if !is_loop {
        let (f, t) = (r["from"].clone(), r["to"].clone());
        if !t.is_null() {
            r["from"] = t;
            r["to"] = f;
        }
    }
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

/// Day hikes whose trailhead is named for where they go read twice:
/// "Coastal Trail from Coastal Trail trailhead", "Gros Morne from Gros Morne
/// Mountain Trail trailhead" (#47). They're named for the destination alone,
/// unless another route in the park goes there too or already has that
/// name: then "from" tells them apart.
/// A name without a trailing "(...)": "mount edith cavell (upper lot)" ->
/// "mount edith cavell".
fn strip_paren(s: &str) -> String {
    match s
        .strip_suffix(')')
        .and_then(|t| t.rfind(" (").map(|i| &t[..i]))
    {
        Some(t) if !t.is_empty() => t.trim().to_string(),
        _ => s.to_string(),
    }
}

/// Multi-day loops are titled "<start> to <start>" (#55): Jasper's
/// "Parking off Maligne Lake Road to Parking off Maligne Lake Road". Name
/// an unnamed one for its turnaround, the stopover farthest from the start
/// as the crow flies: "Tekarra loop from Parking off Maligne Lake Road".
pub fn name_loops(doc: &mut Value) {
    for t in doc["point_to_points"].as_array_mut().into_iter().flatten() {
        if t["name"].is_string() {
            continue;
        }
        let (Some(from), Some(to)) = (t["from"].as_str(), t["to"].as_str()) else {
            continue;
        };
        if from != to || from.is_empty() {
            continue;
        }
        let start = &t["route"]["line"][0];
        let (Some(x0), Some(y0)) = (start[0].as_f64(), start[1].as_f64()) else {
            continue;
        };
        let far = t["route"]["stopovers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let (x, y) = (s["at"][0].as_f64()?, s["at"][1].as_f64()?);
                let k = y0.to_radians().cos();
                Some((
                    ((x - x0) * k).powi(2) + (y - y0).powi(2),
                    s["name"].as_str()?,
                ))
            })
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if let Some((_, stop)) = far {
            t["name"] = json!(format!("{stop} loop from {from}"));
        }
    }
}

/// What a start's name can end in after the trail or place it's named for.
const START_SUFFIXES: [&str; 5] = [
    " trailhead",
    " trail head",
    " parking lot",
    " parking",
    " car park",
];

pub fn drop_redundant_from(doc: &mut Value) {
    let lower = |s: &str| s.trim().to_lowercase();
    // What each day hike would be called, to check a shorter name is unique.
    let hikes = doc["day_hikes"].as_array().cloned().unwrap_or_default();
    let mut titles: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for h in &hikes {
        if let Some(n) = h["name"].as_str() {
            *titles.entry(lower(n)).or_default() += 1;
        }
        if let Some(d) = h["dest"].as_str() {
            *titles.entry(lower(d)).or_default() += 1;
        }
    }
    for h in doc["day_hikes"].as_array_mut().into_iter().flatten() {
        if h["name"].is_string() {
            continue;
        }
        let (Some(dest), Some(from)) = (h["dest"].as_str(), h["from"].as_str()) else {
            continue;
        };
        let d = lower(dest);
        // A parenthetical on the start says which lot, not where you go:
        // "Mount Edith Cavell (upper lot)" (#55).
        let f = strip_paren(&lower(from));
        // A start named for the destination: exactly ("Skyline Trail from
        // Skyline Trail", a trailhead node named for its trail, #51), or with
        // a trailhead or parking suffix and maybe more of a trail name
        // ("Gros Morne from Gros Morne Mountain Trail trailhead").
        let suffixed = START_SUFFIXES.iter().find_map(|s| f.strip_suffix(s));
        let redundant = !d.is_empty()
            && (f == d
                || suffixed.is_some_and(|core| {
                    core == d
                        || core
                            .strip_prefix(&d)
                            .is_some_and(|rest| rest.starts_with(' '))
                }));
        if redundant && titles.get(&d) == Some(&1) {
            h["name"] = json!(dest);
        }
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
    fn bike_relations_are_not_hikes() {
        let k = |name: &str| Known {
            id: String::new(),
            name: name.to_string(),
            network: String::new(),
            wikidata: String::new(),
            roundtrip: String::new(),
            total_km: 0.0,
            segs: vec![],
        };
        assert!(is_bike(&k("Never Never Land Mountain Bike Trail")));
        assert!(is_bike(&k("Family Bike Loop")));
        assert!(is_bike(&k("Knotty Pine MTB Trail")));
        assert!(!is_bike(&k("Bikeway Ridge Hike"))); // a whole word only
        assert!(!is_bike(&k("Lake O'Hara Circuit")));
    }

    #[test]
    fn trailhead_named_for_the_destination_is_dropped() {
        let mut doc = json!({"day_hikes": [
            {"dest": "Coastal Trail", "from": "Coastal Trail trailhead"},
            {"dest": "Gros Morne", "from": "Gros Morne Mountain Trail trailhead"},
            {"dest": "Baker Lake", "from": "Bakerview Trail trailhead"},
            {"dest": "Western Brook Pond", "from": "Parking off Route 430"},
            {"dest": "Green Gardens", "from": "Green Gardens Trail trailhead"},
            {"dest": "Green Gardens", "from": "Long Pond Trail trailhead"},
            {"dest": "Lookout Hills", "from": "Lookout Hills trailhead", "name": "Lookout Trail"},
            {"dest": "Skyline Trail", "from": "Skyline Trail"},
            {"dest": "Fundy Footpath", "from": "Fundy Footpath Parking"},
            {"dest": "Cascade Falls", "from": "Cascade Falls Trail Head"},
            {"dest": "Gros Ventre", "from": "Gros Ventre Campground"},
            {"dest": "Mount Edith Cavell", "from": "Mount Edith Cavell (upper lot)"},
        ]});
        drop_redundant_from(&mut doc);
        let names: Vec<Option<&str>> = doc["day_hikes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                Some("Coastal Trail"),
                Some("Gros Morne"),
                None, // "Bakerview" isn't "Baker Lake"
                None, // not a trailhead name
                None, // two routes to Green Gardens: "from" tells them apart
                None,
                Some("Lookout Trail"),      // already named
                Some("Skyline Trail"),      // a start named exactly for it (#51)
                Some("Fundy Footpath"),     // ... or with a parking suffix
                Some("Cascade Falls"),      // ... or "trail head"
                None,                       // a campground isn't a trailhead name
                Some("Mount Edith Cavell"), // a parenthetical on the start (#55)
            ]
        );
    }

    #[test]
    fn multi_day_loops_are_named_for_their_turnaround() {
        let stop = |name: &str, x: f64, y: f64| json!({"name": name, "km": 0, "at": [x, y]});
        let mut doc = json!({"point_to_points": [
            {"from": "Lot", "to": "Lot", "route": {"line": [[0.0, 0.0], [0.1, 0.1]],
              "stopovers": [stop("Near", 0.01, 0.0), stop("Far", 0.1, 0.1), stop("Middle", 0.05, 0.0)]}},
            {"from": "Lot", "to": "Lot", "name": "La Cloche Silhouette Trail", "route": {"line": [[0.0, 0.0]], "stopovers": [stop("Far", 1.0, 1.0)]}},
            {"from": "A", "to": "B", "route": {"line": [[0.0, 0.0]], "stopovers": [stop("Far", 1.0, 1.0)]}},
            {"from": "Lot", "to": "Lot", "route": {"line": [[0.0, 0.0]], "stopovers": []}},
        ]});
        name_loops(&mut doc);
        let names: Vec<Option<&str>> = doc["point_to_points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                Some("Far loop from Lot"),
                Some("La Cloche Silhouette Trail"),
                None,
                None
            ]
        );
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

    #[test]
    fn offered_hikes_leave_out_approaches() {
        let r = |km: f64| json!({"km": km, "line": [[0.0, 45.0], [0.01, 45.0]]});
        let doc = json!({
            "day_hikes": [{"from": "Lot", "dest": "Lake", "kind": "loop", "route": r(4.0)}],
            "base_camps": [{
                "camp": "H3", "from": "Lot", "approach": r(4.6),
                "hikes": [{"dest": "Peak", "kind": "out-and-back", "route": r(9.7)}],
            }],
            "point_to_points": [{"from": "Lot", "to": "Lot", "name": "Loop Trail", "route": r(73.0)}],
        });
        let got: Vec<(String, String, String, f64)> = offered(&doc)
            .iter()
            .map(|(v, l)| {
                assert_eq!(l.len(), 2);
                let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
                (s("type"), s("from"), s("dest"), v["km"].as_f64().unwrap())
            })
            .collect();
        let want = [
            ("day", "Lot", "Lake", 4.0),
            ("from-camp", "H3", "Peak", 9.7),
            ("multi-day", "Lot", "Lot", 73.0),
        ]
        .map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d));
        assert_eq!(got, want);
        assert_eq!(offered(&doc)[2].0["name"], "Loop Trail");
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;

    // A square loop ~2 km a side, clockwise on the map (y grows south).
    fn square() -> Vec<P> {
        let (x, y, d) = (0.25 * geo::WORLD, 0.35 * geo::WORLD, 2000.0 / mpu(0.35 * geo::WORLD));
        vec![(x, y), (x + d, y), (x + d, y + d), (x, y + d), (x, y)]
    }

    fn segs(pts: &[P]) -> Vec<(P, P)> {
        pts.windows(2).map(|w| (w[0], w[1])).collect()
    }

    fn walk(line: &[P], n: usize) -> Vec<P> {
        // n points evenly along a closed line's corners, in its order.
        let mut out = vec![];
        for w in line.windows(2) {
            for i in 0..n {
                let t = i as f64 / n as f64;
                out.push((w[0].0 + (w[1].0 - w[0].0) * t, w[0].1 + (w[1].1 - w[0].1) * t));
            }
        }
        out
    }

    #[test]
    fn loop_against_its_relation() {
        let line = square();
        let mut rev = line.clone();
        rev.reverse();
        let segs = segs(&walk(&line, 4));
        assert!(!runs_against(true, &line, &segs));
        assert!(runs_against(true, &rev, &segs));
        // Starting elsewhere on the loop doesn't matter, only the direction.
        let shifted: Vec<(P, P)> = segs[5..].iter().chain(segs[..5].iter()).copied().collect();
        assert!(!runs_against(true, &line, &shifted));
    }

    #[test]
    fn point_to_point_against_its_relation() {
        let line: Vec<P> = square()[..3].to_vec();
        let segs = segs(&walk(&line, 4));
        let mut rev = line.clone();
        rev.reverse();
        assert!(!runs_against(false, &line, &segs));
        assert!(runs_against(false, &rev, &segs));
        // A relation elsewhere says nothing.
        let far: Vec<(P, P)> = segs.iter().map(|&(a, b)| ((a.0 + 1e5, a.1), (b.0 + 1e5, b.1))).collect();
        assert!(!runs_against(false, &rev, &far));
    }

    #[test]
    fn reversing_a_point_to_point() {
        let mut r = json!({
            "kind": "point-to-point", "from": "A", "to": "B",
            "route": {"km": 10.0, "gain": 500.0, "loss": 100.0, "line": [[0, 0], [1, 1]],
                      "stopovers": [{"name": "c1", "km": 3.0}, {"name": "c2", "km": 7.5}],
                      "via": ["x", "y"], "passes": [["lake", ["l1", "l2"]]]}
        });
        reverse(&mut r);
        assert_eq!(r["from"], "B");
        assert_eq!(r["to"], "A");
        let route = &r["route"];
        assert_eq!(route["gain"], 100.0);
        assert_eq!(route["loss"], 500.0);
        assert_eq!(route["line"][0], json!([1, 1]));
        assert_eq!(route["stopovers"][0]["name"], "c2");
        assert_eq!(route["stopovers"][0]["km"], 2.5);
        assert_eq!(route["stopovers"][1]["km"], 7.0);
        assert_eq!(route["via"], json!(["y", "x"]));
        assert_eq!(route["passes"][0][1], json!(["l2", "l1"]));
    }
}
