// Everything about one park that curation needs, read from the basemap's z14
// tiles: the trail and road network as a graph, land cover, the park's own
// outline, and destinations, campsites and trailheads.
use crate::geo::{self, EXT, P, Poly, Z, metres, mpu};
use crate::mvt;
use crate::pmtiles::PmTiles;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

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
    /// SAC hiking grade, T1-T6 as 1-6 (the basemap's sac); 0 if ungraded.
    pub sac: u8,
    /// A driveway or a lot's aisle (the basemap's service=driveway or
    /// parking_aisle): part of somewhere to leave a car (routes#156).
    pub lot: bool,
    /// Walked only one way: as its points run (the basemap's oneway, with
    /// -1's points reversed). The Grouse Grind is up only (routes#163).
    pub oneway: bool,
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
    /// The OSM ids of the feature (and of copies merged into it, see
    /// curate::merge_dests), from the tiles' osm_id; empty where the layer
    /// has none.
    pub osm_ids: Vec<String>,
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
    /// From the tiles' osm_id ("" if none).
    pub osm_id: String,
}

/// A feature's osm_id as a list: empty when the layer has none.
pub fn osm_ids(id: &str) -> Vec<String> {
    if id.is_empty() {
        vec![]
    } else {
        vec![id.to_string()]
    }
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
    /// Lots mapped only by their aisles: the middle of each piece of
    /// service=parking_aisle road (the basemap's parking_aisle flag).
    pub aisles: Vec<P>,
    /// Hiking route relations (the hiking_route layer) passing through.
    pub known: Vec<crate::recall::Known>,
    /// Passenger lifts (the basemap's kind=lift paths): a way back down,
    /// never walked (routes#165).
    pub lifts: Vec<Lift>,
    /// WorldCover, for land cover where OSM maps none (landcover.rs).
    pub worldcover: Option<crate::landcover::Lookup>,
}

/// A gondola or cable car, by its two stations.
#[derive(Clone, Debug)]
pub struct Lift {
    pub name: String,
    pub ends: [P; 2],
}

