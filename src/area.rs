// Everything about one park that curation needs, read from the basemap's z14
// tiles: the trail and road network as a graph, land cover, the park's own
// outline, and destinations, campsites and trailheads.
use crate::geo::{self, EXT, P, Poly, Z, metres, mpu};
use crate::mvt;
use crate::pmtiles::PmTiles;
use rayon::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Trail,
    Track,
    Road,
    Join, // joins the two copies of a line cut at a tile edge
}

pub struct Way {
    pub name: String,
    pub class: Class,
    pub bike: bool,
    pub walk: bool, // foot=designated, or on a hiking route relation
}

#[derive(Clone, Copy)]
pub struct Edge {
    pub to: u32,
    pub m: f32,
    pub cost: f32,
    pub way: u32,
}

#[derive(Default)]
pub struct Graph {
    pub pts: Vec<P>,
    pub adj: Vec<Vec<Edge>>,
    pub ways: Vec<Way>,
}

// Land cover classes; when polygons overlap, the earliest wins.
pub const LANDCOVER: [&str; 7] = [
    "glacier",
    "water",
    "alpine rock",
    "meadow",
    "scrub",
    "wetland",
    "forest",
];
pub const OTHER: u8 = 255;
pub fn alpine(c: u8) -> bool {
    c <= 3 && c != 1
}

fn landcover(key: &str, v: &str) -> Option<u8> {
    match (key, v) {
        ("natural", "glacier") => Some(0),
        ("natural", "bare_rock" | "scree" | "shingle") => Some(2),
        ("natural", "heath" | "grassland") | ("landuse", "meadow" | "grass") => Some(3),
        ("natural", "scrub") => Some(4),
        ("wetland", _) => Some(5),
        ("natural", "wood") | ("landuse", "forest") => Some(6),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Peak,
    Viewpoint,
    Lake,
    Waterfall,
    Spring,
    Hut,
    /// A named trail walked for itself (see Ctx::named_trails): a loop, or
    /// out and back to its far end, with no destination on it needed.
    Trail,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Peak => "peak",
            Kind::Viewpoint => "viewpoint",
            Kind::Lake => "lake",
            Kind::Waterfall => "waterfall",
            Kind::Spring => "spring",
            Kind::Hut => "hut",
            Kind::Trail => "trail",
        }
    }
    pub fn appeal(self) -> f64 {
        match self {
            Kind::Peak => 1.0,
            Kind::Viewpoint => 0.9,
            Kind::Lake => 0.8,
            Kind::Waterfall => 0.75,
            // Named springs: desert oases (Joshua Tree's Lost Palms Oasis is
            // natural=spring) and the like.
            Kind::Spring => 0.6,
            Kind::Hut => 0.4,
            Kind::Trail => TRAIL_APPEAL,
        }
    }
}

/// Appeal of a named trail walked for itself (Kind::Trail). Tuned on the
/// oracle sample's tuning half (fixtures/oracle.py).
pub const TRAIL_APPEAL: f64 = 0.6;

pub struct Dest {
    pub name: String,
    pub kind: Kind,
    pub pt: P,
    pub area: f64,
    pub wikidata: String,
    pub qrank: f64,
    pub lake: Option<u32>, // for lakes: which lake (index into Area::lakes)
    /// A peak with neither an elevation nor a Wikidata item: often a
    /// climbing crag mapped as a peak (Joshua Tree's Cyclops Rock, Sports
    /// Challenge Rock), less of a draw than a summit.
    pub minor: bool,
}

pub struct Place {
    pub name: String,
    pub pt: P,
    pub trailhead: bool,
}

#[derive(Default)]
pub struct TileData {
    pub land: Vec<(u8, Poly)>,
    pub park: Vec<Poly>,
    pub lakes: Vec<(u32, Poly)>,
}

pub struct Area {
    pub tiles: HashMap<(u32, u32), TileData>,
    pub graph: Graph,
    pub dests: Vec<Dest>,
    pub camps: Vec<Place>,
    pub parkings: Vec<Place>,
    /// Hiking route relations (the hiking_route layer) passing through.
    pub known: Vec<crate::recall::Known>,
}

