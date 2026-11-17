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
    /// A named sight that isn't one of the above: a natural arch, a beach or
    /// a cave entrance (Bruce Peninsula's Grotto, a cave on the shore), or a
    /// natural feature tagged an attraction (Halfway Log Dump, a bay). Not
    /// any attraction: Minneapolis' water taxi and Lake Louise's bear sign
    /// are tourism=attraction too.
    Sight,
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
            Kind::Sight => "sight",
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
            Kind::Sight => 0.75,
            Kind::Trail => TRAIL_APPEAL,
        }
    }
}

/// See Area::outside_lakes_unknown: the share of a lake's area that must be
/// in the tiles loaded for the park for its fame to count, and the size
/// from which that's checked. A smaller lake is all near where a hike
/// reaches it.
const LAKE_LOADED: f64 = 0.5;
const LAKE_WIDE_M2: f64 = 1e7;

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
    /// How many sites have a trip report or trail guide about it
    /// (hikeratlas/cc-crawler's mentions.py; --mentions): 0 if none, or
    /// without --mentions.
    pub mentions: u32,
    pub lake: Option<u32>, // for lakes: which lake (index into Area::lakes)
    /// A peak with neither an elevation nor a Wikidata item: often a
    /// climbing crag mapped as a peak (Joshua Tree's Cyclops Rock, Sports
    /// Challenge Rock), less of a draw than a summit. Or a lake mostly
    /// beyond the park (Area::outside_lakes_unknown).
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
    /// Small things worth walking to, named or not: benches, viewpoints,
    /// picnic sites, waterfalls, beaches (OSM nodes). For a short trail's
    /// dead end (curate's named_pieces).
    pub spots: Vec<P>,
    /// Viewpoints mapped without a name (see Area::name_lookouts).
    pub views: Vec<P>,
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
    /// Named trails' mentions (see Dest::mentions) and length in the
    /// park's ways (metres), by lowercased name: those near the park, from
    /// --mentions.
    pub trail_mentions: HashMap<String, (u32, f64)>,
    /// Trails told apart only by a direction, as one trail by their shared
    /// name (trail_stems): "Gibson Trail" for North and South Gibson Trail.
    pub trail_stems: Vec<(String, Vec<String>)>,
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
    spots: Vec<P>,
    /// Viewpoints mapped without a name (see Area::name_lookouts).
    views: Vec<P>,
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
    park_ids: &[String],
    lake_ids: &std::sync::Mutex<HashMap<(String, i64), u32>>,
) -> Raw {
    let mut raw = Raw {
        tx,
        ty,
        data: TileData::default(),
        lines: vec![],
        dests: vec![],
        camps: vec![],
        spots: vec![],
        views: vec![],
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
                            // Ids in the order threads reach lakes; Area::load
                            // renumbers them by name, and keeps the first
                            // tile's destination (in tile order) of each, so
                            // curating doesn't depend on which thread read
                            // which tile first (routes#228).
                            let id = {
                                let mut m = lake_ids.lock().unwrap();
                                let n = m.len() as u32;
                                *m.entry(key).or_insert(n)
                            };
                            let poly = Poly::new(rings.clone(), tx, ty);
                            {
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
                                    mentions: 0,
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
                    "park" | "trail_area" | "marine_park" if f.typ == 3 && park_ids.iter().any(|i| i == l.str(f, "osm_id")) => {
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
                        if amenity == "bench"
                            || matches!(tourism, "viewpoint" | "picnic_site")
                            || matches!(l.str(f, "natural"), "beach" | "cave_entrance")
                            || l.str(f, "waterway") == "waterfall"
                        {
                            raw.spots.push(pt);
                        }
                        if tourism == "viewpoint" && name.is_empty() {
                            raw.views.push(pt);
                        }
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
                        } else if matches!(l.str(f, "natural"), "arch" | "beach" | "cave_entrance")
                            || (tourism == "attraction" && !l.str(f, "natural").is_empty())
                            // A historic site mapped as an attraction: Writing-
                            // on-Stone's Battle Scene Petroglyph, an
                            // archaeological site (routes#95). Not one without
                            // tourism=attraction: many such sites aren't for
                            // visiting.
                            || (tourism == "attraction" && !l.str(f, "historic").is_empty())
                        {
                            Some(Kind::Sight)
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
                                mentions: 0,
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
        g.join_gaps(raws.iter().flat_map(|r| r.parkings.iter().map(|p| p.pt)));
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
    /// A trail that dead-ends by a parking lot or trailhead, just short of
    /// another way's dead end, goes on to it, across what is a gap in the
    /// mapping: Yoho's Mt. Hunter Lookout Trail ends 11 m from the end of
    /// Wapta Falls Road, 38 m from the lookout's lot, joined to nothing, so
    /// no trailhead reached it. Each such trail dead end (within GAP_LOT_M
    /// of a lot or trailhead) gets a way (a track's cost) to the nearest dead
    /// end of another way within GAP_M. Only end to end, and only by a lot:
    /// elsewhere a trail ending near another may be across a fence, a
    /// stream or a cliff (joining every such pair changed 96 of the
    /// fixtures' hikes).
    fn join_gaps(&mut self, lots: impl Iterator<Item = P>) {
        let mut by_lot: HashMap<(u32, u32), Vec<P>> = HashMap::new();
        for p in lots {
            by_lot.entry(geo::tile_of(p)).or_default().push(p);
        }
        let near_lot = |p: P| {
            let (tx, ty) = geo::tile_of(p);
            (tx.saturating_sub(1)..=tx + 1).any(|x| {
                (ty.saturating_sub(1)..=ty + 1).any(|y| {
                    by_lot.get(&(x, y)).is_some_and(|v| v.iter().any(|&q| metres(p, q) <= GAP_LOT_M))
                })
            })
        };
        let dead: Vec<u32> = (0..self.pts.len() as u32).filter(|&n| self.adj[n as usize].len() == 1).collect();
        if dead.is_empty() {
            return;
        }
        // World units are smaller than metres toward the poles: cells of
        // 4 * GAP_M units hold GAP_M up to 75 degrees.
        let cell = |p: P| ((p.0 / (4.0 * GAP_M)).floor() as i64, (p.1 / (4.0 * GAP_M)).floor() as i64);
        let mut grid: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
        for &n in &dead {
            grid.entry(cell(self.pts[n as usize])).or_default().push(n);
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
        let mut add: Vec<(u32, u32, f64)> = vec![];
        for &t in &dead {
            let tw = self.adj[t as usize][0].way;
            if self.ways[tw as usize].class != Class::Trail {
                continue;
            }
            let p = self.pts[t as usize];
            if !near_lot(p) {
                continue;
            }
            let (cx, cy) = cell(p);
            let mut best: Option<(f64, u32)> = None;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &o in grid.get(&(cx + dx, cy + dy)).map(|v| &v[..]).unwrap_or(&[]) {
                        if o == t || self.adj[o as usize][0].way == tw {
                            continue;
                        }
                        let m = metres(p, self.pts[o as usize]);
                        if m <= GAP_M && best.is_none_or(|b| (m, o) < b) {
                            best = Some((m, o));
                        }
                    }
                }
            }
            if let Some((m, o)) = best {
                // Once a pair, when both ends are trails' and nearest each other.
                if !add.iter().any(|&(a, b, _)| (a, b) == (o, t)) {
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

/// The lookout a trail is named for: "Mt. Hunter Lookout" for "Mt. Hunter
/// Lookout Trail". Only "<name> Lookout|Overlook|Viewpoint Trail", where
/// <name> says which: "Lookout Trail" alone, "Viewpoint Loop" or "Little
/// River Viewpoint Nature Trail" don't name one.
fn lookout_name(trail: &str) -> Option<&str> {
    let t = trail.strip_suffix(" Trail").or_else(|| trail.strip_suffix(" trail"))?;
    let lower = t.to_lowercase();
    ["lookout", "look out", "overlook", "viewpoint"]
        .iter()
        .any(|k| lower.ends_with(&format!(" {k}")))
        .then_some(t)
}

/// "Mt. Hunter Lower Lookout" for "Mt. Hunter Lookout" and "Lower": the word
/// before the last one.
pub fn with_word(base: &str, word: &str) -> String {
    match base.rsplit_once(' ') {
        Some((a, b)) => format!("{a} {word} {b}"),
        None => format!("{word} {base}"),
    }
}

/// Two lookouts of one trail nearer than this are one (Area::name_lookouts),
/// in metres.
const LOOKOUT_APART_M: f64 = 300.0;

/// How near a trail named for a lookout an unnamed viewpoint is that lookout
/// (Area::name_lookouts), in metres: Mt. Hunter's tower is 204 m from its
/// trail as mapped.
const LOOKOUT_ON_M: f64 = 250.0;

/// How near another way's dead end a trail's dead end is joined to it
/// (Graph::join_gaps), in metres.
const GAP_M: f64 = 20.0;
/// How near a parking lot or trailhead a trail's dead end must be for
/// Graph::join_gaps to join it (as SIGNED_END_M, curate's for a spur's).
const GAP_LOT_M: f64 = 150.0;

/// How close (world units, ~1 m) a node must be to a lot's outline to be on it.
const ON_OUTLINE: f64 = 2.0;

/// Directions that tell trails of one name apart ("North Gibson Trail").
const STEM_PREFIXES: [&str; 6] = ["North ", "South ", "East ", "West ", "Upper ", "Lower "];

/// Names that differ only by a leading direction, by that shared name: two
/// or more of them, and not where a trail already has the shared name
/// (whose own trail it is). "North Gibson Trail" and "South Gibson Trail"
/// are "Gibson Trail". Sorted, each name once.
pub fn trail_stems<'a>(names: impl Iterator<Item = &'a str>) -> Vec<(String, Vec<String>)> {
    let names: std::collections::BTreeSet<&str> = names.collect();
    let lower: std::collections::HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let mut by_stem: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for &n in &names {
        for p in STEM_PREFIXES {
            if let Some(rest) = n.strip_prefix(p) {
                // A stem of more than a bare "Trail".
                if rest.split_whitespace().any(|w| !w.eq_ignore_ascii_case("trail")) {
                    by_stem.entry(rest.to_string()).or_default().push(n.to_string());
                }
            }
        }
    }
    by_stem
        .into_iter()
        .filter(|(s, ns)| ns.len() >= 2 && !lower.contains(&s.to_lowercase()))
        .collect()
}

/// How near a stem's trails come where they meet (through a road or
/// another trail between them): North and South Gibson Trail touch at one
/// end and are 31 m apart at the other, across Strawberry Flats Loop.
pub const STEM_JOIN_M: f64 = 50.0;
/// Two places a stem's trails meet are this far apart.
const STEM_PLACES_M: f64 = 1000.0;

/// Whether a stem's trails are one: each meets another at two places,
/// within STEM_JOIN_M, at least STEM_PLACES_M apart, so they make a loop
/// together (North and South Gibson Trail). Not two trails that only touch
/// end to end, or run apart: Olympic's North and South Coast Beach
/// Travelway, North and South Kalaloch Beach.
fn stem_meets(g: &Graph, names: &[String]) -> bool {
    let pts: Vec<Vec<P>> = names
        .iter()
        .map(|n| {
            let mut v = vec![];
            for (a, es) in g.adj.iter().enumerate() {
                if es.iter().any(|e| g.ways[e.way as usize].name == *n) {
                    v.push(g.pts[a]);
                }
            }
            v
        })
        .collect();
    (0..pts.len()).all(|i| {
        // Where trail i comes within STEM_JOIN_M of another of them.
        let meets: Vec<P> = pts[i]
            .iter()
            .copied()
            .filter(|&p| {
                (0..pts.len()).any(|j| j != i && pts[j].iter().any(|&q| crate::geo::metres(p, q) <= STEM_JOIN_M))
            })
            .collect();
        meets.iter().any(|&p| meets.iter().any(|&q| crate::geo::metres(p, q) >= STEM_PLACES_M))
    })
}

/// A waterfall's height in metres, from Wikidata (data/waterfall-heights.tsv,
/// by data/waterfall-heights.py), by its Wikidata id: None if it has none.
pub fn waterfall_height(wikidata: &str) -> Option<f64> {
    static HEIGHTS: std::sync::OnceLock<HashMap<&'static str, f64>> = std::sync::OnceLock::new();
    HEIGHTS
        .get_or_init(|| {
            include_str!("../data/waterfall-heights.tsv")
                .lines()
                .skip(1)
                .filter_map(|l| {
                    let (id, h) = l.split_once('\t')?;
                    Some((id, h.parse().ok()?))
                })
                .collect()
        })
        .get(wikidata)
        .copied()
}

/// Whether a way named `way` is part of the trail `name`: of that name, or
/// one of its trail_stems'.
pub fn of_trail(stems: &[(String, Vec<String>)], way: &str, name: &str) -> bool {
    way == name || stems.iter().any(|(s, ns)| s == name && ns.iter().any(|n| n == way))
}

impl Area {
    /// park_ids: the park's OSM ids, any of whose polygons is the park (one
    /// park mapped as several same-named pieces is curated as one).
    pub fn load(archives: &[PmTiles], park_ids: &[String], tiles: &[(u32, u32)]) -> Area {
        let lake_ids = std::sync::Mutex::new(HashMap::new());
        let raws: Vec<Raw> = tiles
            .par_iter()
            .map(|&(x, y)| read_tile(archives, x, y, park_ids, &lake_ids))
            .collect();
        let graph = Graph::build(&raws);
        let mut area = Area {
            tiles: HashMap::new(),
            graph,
            dests: vec![],
            camps: vec![],
            spots: vec![],
            views: vec![],
            parkings: vec![],
            aisles: vec![],
            known: vec![],
            lifts: vec![],
            worldcover: None,
            trail_mentions: HashMap::new(),
            trail_stems: vec![],
        };
        // Lake ids by (name, area), not by which thread reached each first.
        let mut keys: Vec<((String, i64), u32)> = lake_ids.into_inner().unwrap().into_iter().collect();
        keys.sort();
        let mut renumber: HashMap<u32, u32> = HashMap::new();
        for (new, (_, old)) in keys.into_iter().enumerate() {
            renumber.insert(old, new as u32);
        }
        let mut raws = raws;
        for r in &mut raws {
            for (id, _) in &mut r.data.lakes {
                *id = renumber[id];
            }
            for d in &mut r.dests {
                if let Some(id) = &mut d.lake {
                    *id = renumber[id];
                }
            }
        }
        // Each lake's destination once: the first tile's, in tile order.
        let mut lake_seen: std::collections::HashSet<u32> = Default::default();
        let mut known: HashMap<String, crate::recall::Known> = HashMap::new();
        let mut lake_pts: HashMap<u32, (f64, f64, f64)> = HashMap::new();
        let mut lift_pieces = vec![];
        for mut r in raws {
            r.dests.retain(|d| d.lake.is_none_or(|id| lake_seen.insert(id)));
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
            area.spots.extend(r.spots);
            area.views.extend(r.views);
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
        let stems = trail_stems(
            area.graph
                .ways
                .iter()
                .filter(|w| matches!(w.class, Class::Trail | Class::Track) && !w.name.is_empty())
                .map(|w| w.name.as_str()),
        );
        area.trail_stems = stems.into_iter().filter(|(_, ns)| stem_meets(&area.graph, ns)).collect();
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

    /// The lookouts a trail named for one leads to ("Mt. Hunter Lookout
    /// Trail", see lookout_name), as destinations named for it: a viewpoint
    /// mapped without a name within LOOKOUT_ON_M of the trail, where the trail
    /// comes nearest it, and the trail's dead end away from any lot or
    /// trailhead (where it leads). Two of them more than LOOKOUT_APART_M
    /// apart are the lower and upper lookout, by the DEM ("Mt. Hunter Lower
    /// Lookout", "Mt. Hunter Upper Lookout"); one is the trail's ("Mt. Hunter
    /// Lookout"). Yoho's Mt. Hunter Lookout Trail climbs past its lower
    /// lookout, an observation tower mapped as a viewpoint with no name
    /// 204 m off the trail as mapped, to its upper one at the trail's end,
    /// mapped as nothing: no hike went to either. Not where a destination
    /// already has the name. Called once the area is loaded (main).
    pub fn name_lookouts(&mut self, dem: &crate::dem::Dem) {
        let g = &self.graph;
        // Each such trail's nodes, and by z14 tile.
        let mut trails: HashMap<&str, Vec<u32>> = HashMap::new();
        let mut near: HashMap<(u32, u32), Vec<(u32, u32)>> = HashMap::new();
        for (u, adj) in g.adj.iter().enumerate() {
            for e in adj {
                let w = &g.ways[e.way as usize];
                if matches!(w.class, Class::Trail | Class::Track) && lookout_name(&w.name).is_some() {
                    near.entry(geo::tile_of(g.pts[u])).or_default().push((u as u32, e.way));
                    let ns = trails.entry(w.name.as_str()).or_default();
                    if ns.last() != Some(&(u as u32)) {
                        ns.push(u as u32);
                    }
                }
            }
        }
        if trails.is_empty() {
            return;
        }
        // Each trail's nearest unnamed viewpoint, at the trail.
        let mut found: HashMap<&str, Vec<P>> = HashMap::new();
        let mut best: HashMap<&str, (f64, u32)> = HashMap::new();
        for &v in &self.views {
            let (tx, ty) = geo::tile_of(v);
            let mut hit: Option<(f64, u32, u32)> = None;
            for x in tx.saturating_sub(1)..=tx + 1 {
                for y in ty.saturating_sub(1)..=ty + 1 {
                    for &(n, w) in near.get(&(x, y)).map(|v| &v[..]).unwrap_or(&[]) {
                        let m = metres(g.pts[n as usize], v);
                        if m <= LOOKOUT_ON_M && hit.is_none_or(|h| (m, w, n) < h) {
                            hit = Some((m, w, n));
                        }
                    }
                }
            }
            if let Some((m, w, n)) = hit {
                let name = g.ways[w as usize].name.as_str();
                if best.get(name).is_none_or(|b| m < b.0) {
                    best.insert(name, (m, n));
                }
            }
        }
        for (name, (_, n)) in best {
            found.entry(name).or_default().push(g.pts[n as usize]);
        }
        // Each trail's dead ends away from lots and trailheads.
        for (&name, ns) in &trails {
            for &n in ns {
                let p = g.pts[n as usize];
                if g.adj[n as usize].len() == 1
                    && !self.parkings.iter().any(|l| metres(l.pt, p) <= GAP_LOT_M)
                {
                    found.entry(name).or_default().push(p);
                }
            }
        }
        let mut add: Vec<(String, P)> = vec![];
        for (trail, mut pts) in found {
            let Some(base) = lookout_name(trail) else { continue };
            pts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            pts.dedup_by(|a, b| metres(*a, *b) <= LOOKOUT_APART_M);
            match pts.len() {
                1 => add.push((base.to_string(), pts[0])),
                2 => {
                    let ele = dem.profile(&pts);
                    let (lo, hi) = if ele[0] <= ele[1] { (pts[0], pts[1]) } else { (pts[1], pts[0]) };
                    add.push((with_word(base, "Lower"), lo));
                    add.push((with_word(base, "Upper"), hi));
                }
                // More than two: which is which isn't clear.
                _ => {}
            }
        }
        add.retain(|(n, _)| !self.dests.iter().any(|d| d.name.eq_ignore_ascii_case(n)));
        add.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.partial_cmp(&b.1).unwrap()));
        for (name, pt) in add {
            self.dests.push(Dest {
                name,
                kind: Kind::Viewpoint,
                pt,
                area: 0.0,
                wikidata: String::new(),
                osm_ids: vec![],
                qrank: 1.0,
                mentions: 0,
                lake: None,
                minor: false,
            });
        }
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

    /// Takes the popularity (QRank) and mentions from lakes of LAKE_WIDE_M2
    /// or more that are mostly beyond the tiles loaded for the park (less
    /// than LAKE_LOADED of their area in them): what's written about Lake
    /// Huron is about all of it, not the shore a park's hike reaches
    /// (Killarney's top easy hike was 3 km to it, routes#192). Not lakes at
    /// the park's edge: Golden Ears' Alouette Lake, outside its outline, is
    /// its beaches and the hikes along its shore.
    pub fn outside_lakes_unknown(&mut self) {
        let want: HashSet<u32> = self
            .dests
            .iter()
            .filter(|d| d.kind == Kind::Lake && d.area >= LAKE_WIDE_M2 && (d.qrank > 1.0 || d.mentions > 0))
            .filter_map(|d| d.lake)
            .collect();
        if want.is_empty() {
            return;
        }
        // Each lake's loaded area (m²): its pieces' by the shoelace formula.
        let mut loaded: HashMap<u32, f64> = HashMap::new();
        for t in self.tiles.values() {
            for (id, poly) in t.lakes.iter().filter(|(id, _)| want.contains(id)) {
                let m = mpu((poly.min.1 + poly.max.1) / 2.0);
                let a: f64 = poly
                    .rings
                    .iter()
                    .map(|r| r.windows(2).map(|w| w[0].0 * w[1].1 - w[1].0 * w[0].1).sum::<f64>() / 2.0)
                    .sum::<f64>()
                    .abs();
                *loaded.entry(*id).or_default() += a * m * m;
            }
        }
        for d in &mut self.dests {
            if let Some(id) = d.lake.filter(|id| want.contains(id)) {
                if loaded.get(&id).copied().unwrap_or(0.0) < LAKE_LOADED * d.area {
                    d.qrank = 1.0;
                    d.mentions = 0;
                    // And it's no big draw (curate::score): Bruce Peninsula's
                    // loop to the Grotto was "Lake Huron", the shore it's on.
                    d.minor = true;
                }
            }
        }
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
        without_length(name).to_string()
    }
}

/// A name less a length in brackets at its end: Pinery's "Wilderness Trail
/// (2.6 km)" is Wilderness Trail, as its signs and the park's guide name it.
/// The length is a note for the map, and hikes named for the trail took it
/// on ("Wilderness Trail (2.6 km) from Wilderness Trail (2.6 km) trailhead").
fn without_length(name: &str) -> &str {
    let t = name.trim_end();
    let Some(open) = t.rfind('(') else { return name };
    let Some(inner) = t[open + 1..].strip_suffix(')') else { return name };
    let inner = inner.trim().to_lowercase();
    let Some((num, unit)) = inner.split_once(|c: char| c.is_whitespace()).or_else(|| {
        let i = inner.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))?;
        Some((&inner[..i], &inner[i..]))
    }) else {
        return name;
    };
    let num_ok = !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',');
    let unit_ok = matches!(unit.trim(), "km" | "m" | "mi" | "mile" | "miles" | "kilometres" | "kilometers");
    if num_ok && unit_ok && open > 0 {
        t[..open].trim_end()
    } else {
        name
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

    #[test]
    fn lengths_in_brackets_are_not_names() {
        assert_eq!(not_a_note("Wilderness Trail (2.6 km)"), "Wilderness Trail");
        assert_eq!(not_a_note("Sassafras (1.0 km)"), "Sassafras");
        assert_eq!(not_a_note("Loop (3mi)"), "Loop");
        // Other notes in brackets stay: they tell trails apart.
        assert_eq!(not_a_note("Private beach path (Huron woods)"), "Private beach path (Huron woods)");
        assert_eq!(not_a_note("Beach lookout (Wilderness trail)"), "Beach lookout (Wilderness trail)");
        assert_eq!(not_a_note("(2 km)"), "(2 km)");
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