/// Lifts from their pieces (cut at tile edges, repeated in tile buffers):
/// a lift is straight, so its stations are the two points of its pieces
/// farthest apart. Unnamed lifts can't be told apart, and are dropped.
fn lifts(pieces: Vec<(String, Vec<P>)>) -> Vec<Lift> {
    let mut by_name: HashMap<String, Vec<P>> = HashMap::new();
    for (name, pts) in pieces {
        if !name.is_empty() {
            by_name.entry(name).or_default().extend(pts);
        }
    }
    let mut out: Vec<Lift> = by_name
        .into_iter()
        .filter_map(|(name, pts)| {
            let mut best = (0.0, pts[0], pts[0]);
            for (i, &a) in pts.iter().enumerate() {
                for &b in &pts[i + 1..] {
                    let m = metres(a, b);
                    if m > best.0 {
                        best = (m, a, b);
                    }
                }
            }
            (best.0 > 0.0).then(|| Lift { name, ends: [best.1, best.2] })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

struct Raw {
    tx: u32,
    ty: u32,
    data: TileData,
    lines: Vec<(Way, Vec<P>)>,
    dests: Vec<Dest>,
    camps: Vec<Place>,
    parkings: Vec<Place>,
    aisles: Vec<P>,
    /// Parking lots' outlines (the basemap's building layer, amenity=parking:
    /// in parks, city parks and campgrounds).
    lots: Vec<Poly>,
    known: Vec<crate::recall::Known>, // pieces of relations in this tile
    lifts: Vec<(String, Vec<P>)>,
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
        aisles: vec![],
        lots: vec![],
        known: vec![],
        lifts: vec![],
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
                        // Via ferratas need a harness and lanyards, not
                        // just boots (#45).
                        if kind == "via_ferrata" {
                            continue;
                        }
                        if kind == "lift" {
                            for line in f.geometry() {
                                let pts: Vec<P> =
                                    line.into_iter().map(|(x, y)| s(x, y, l.extent)).collect();
                                if !pts.is_empty() {
                                    raw.lifts.push((l.str(f, "name").to_string(), pts));
                                }
                            }
                            continue;
                        }
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
                        // Paths graded T5-T6 on the SAC scale (demanding or
                        // difficult alpine hiking): climbing and glaciers,
                        // which need ropes or crampons (#45). Jasper's Mount
                        // Edith Cavell Route (T6) took the Tonquin Valley
                        // hike over the mountain (#54). T4 is a scramble:
                        // allowed, and reported (Eval::max_sac).
                        let sac = l.get(f, "sac").map(|v| v.as_f64()).unwrap_or(0.0);
                        if sac >= 5.0 {
                            continue;
                        }
                        let sac = sac.max(0.0) as u8;
                        let flag = |k: &str| {
                            l.get(f, k)
                                .is_some_and(|v| matches!(v, mvt::Value::Bool(true)))
                        };
                        let walk = foot == "designated" || flag("on_route");
                        // A footway is for walking whatever else may use it:
                        // Banff's Fenland Trail is a footway designated for
                        // bikes too, untagged for foot on one way, which made
                        // all of it a bike trail (routes#150).
                        let bike = !walk
                            && (flag("mtb")
                                || (l.str(f, "bicycle") == "designated"
                                    && foot != "yes"
                                    && kind != "footway"));
                        let aisle = class == Class::Road && flag("parking_aisle");
                        let lot = class == Class::Road
                            && (aisle || matches!(l.str(f, "service"), "driveway" | "parking_aisle"));
                        let oneway = l.str(f, "oneway");
                        for line in f.geometry() {
                            let mut pts: Vec<P> =
                                line.into_iter().map(|(x, y)| s(x, y, l.extent)).collect();
                            if oneway == "-1" {
                                pts.reverse();
                            }
                            if aisle && !pts.is_empty() {
                                // Its middle, by its points; pieces in the
                                // tile's buffer too (Ctx::starts merges them).
                                let n = pts.len() as f64;
                                raw.aisles.push((
                                    pts.iter().map(|p| p.0).sum::<f64>() / n,
                                    pts.iter().map(|p| p.1).sum::<f64>() / n,
                                ));
                            }
                            raw.lines.push((
                                Way {
                                    name: not_a_note(l.str(f, "name")),
                                    class,
                                    bike,
                                    walk,
                                    sac,
                                    lot,
                                    oneway: matches!(oneway, "yes" | "-1"),
                                },
                                pts,
                            ));
                        }
                    }
                    "building" if f.typ == 3 && l.str(f, "amenity") == "parking" => {
                        raw.lots.push(Poly::new(geo::rings(f.geometry(), tx, ty, l.extent), tx, ty));
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
                                    osm_ids: osm_ids(l.str(f, "osm_id")),
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
                                name: not_a_note(name),
                                pt,
                                trailhead: l.str(f, "highway") == "trailhead",
                                osm_id: l.str(f, "osm_id").to_string(),
                            });
                        }
                        // Places to sleep on a multi-day route: campsites, huts,
                        // and lean-tos (the Letchworth Trail's shelters are
                        // amenity=shelter, shelter_type=lean_to; #39). Unnamed
                        // ones too (Sleeping Giant's Tee Harbour sites): curate
                        // names them after the trail they're on.
                        let sleep = tourism == "camp_site"
                            || tourism == "wilderness_hut"
                            || (amenity == "shelter"
                                && matches!(l.str(f, "shelter_type"), "lean_to" | "basic_hut"));
                        if sleep {
                            raw.camps.push(Place {
                                name: name.to_string(),
                                pt,
                                trailhead: false,
                                osm_id: l.str(f, "osm_id").to_string(),
                            });
                        }
                        if name.is_empty() {
                            continue;
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
                                osm_ids: osm_ids(l.str(f, "osm_id")),
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
                    sac: way.sac,
                    lot: way.lot,
                    oneway: way.oneway,
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
                        // Tiles repeat what crosses their edges, so the same
                        // segment can come from two tiles: keep one, or each
                        // end looks like a fork (Algonquin's Two Rivers
                        // Trail, routes#133).
                        let dup = g.adj[a as usize].iter().any(|e| {
                            let w = &g.ways[e.way as usize];
                            e.to == i
                                && w.name == way.name
                                && w.class == way.class
                                && w.bike == bike
                                && w.walk == way.walk
                                && w.sac == way.sac
                        });
                        if a != i && !dup {
                            let m = metres(g.pts[a as usize], p) as f32;
                            g.adj[a as usize].push(Edge {
                                to: i,
                                m,
                                cost: m * factor,
                                way: wi,
                            });
                            // Against a one-way trail: never walked.
                            g.adj[i as usize].push(Edge {
                                to: a,
                                m,
                                cost: if way.oneway { f32::INFINITY } else { m * factor },
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
            sac: 0,
            lot: false,
            oneway: false,
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
        g.join_lots(raws.iter().flat_map(|r| &r.lots));
        g
    }

    /// A trail that ends on a parking lot's outline joins what else touches or
    /// crosses the lot: a lot is open ground, but its outline isn't a way to
    /// walk. Algonquin's Big Pines Trail ends on its lot's outline, and the
    /// road through the lot doesn't touch it: no trailhead reached the trail
    /// (routes#133). Each trail node on the outline gets a way (a track's
    /// cost) to the nearest node of another way on the outline or inside.
    fn join_lots<'a>(&mut self, lots: impl Iterator<Item = &'a Poly>) {
        let lots: Vec<&Poly> = lots.collect();
        if lots.is_empty() {
            return;
        }
        const CELL: f64 = 128.0;
        let cell = |p: P| ((p.0 / CELL).floor() as i64, (p.1 / CELL).floor() as i64);
        let mut grid: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
        for (i, p) in self.pts.iter().enumerate() {
            grid.entry(cell(*p)).or_default().push(i as u32);
        }
        let join = self.ways.len() as u32;
        self.ways.push(Way {
            name: String::new(),
            class: Class::Track,
            bike: false,
            walk: false,
            sac: 0,
            lot: false,
            oneway: false,
        });
        let mut add = vec![];
        for lot in lots {
            // The lot's nodes: on its outline (within ON_OUTLINE) or inside.
            let (c0, c1) = (cell(lot.min), cell(lot.max));
            let mut on = vec![];
            let mut members = vec![];
            for cx in c0.0 - 1..=c1.0 + 1 {
                for cy in c0.1 - 1..=c1.1 + 1 {
                    for &n in grid.get(&(cx, cy)).map(|v| &v[..]).unwrap_or(&[]) {
                        let p = self.pts[n as usize];
                        let edge = lot.boundary_dist(p, ON_OUTLINE) <= ON_OUTLINE;
                        if edge || lot.contains(p) {
                            members.push(n);
                            if edge {
                                on.push(n);
                            }
                        }
                    }
                }
            }
            for &t in &on {
                let tw: Vec<u32> = self.adj[t as usize].iter().map(|e| e.way).collect();
                if !tw.iter().any(|&w| self.ways[w as usize].class == Class::Trail) {
                    continue;
                }
                // Another way's node: not one of the trail's own ways.
                let other = members
                    .iter()
                    .copied()
                    .filter(|&o| o != t)
                    .filter(|&o| self.adj[o as usize].iter().any(|e| !tw.contains(&e.way) && e.way != join))
                    .map(|o| (metres(self.pts[t as usize], self.pts[o as usize]), o))
                    .min_by(|a, b| a.0.total_cmp(&b.0));
                if let Some((m, o)) = other {
                    add.push((t, o, m));
                }
            }
        }
        for (t, o, m) in add {
            let m = m as f32;
            self.adj[t as usize].push(Edge { to: o, m, cost: m * 1.3, way: join });
            self.adj[o as usize].push(Edge { to: t, m, cost: m * 1.3, way: join });
        }
    }
}

/// How close (world units, ~1 m) a node must be to a lot's outline to be on it.
const ON_OUTLINE: f64 = 2.0;

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
            aisles: vec![],
            known: vec![],
            lifts: vec![],
            worldcover: None,
        };
        let mut known: HashMap<String, crate::recall::Known> = HashMap::new();
        let mut lake_pts: HashMap<u32, (f64, f64, f64)> = HashMap::new();
        let mut lift_pieces = vec![];
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
            area.aisles.extend(r.aisles);
            lift_pieces.extend(r.lifts);
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
        area.lifts = lifts(lift_pieces);
        area.known = known.into_values().collect();
        area.known.sort_by(|a, b| a.id.cmp(&b.id));
        for d in &mut area.dests {
            if let Some((x, y, n)) = d.lake.and_then(|id| lake_pts.get(&id)) {
                d.pt = (x / n, y / n);
            }
        }
        area.drop_glacier_trails();
        area
    }

    /// Whether a point is on a glacier (land cover).
    fn on_glacier(&self, p: P) -> bool {
        self.tiles
            .get(&geo::tile_of(p))
            .is_some_and(|t| t.land.iter().any(|(c, poly)| *c == 0 && poly.contains(p)))
    }

    /// Drop ungraded trail edges across a glacier (#45): crossing one needs
    /// a rope and crampons, and most alpine paths have no sac_scale to say
    /// so (Area's T5-T6 skip only catches graded ones). An edge goes when
    /// both its ends and its middle are on the ice, so a trail to a
    /// glacier's toe or along its edge stays. A graded path (T1-T4) is
    /// trusted: glaciers shrink, and their outlines lag.
    fn drop_glacier_trails(&mut self) {
        if !self.tiles.values().any(|t| t.land.iter().any(|(c, _)| *c == 0)) {
            return;
        }
        let g = &self.graph;
        let ungraded = |w: u32| {
            let w = &g.ways[w as usize];
            w.class == Class::Trail && w.sac == 0
        };
        let mut ice: HashMap<u32, bool> = HashMap::new();
        let mut drop: HashSet<(u32, u32)> = HashSet::new();
        for (u, es) in g.adj.iter().enumerate() {
            let u = u as u32;
            for e in es {
                if e.to < u || !ungraded(e.way) {
                    continue;
                }
                let (a, b) = (g.pts[u as usize], g.pts[e.to as usize]);
                let on = |n: u32, ice: &mut HashMap<u32, bool>| {
                    *ice.entry(n).or_insert_with(|| self.on_glacier(g.pts[n as usize]))
                };
                if on(u, &mut ice)
                    && on(e.to, &mut ice)
                    && self.on_glacier(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0))
                {
                    drop.insert((u, e.to));
                }
            }
        }
        if drop.is_empty() {
            return;
        }
        for (u, es) in self.graph.adj.iter_mut().enumerate() {
            let u = u as u32;
            es.retain(|e| !drop.contains(&(u.min(e.to), u.max(e.to))));
        }
    }

    /// The land cover at p: OSM's where a land polygon covers it, otherwise
    /// WorldCover's (if loaded), otherwise OTHER.
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
        if best == OTHER
            && let Some(wc) = &self.worldcover
        {
            return wc.at(geo::lonlat(p));
        }
        best
    }

    /// Km of walkable trail (paths and tracks, not bike-only) in the park:
    /// how much hiking it has, for how many day hikes to choose (#38).
    pub fn trail_km_in_park(&self) -> f64 {
        let g = &self.graph;
        let mut m = 0.0;
        for (a, es) in g.adj.iter().enumerate() {
            for e in es {
                // Each edge is in both nodes' lists: count it once.
                if (e.to as usize) < a {
                    continue;
                }
                let w = &g.ways[e.way as usize];
                if !matches!(w.class, Class::Trail | Class::Track) || w.bike {
                    continue;
                }
                let (p, q) = (g.pts[a], g.pts[e.to as usize]);
                if self.in_park(((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0)) {
                    m += e.m as f64;
                }
            }
        }
        m / 1000.0
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

/// A way's or parking lot's name, or "" when the name is an access note
/// rather than a name: Yoho's Lake O'Hara road is named "Bus Only", which
/// made "Mount Whyte from Bus Only trailhead" (#42).
pub fn not_a_note(name: &str) -> String {
    let n = name.trim().to_lowercase();
    let words: Vec<&str> = n
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let note = words.contains(&"only")
        || matches!(
            n.as_str(),
            "private"
                | "private road"
                | "private drive"
                | "no access"
                | "no entry"
                | "no trespassing"
                | "closed"
                | "gate"
                | "gated"
                | "authorized vehicles"
                | "emergency access"
        );
    if note {
        String::new()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod note_tests {
    use super::not_a_note;

    #[test]
    fn access_notes_are_not_names() {
        assert_eq!(not_a_note("Bus Only"), "");
        assert_eq!(not_a_note("Authorized Vehicles Only"), "");
        assert_eq!(not_a_note("Private"), "");
        assert_eq!(not_a_note("Lake O'Hara Fire Road"), "Lake O'Hara Fire Road");
        // "only" as part of a word is fine.
        assert_eq!(not_a_note("Onlyhill Trail"), "Onlyhill Trail");
    }
}

#[cfg(test)]
mod lift_tests {
    use super::lifts;

    // A lift cut at a tile edge, its pieces overlapping in the tile buffer:
    // its stations are its farthest points. Unnamed lifts are dropped.
    #[test]
    fn a_lift_is_its_two_farthest_points() {
        let y = 0.3;
        let pieces = vec![
            ("Skyride".to_string(), vec![(0.10, y), (0.11, y + 0.01)]),
            ("Skyride".to_string(), vec![(0.109, y + 0.009), (0.12, y + 0.02)]),
            (String::new(), vec![(0.5, y), (0.6, y)]),
        ];
        let l = lifts(pieces);
        assert_eq!(l.len(), 1);
        let mut ends = l[0].ends;
        ends.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(ends, [(0.10, y), (0.12, y + 0.02)]);
    }
}