struct Raw {
    tx: u32,
    ty: u32,
    data: TileData,
    lines: Vec<(Way, Vec<P>)>,
    dests: Vec<Dest>,
    camps: Vec<Place>,
    parkings: Vec<Place>,
    known: Vec<crate::recall::Known>, // pieces of relations in this tile
}

fn read_tile(
    archives: &[PmTiles],
    tx: u32,
    ty: u32,
    park_id: &str,
    lake_ids: &std::sync::Mutex<HashMap<(String, i64), u32>>,
) -> Raw {
    let mut raw = Raw {
        tx,
        ty,
        data: TileData::default(),
        lines: vec![],
        dests: vec![],
        camps: vec![],
        parkings: vec![],
        known: vec![],
    };
    let s = |x: i32, y: i32, ext: u32| -> P {
        (
            tx as f64 * EXT + x as f64 * EXT / ext as f64,
            ty as f64 * EXT + y as f64 * EXT / ext as f64,
        )
    };
    for a in archives {
        let Some(bytes) = a.get(Z, tx, ty).expect("reading tile") else {
            continue;
        };
        for l in mvt::decode(&bytes) {
            for f in &l.features {
                match l.name.as_str() {
                    "path" | "road" if f.typ == 2 => {
                        let kind = l.str(f, "kind");
                        let class = if l.name == "path" {
                            // Ferries and cable cars on trails (e.g. the West
                            // Coast Trail's Nitinat Narrows): walkable, at a
                            // track's cost.
                            if matches!(kind, "cycleway" | "ferry" | "cable_car") {
                                Class::Track
                            } else {
                                Class::Trail
                            }
                        } else {
                            let class = match kind {
                                "track" => Class::Track,
                                "service" | "unclassified" | "residential" | "living_street"
                                | "tertiary" | "tertiary_link" | "secondary" | "secondary_link"
                                | "road" => Class::Road,
                                _ => continue,
                            };
                            // Roads closed to private cars (e.g. the Lake O'Hara
                            // Fire Road, bus only) are walked like tracks: a
                            // campsite beside one isn't drive-in, and a trail
                            // meeting one isn't a trailhead.
                            let closed = l
                                .get(f, "closed_to_cars")
                                .is_some_and(|v| matches!(v, mvt::Value::Bool(true)));
                            if closed && class == Class::Road {
                                Class::Track
                            } else {
                                class
                            }
                        };
                        let foot = l.str(f, "foot");
                        // Paths walkers may not use, e.g. bike-only trails
                        // (foot=no), which the basemap keeps so the map can
                        // show them. Curated hikes must never use them, not
                        // just score them lower like other bike trails.
                        if matches!(foot, "no" | "private" | "use_sidepath") {
                            continue;
                        }
                        // Closed trails (the basemap's closed flag: access=no,
                        // or a 'Trail Closed' name), e.g. in Gros Morne.
                        if l.get(f, "closed")
                            .is_some_and(|v| matches!(v, mvt::Value::Bool(true)))
                        {
                            continue;
                        }
                        // Bike trails. Graded for mountain bikes (mtb) is a
                        // bike trail unless it's designated for walking or on
                        // a hiking route: foot=yes only says walkers may use
                        // it, and Whistler's bike park is mostly foot=yes. A
                        // path designated for bikes is one unless walkers are
                        // at least allowed (a shared-use path).
                        let flag = |k: &str| {
                            l.get(f, k)
                                .is_some_and(|v| matches!(v, mvt::Value::Bool(true)))
                        };
                        let walk = foot == "designated" || flag("on_route");
                        let bike = !walk
                            && (flag("mtb")
                                || (l.str(f, "bicycle") == "designated" && foot != "yes"));
                        for line in f.geometry() {
                            let pts: Vec<P> =
                                line.into_iter().map(|(x, y)| s(x, y, l.extent)).collect();
                            raw.lines.push((
                                Way {
                                    name: l.str(f, "name").to_string(),
                                    class,
                                    bike,
                                    walk,
                                },
                                pts,
                            ));
                        }
                    }
                    "land" if f.typ == 3 => {
                        let c = ["natural", "landuse", "wetland"]
                            .iter()
                            .find_map(|k| l.get(f, k).and_then(|v| landcover(k, v.as_str())));
                        if let Some(c) = c {
                            raw.data.land.push((
                                c,
                                Poly::new(geo::rings(f.geometry(), tx, ty, l.extent), tx, ty),
                            ));
                        }
                    }
                    "water_poly" if f.typ == 3 => {
                        let rings = geo::rings(f.geometry(), tx, ty, l.extent);
                        let name = l.str(f, "name");
                        let area = l.get(f, "area").map(|v| v.as_f64()).unwrap_or(0.0);
                        // River areas are in this layer too, as kind=lake; skip them by name.
                        let river = ["River", "Creek", "Brook", "Stream", "Channel", "Slough"]
                            .iter()
                            .any(|w| name.ends_with(w) || name.contains(&format!("{w} ")));
                        if !name.is_empty()
                            && !river
                            && l.str(f, "kind") == "lake"
                            && area >= 20000.0
                        {
                            let key = (name.to_string(), area as i64);
                            let (id, new) = {
                                let mut m = lake_ids.lock().unwrap();
                                let n = m.len() as u32;
                                let id = *m.entry(key).or_insert(n);
                                (id, id == n)
                            };
                            let poly = Poly::new(rings.clone(), tx, ty);
                            if new {
                                let c = (
                                    (poly.min.0 + poly.max.0) / 2.0,
                                    (poly.min.1 + poly.max.1) / 2.0,
                                );
                                raw.dests.push(Dest {
                                    name: name.to_string(),
                                    kind: Kind::Lake,
                                    pt: c,
                                    area,
                                    wikidata: l.str(f, "wikidata").to_string(),
                                    qrank: 1.0,
                                    lake: Some(id),
                                    minor: false,
                                });
                            }
                            raw.data.lakes.push((id, poly));
                        }
                        raw.data.land.push((1, Poly::new(rings, tx, ty)));
                    }
                    "hiking_route" if f.typ == 2 => {
                        // Segments whose midpoint is in this tile: the tile's
                        // buffer repeats its neighbours'.
                        let e = l.extent as i32;
                        let mut segs = vec![];
                        for part in f.geometry() {
                            for w in part.windows(2) {
                                let (mx, my) = (w[0].0 + w[1].0, w[0].1 + w[1].1);
                                if mx < 0 || my < 0 || mx >= 2 * e || my >= 2 * e {
                                    continue;
                                }
                                segs.push((
                                    s(w[0].0, w[0].1, l.extent),
                                    s(w[1].0, w[1].1, l.extent),
                                ));
                            }
                        }
                        if !segs.is_empty() {
                            raw.known.push(crate::recall::Known {
                                id: l.str(f, "osm_id").to_string(),
                                name: l.str(f, "name").to_string(),
                                network: l.str(f, "network").to_string(),
                                wikidata: l.str(f, "wikidata").to_string(),
                                roundtrip: l.str(f, "roundtrip").to_string(),
                                total_km: l.get(f, "km").map(|v| v.as_f64()).unwrap_or(0.0),
                                segs,
                            });
                        }
                    }
                    "park" | "trail_area" if f.typ == 3 && l.str(f, "osm_id") == park_id => {
                        raw.data.park.push(Poly::new(
                            geo::rings(f.geometry(), tx, ty, l.extent),
                            tx,
                            ty,
                        ));
                    }
                    "peak" | "poi" if f.typ == 1 => {
                        let name = l.str(f, "name");
                        let Some(&(x, y)) = f.geometry().first().and_then(|g| g.first()) else {
                            continue;
                        };
                        // Skip copies in the tile's buffer: the neighbour has them.
                        if x < 0 || y < 0 || x >= l.extent as i32 || y >= l.extent as i32 {
                            continue;
                        }
                        let pt = s(x, y, l.extent);
                        let tourism = l.str(f, "tourism");
                        let amenity = l.str(f, "amenity");
                        if amenity == "parking" || l.str(f, "highway") == "trailhead" {
                            raw.parkings.push(Place {
                                name: name.to_string(),
                                pt,
                                trailhead: l.str(f, "highway") == "trailhead",
                            });
                        }
                        if name.is_empty() {
                            continue;
                        }
                        if tourism == "camp_site" || tourism == "wilderness_hut" {
                            raw.camps.push(Place {
                                name: name.to_string(),
                                pt,
                                trailhead: false,
                            });
                        }
                        let kind = if l.name == "peak" {
                            Some(Kind::Peak)
                        } else if tourism == "viewpoint" {
                            Some(Kind::Viewpoint)
                        } else if l.str(f, "waterway") == "waterfall" {
                            Some(Kind::Waterfall)
                        } else if l.str(f, "natural") == "spring" {
                            Some(Kind::Spring)
                        } else if tourism == "wilderness_hut"
                            || tourism == "alpine_hut"
                            || amenity == "shelter"
                        {
                            Some(Kind::Hut)
                        } else {
                            None
                        };
                        if let Some(kind) = kind {
                            raw.dests.push(Dest {
                                name: name.to_string(),
                                kind,
                                pt,
                                area: 0.0,
                                wikidata: l.str(f, "wikidata").to_string(),
                                qrank: 1.0,
                                lake: None,
                                minor: kind == Kind::Peak
                                    && l.get(f, "ele").is_none()
                                    && l.str(f, "wikidata").is_empty(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    raw
}

impl Graph {
    fn build(raws: &[Raw]) -> Graph {
        let mut g = Graph::default();
        let mut ids: HashMap<(i64, i64), u32> = HashMap::new();
        let mut edge_nodes = Vec::new();
        // A bike trail is often tagged as one on only some of its ways (e.g.
        // Whistler's Yummy Numby: one of four), so a named trail with any
        // bike way is a bike trail throughout, wherever walking isn't
        // designated (or it's on a hiking route).
        let bike_names: std::collections::HashSet<&str> = raws
            .iter()
            .flat_map(|r| &r.lines)
            .filter(|(w, _)| w.bike && w.class == Class::Trail && !w.name.is_empty())
            .map(|(w, _)| w.name.as_str())
            .collect();
        for r in raws {
            let (x0, y0) = (r.tx as f64 * EXT, r.ty as f64 * EXT);
            for (way, pts) in &r.lines {
                let wi = g.ways.len() as u32;
                let bike = way.bike
                    || (way.class == Class::Trail
                        && !way.walk
                        && bike_names.contains(way.name.as_str()));
                g.ways.push(Way {
                    name: way.name.clone(),
                    class: way.class,
                    bike,
                    walk: way.walk,
                });
                let factor = match way.class {
                    Class::Trail => 1.0,
                    Class::Track => 1.3,
                    _ => 2.0,
                } * if bike { 1.8 } else { 1.0 };
                let mut prev: Option<u32> = None;
                for &p in pts {
                    let key = ((p.0 * 16.0).round() as i64, (p.1 * 16.0).round() as i64);
                    let i = *ids.entry(key).or_insert_with(|| {
                        g.pts.push(p);
                        g.adj.push(vec![]);
                        let on_edge = p.0 <= x0 || p.0 >= x0 + EXT || p.1 <= y0 || p.1 >= y0 + EXT;
                        if on_edge {
                            edge_nodes.push(g.pts.len() as u32 - 1);
                        }
                        g.pts.len() as u32 - 1
                    });
                    if let Some(a) = prev {
                        if a != i {
                            let m = metres(g.pts[a as usize], p) as f32;
                            g.adj[a as usize].push(Edge {
                                to: i,
                                m,
                                cost: m * factor,
                                way: wi,
                            });
                            g.adj[i as usize].push(Edge {
                                to: a,
                                m,
                                cost: m * factor,
                                way: wi,
                            });
                        }
                    }
                    prev = Some(i);
                }
            }
        }
        // Join the copies of a line cut at a tile edge: vertices on tile
        // edges within 3 units (~1 m) of each other.
        let join = g.ways.len() as u32;
        g.ways.push(Way {
            name: String::new(),
            class: Class::Join,
            bike: false,
            walk: false,
        });
        let mut buckets: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
        for &i in &edge_nodes {
            let p = g.pts[i as usize];
            buckets
                .entry(((p.0 / 4.0).floor() as i64, (p.1 / 4.0).floor() as i64))
                .or_default()
                .push(i);
        }
        for &i in &edge_nodes {
            let p = g.pts[i as usize];
            let (bx, by) = ((p.0 / 4.0).floor() as i64, (p.1 / 4.0).floor() as i64);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &j in buckets
                        .get(&(bx + dx, by + dy))
                        .map(|v| &v[..])
                        .unwrap_or(&[])
                    {
                        let q = g.pts[j as usize];
                        if j > i && (p.0 - q.0).abs() <= 3.0 && (p.1 - q.1).abs() <= 3.0 {
                            g.adj[i as usize].push(Edge {
                                to: j,
                                m: 0.0,
                                cost: 0.0,
                                way: join,
                            });
                            g.adj[j as usize].push(Edge {
                                to: i,
                                m: 0.0,
                                cost: 0.0,
                                way: join,
                            });
                        }
                    }
                }
            }
        }
        g
    }
}

impl Area {
    pub fn load(archives: &[PmTiles], park_id: &str, tiles: &[(u32, u32)]) -> Area {
        let lake_ids = std::sync::Mutex::new(HashMap::new());
        let raws: Vec<Raw> = tiles
            .par_iter()
            .map(|&(x, y)| read_tile(archives, x, y, park_id, &lake_ids))
            .collect();
        let graph = Graph::build(&raws);
        let mut area = Area {
            tiles: HashMap::new(),
            graph,
            dests: vec![],
            camps: vec![],
            parkings: vec![],
            known: vec![],
        };
        let mut known: HashMap<String, crate::recall::Known> = HashMap::new();
        let mut lake_pts: HashMap<u32, (f64, f64, f64)> = HashMap::new();
        for r in raws {
            for (id, p) in &r.data.lakes {
                for ring in &p.rings {
                    for q in ring {
                        let e = lake_pts.entry(*id).or_default();
                        e.0 += q.0;
                        e.1 += q.1;
                        e.2 += 1.0;
                    }
                }
            }
            area.dests.extend(r.dests);
            area.camps.extend(r.camps);
            area.parkings.extend(r.parkings);
            for k in r.known {
                match known.get_mut(&k.id) {
                    Some(e) => e.segs.extend(k.segs),
                    None => {
                        known.insert(k.id.clone(), k);
                    }
                }
            }
            area.tiles.insert((r.tx, r.ty), r.data);
        }
        area.known = known.into_values().collect();
        area.known.sort_by(|a, b| a.id.cmp(&b.id));
        for d in &mut area.dests {
            if let Some((x, y, n)) = d.lake.and_then(|id| lake_pts.get(&id)) {
                d.pt = (x / n, y / n);
            }
        }
        area
    }

    pub fn landcover(&self, p: P) -> u8 {
        let Some(t) = self.tiles.get(&geo::tile_of(p)) else {
            return OTHER;
        };
        let mut best = OTHER;
        for (c, poly) in &t.land {
            if *c < best && poly.contains(p) {
                best = *c;
            }
        }
        best
    }

    pub fn in_park(&self, p: P) -> bool {
        self.tiles
            .get(&geo::tile_of(p))
            .is_some_and(|t| t.park.iter().any(|q| q.contains(p)))
    }

    /// Metres from p to the park (0 inside), or None if further than `max`.
    pub fn park_dist(&self, p: P, max: f64) -> Option<f64> {
        if self.in_park(p) {
            return Some(0.0);
        }
        let r = max / mpu(p.1);
        let (t0x, t0y) = geo::tile_of((p.0 - r, p.1 - r));
        let (t1x, t1y) = geo::tile_of((p.0 + r, p.1 + r));
        let mut best = f64::MAX;
        for x in t0x..=t1x {
            for y in t0y..=t1y {
                if let Some(t) = self.tiles.get(&(x, y)) {
                    for q in &t.park {
                        best = best.min(q.boundary_dist(p, r));
                    }
                }
            }
        }
        let m = best * mpu(p.1);
        (m <= max).then_some(m)
    }

    /// Nodes within `max_m` metres of the lake's shore (or in it).
    pub fn lake_access(&self) -> HashMap<u32, Vec<u32>> {
        let mut out: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, &p) in self.graph.pts.iter().enumerate() {
            if self.graph.adj[i].is_empty() {
                continue;
            }
            let Some(t) = self.tiles.get(&geo::tile_of(p)) else {
                continue;
            };
            let r = 75.0 / mpu(p.1);
            for (id, poly) in &t.lakes {
                if poly.contains(p) || poly.boundary_dist(p, r) <= r {
                    let v = out.entry(*id).or_default();
                    if v.last() != Some(&(i as u32)) {
                        v.push(i as u32);
                    }
                }
            }
        }
        out
    }
}
