// Curate a park's hikes: day hikes from trailheads, base camps (walk in,
// then day hikes from camp), and multi-day point-to-points via campsites.
use crate::area::{Area, Class, Dest, Kind, LANDCOVER, OTHER, Place, alpine};
use crate::dem::{Dem, gain_loss};
use crate::geo::{self, P, metres, mpu};
use rayon::prelude::*;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
// Time spent searching and evaluating routes, summed over threads, for the
// log: where curating's time goes.
static T_SEARCH: AtomicU64 = AtomicU64::new(0);
static T_EVAL: AtomicU64 = AtomicU64::new(0);
static N_SEARCH: AtomicU64 = AtomicU64::new(0);
static N_SETTLED: AtomicU64 = AtomicU64::new(0);
struct Tm(&'static AtomicU64, std::time::Instant);
impl Drop for Tm {
    fn drop(&mut self) {
        self.0
            .fetch_add(self.1.elapsed().as_micros() as u64, Relaxed);
    }
}

pub fn profile() -> String {
    format!(
        "searching {:.0}s ({} searches, {} nodes settled), evaluating {:.0}s (thread time)",
        T_SEARCH.load(Relaxed) as f64 / 1e6,
        N_SEARCH.load(Relaxed),
        N_SETTLED.load(Relaxed),
        T_EVAL.load(Relaxed) as f64 / 1e6
    )
}

pub struct Ctx<'a> {
    pub area: &'a Area,
    pub dem: &'a Dem,
    grid: HashMap<(u32, u32), Vec<u32>>, // nodes by z14 tile
    camp_near: HashMap<u32, Vec<u32>>,   // node -> camps within 150 m
    camps: Vec<Place>,
    dests: Vec<Dest>,
    pub lake_access: HashMap<u32, Vec<u32>>,
    dest_near: Vec<Vec<u32>>, // nodes within 100 m of each destination
    in_park: Vec<bool>,       // by node
}

#[derive(PartialEq)]
struct Q(f64, u32);
impl Eq for Q {}
impl Ord for Q {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.partial_cmp(&self.0).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for Q {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// A search's per-node state, in arrays indexed by node, reused from search
/// to search (see POOL): an entry is current only if its stamp is the
/// search's, so nothing needs clearing between searches. Hash maps of the
/// nodes reached took most of curating's time in dense city networks.
struct Scratch {
    stamp: Vec<u32>,
    epoch: u32,
    dist: Vec<f64>,
    metres: Vec<f64>,
    prev: Vec<(u32, u32)>,
}

thread_local! {
    static POOL: std::cell::RefCell<Vec<Scratch>> = const { std::cell::RefCell::new(vec![]) };
}

impl Scratch {
    fn take(n: usize) -> Scratch {
        let mut sc = POOL.with(|p| p.borrow_mut().pop()).unwrap_or(Scratch {
            stamp: vec![],
            epoch: 0,
            dist: vec![],
            metres: vec![],
            prev: vec![],
        });
        if sc.stamp.len() < n {
            sc.stamp.resize(n, 0);
            sc.dist.resize(n, 0.0);
            sc.metres.resize(n, 0.0);
            sc.prev.resize(n, (0, 0));
        }
        sc.epoch = sc.epoch.wrapping_add(1);
        if sc.epoch == 0 {
            sc.stamp.iter_mut().for_each(|s| *s = 0);
            sc.epoch = 1;
        }
        sc
    }
    fn has(&self, n: u32) -> bool {
        self.stamp[n as usize] == self.epoch
    }
}

pub struct Search {
    sc: Option<Scratch>,
    touched: Vec<u32>, // nodes given a distance, in that order
    pub src: u32,
    order: Vec<u32>, // nodes in the order they were settled
}

impl Drop for Search {
    fn drop(&mut self) {
        if let Some(sc) = self.sc.take() {
            POOL.with(|p| p.borrow_mut().push(sc));
        }
    }
}

impl Search {
    fn sc(&self) -> &Scratch {
        self.sc.as_ref().unwrap()
    }
    pub fn reached(&self, n: u32) -> Option<f64> {
        let sc = self.sc();
        sc.has(n).then(|| sc.metres[n as usize])
    }
    /// Every node given a distance, with its metres from the source.
    pub fn all_reached(&self) -> impl Iterator<Item = (u32, f64)> + '_ {
        let sc = self.sc();
        self.touched
            .iter()
            .map(move |&n| (n, sc.metres[n as usize]))
    }
    /// The node before n on the way from the source, and the way between.
    fn prev(&self, n: u32) -> (u32, u32) {
        let sc = self.sc();
        assert!(sc.has(n) && n != self.src);
        sc.prev[n as usize]
    }
    /// Nodes and ways from the source to n.
    pub fn path(&self, n: u32) -> (Vec<u32>, Vec<u32>) {
        let (mut path, mut ways) = (vec![n], vec![]);
        while *path.last().unwrap() != self.src {
            let (u, w) = self.prev(*path.last().unwrap());
            path.push(u);
            ways.push(w);
        }
        path.reverse();
        ways.reverse();
        (path, ways)
    }
}

fn ekey(a: u32, b: u32) -> u64 {
    let (a, b) = if a < b { (a, b) } else { (b, a) };
    (a as u64) << 32 | b as u64
}

fn edge_set(path: &[u32]) -> HashSet<u64> {
    path.windows(2).map(|w| ekey(w[0], w[1])).collect()
}

#[derive(Clone)]
pub struct Eval {
    pub one_way: f64,
    pub total: f64,
    pub gain: f64,
    pub loss: f64,
    pub max_ele: f64,
    pub min_ele: f64,
    pub lc_shares: Vec<(&'static str, f64)>,
    pub lc_variety: f64,
    pub open_high: f64,
    pub passed: Vec<(Kind, Vec<String>)>,
    pub camps: Vec<(String, f64, P)>,
    pub road: f64,
    pub bike: f64,
    pub via: Vec<String>,
    pub line: Vec<P>,
}

#[derive(Clone)]
pub struct Cand {
    pub from: String,
    pub start: P,
    pub dest: String,
    pub dest_kind: Kind,
    pub dest_pt: P,
    pub kind: &'static str,
    pub score: f64,
    pub parts: Vec<(&'static str, f64)>,
    pub edges: HashSet<u64>,
    pub ev: Eval,
}

/// An out-and-back day hike, with what its loop (Ctx::loop_of) needs.
pub struct OutBack {
    pub cand: Cand,
    di: usize,
    src: u32,
    node: u32,
    m: f64,
    path: Vec<u32>,
    ways: Vec<u32>,
}

/// Merge destinations that are the same place: same kind within 400 m with
/// the same name once any "(...)" is dropped, or one name inside the other
/// within 500 m ("Panorama Ridge" and "Panorama Ridge overlook"). Keeps the
/// most appealing, then the most popular, then the shortest name.
pub fn merge_dests(mut dests: Vec<Dest>) -> Vec<Dest> {
    let base = |n: &str| {
        let mut s = String::new();
        let mut depth = 0;
        for c in n.chars() {
            match c {
                '(' => depth += 1,
                ')' if depth > 0 => depth -= 1,
                _ if depth == 0 => s.push(c),
                _ => {}
            }
        }
        s.trim().to_lowercase()
    };
    dests.sort_by(|a, b| {
        b.kind
            .appeal()
            .partial_cmp(&a.kind.appeal())
            .unwrap()
            .then(b.qrank.partial_cmp(&a.qrank).unwrap())
            .then(a.name.len().cmp(&b.name.len()))
    });
    let mut out: Vec<Dest> = Vec::new();
    for d in dests {
        let bd = base(&d.name);
        let dup = out.iter().any(|o| {
            let bo = base(&o.name);
            let m = metres(o.pt, d.pt);
            let reach = if o.kind == Kind::Lake || d.kind == Kind::Lake {
                1500.0
            } else {
                0.0
            };
            (o.kind == d.kind && bo == bd && m < 400.0 + reach)
                || ((bo.contains(&bd) || bd.contains(&bo))
                    && m < 500.0 + reach
                    && o.kind != Kind::Lake
                    && d.kind != Kind::Lake)
        });
        if !dup {
            out.push(d);
        }
    }
    out
}

impl<'a> Ctx<'a> {
    pub fn new(area: &'a Area, dem: &'a Dem, dests: Vec<Dest>, camps: Vec<Place>) -> Ctx<'a> {
        let g = &area.graph;
        let mut grid: HashMap<(u32, u32), Vec<u32>> = HashMap::default();
        for (i, &p) in g.pts.iter().enumerate() {
            if !g.adj[i].is_empty() {
                grid.entry(geo::tile_of(p)).or_default().push(i as u32);
            }
        }
        let mut ctx = Ctx {
            area,
            dem,
            grid,
            camp_near: HashMap::default(),
            camps: vec![],
            dests,
            lake_access: HashMap::default(),
            dest_near: vec![],
            in_park: vec![],
        };
        ctx.dest_near = ctx
            .dests
            .par_iter()
            .map(|d| {
                if d.lake.is_some() {
                    vec![]
                } else {
                    ctx.nodes_near(d.pt, 100.0)
                }
            })
            .collect();
        ctx.in_park = g.pts.par_iter().map(|&p| area.in_park(p)).collect();
        // Stopovers are backcountry campsites and huts: not ones you can
        // drive to (no road within 400 m).
        ctx.camps = camps
            .into_iter()
            .filter(|c| {
                !ctx.nodes_near(c.pt, 400.0).iter().any(|&n| {
                    area.graph.adj[n as usize]
                        .iter()
                        .any(|e| area.graph.ways[e.way as usize].class == Class::Road)
                })
            })
            .collect();
        for (ci, c) in ctx.camps.iter().enumerate() {
            for n in ctx.nodes_near(c.pt, 150.0) {
                ctx.camp_near.entry(n).or_default().push(ci as u32);
            }
        }
        ctx
    }

    pub fn nodes_near(&self, p: P, r: f64) -> Vec<u32> {
        let (tx, ty) = geo::tile_of(p);
        let mut out = vec![];
        for x in tx.saturating_sub(1)..=tx + 1 {
            for y in ty.saturating_sub(1)..=ty + 1 {
                for &n in self.grid.get(&(x, y)).map(|v| &v[..]).unwrap_or(&[]) {
                    if metres(self.area.graph.pts[n as usize], p) <= r {
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    pub fn nearest(&self, p: P, r: f64) -> Option<u32> {
        self.nodes_near(p, r).into_iter().min_by(|&a, &b| {
            let g = &self.area.graph.pts;
            metres(g[a as usize], p)
                .partial_cmp(&metres(g[b as usize], p))
                .unwrap()
        })
    }

    /// Dijkstra by cost, out to `limit` metres. `penal` edges cost 8x.
    pub fn search(&self, src: u32, limit: f64, penal: Option<&HashSet<u64>>) -> Search {
        self.search_until(src, limit, penal, None)
    }

    /// As search, but stops once `target` is settled, when only it is wanted:
    /// a node's distance and path are final once it leaves the queue, and
    /// every node given a distance leaves the queue, so `reached(target)` and
    /// `path(target)` are the same as a full search's.
    pub fn search_to(
        &self,
        src: u32,
        limit: f64,
        penal: Option<&HashSet<u64>>,
        target: u32,
    ) -> Search {
        self.search_until(src, limit, penal, Some(target))
    }

    fn search_until(
        &self,
        src: u32,
        limit: f64,
        penal: Option<&HashSet<u64>>,
        target: Option<u32>,
    ) -> Search {
        let _t = Tm(&T_SEARCH, std::time::Instant::now());
        N_SEARCH.fetch_add(1, Relaxed);
        let g = &self.area.graph;
        let mut sc = Scratch::take(g.pts.len());
        let mut touched = vec![src];
        let mut order = vec![];
        sc.stamp[src as usize] = sc.epoch;
        sc.dist[src as usize] = 0.0;
        sc.metres[src as usize] = 0.0;
        let mut pq = BinaryHeap::new();
        pq.push(Q(0.0, src));
        while let Some(Q(d, u)) = pq.pop() {
            if d > sc.dist[u as usize] {
                continue;
            }
            if target == Some(u) {
                break;
            }
            let mu = sc.metres[u as usize];
            if mu > limit {
                continue;
            }
            order.push(u);
            for e in &g.adj[u as usize] {
                let mut c = e.cost as f64;
                if penal.is_some_and(|p| p.contains(&ekey(u, e.to))) {
                    c *= 8.0;
                }
                let nd = d + c;
                let v = e.to as usize;
                let seen = sc.stamp[v] == sc.epoch;
                if !seen || nd < sc.dist[v] {
                    if !seen {
                        sc.stamp[v] = sc.epoch;
                        touched.push(e.to);
                    }
                    sc.dist[v] = nd;
                    sc.metres[v] = mu + e.m as f64;
                    sc.prev[v] = (u, e.way);
                    pq.push(Q(nd, e.to));
                }
            }
        }
        let s = Search {
            sc: Some(sc),
            touched,
            src,
            order,
        };
        N_SETTLED.fetch_add(s.order.len() as u64, Relaxed);
        s
    }

    /// Measure a route. `path` is the outward route; for an out-and-back the
    /// walk is out and back along it; otherwise `full` is the whole walk.
    pub fn evaluate(&self, path: &[u32], ways: &[u32], out_and_back: bool) -> Eval {
        let _t = Tm(&T_EVAL, std::time::Instant::now());
        let g = &self.area.graph;
        let coords: Vec<P> = path.iter().map(|&n| g.pts[n as usize]).collect();
        let one_way = geo::length(&coords);
        let full: Vec<P> = if out_and_back {
            coords
                .iter()
                .chain(coords.iter().rev().skip(1))
                .copied()
                .collect()
        } else {
            coords.clone()
        };
        let total = geo::length(&full);
        // land cover along the unique part of the route
        let mut lc = [0.0f64; 256];
        let uniq = &coords;
        let samples = geo::sample(uniq, 50.0);
        for &p in &samples {
            lc[self.area.landcover(p) as usize] += 1.0;
        }
        let known: f64 = lc[..LANDCOVER.len()].iter().sum();
        let mut lc_shares: Vec<(&'static str, f64)> = (0..LANDCOVER.len())
            .filter(|&c| lc[c] > 0.0)
            .map(|c| (LANDCOVER[c], lc[c] / known))
            .collect();
        lc_shares.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let lc_variety = if lc_shares.is_empty() {
            1.0
        } else {
            (-lc_shares.iter().map(|(_, s)| s * s.ln()).sum::<f64>()).exp()
        };
        let alpine_share = (0..LANDCOVER.len())
            .filter(|&c| alpine(c as u8))
            .map(|c| lc[c])
            .sum::<f64>()
            / (known + lc[OTHER as usize]).max(1.0);
        // elevation
        let prof = geo::sample(&full, 25.0);
        let ele: Vec<f64> = self.dem.profile(&prof);
        let (gain, loss) = gain_loss(&ele, 10.0);
        let high = ele.iter().filter(|&&e| e >= 1650.0).count() as f64 / ele.len() as f64;
        // destinations passed within 100 m
        let mut passed: HashMap<Kind, Vec<String>> = HashMap::default();
        let near: HashSet<u32> = path.iter().copied().collect();
        for (d, dn) in self.dests.iter().zip(&self.dest_near) {
            if dn.iter().any(|n| near.contains(n)) {
                passed.entry(d.kind).or_default().push(d.name.clone());
            }
        }
        // lakes: passed if the route comes within 75 m of the shore
        for d in &self.dests {
            if let Some(id) = d.lake {
                if self
                    .lake_access
                    .get(&id)
                    .is_some_and(|ns| ns.iter().any(|n| near.contains(n)))
                {
                    passed.entry(Kind::Lake).or_default().push(d.name.clone());
                }
            }
        }
        let mut passed: Vec<(Kind, Vec<String>)> = passed.into_iter().collect();
        passed.sort_by_key(|(k, _)| k.name());
        // campsites along the way, at their distance from the start
        let mut camps: Vec<(String, f64, P)> = vec![];
        let walk: Vec<u32> = if out_and_back {
            path.iter()
                .chain(path.iter().rev().skip(1))
                .copied()
                .collect()
        } else {
            path.to_vec()
        };
        for &n in &walk {
            for &ci in self.camp_near.get(&n).map(|v| &v[..]).unwrap_or(&[]) {
                let c = &self.camps[ci as usize];
                if camps.iter().all(|o| o.0 != c.name) {
                    camps.push((c.name.clone(), geo::project(&full, c.pt), c.pt));
                }
            }
        }
        camps.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let share = |f: &dyn Fn(u32) -> bool| -> f64 {
            path.windows(2)
                .zip(ways)
                .filter(|(_, w)| f(**w))
                .map(|(w, _)| metres(g.pts[w[0] as usize], g.pts[w[1] as usize]))
                .sum::<f64>()
                / one_way.max(1.0)
        };
        let road = share(&|w| g.ways[w as usize].class == Class::Road);
        let bike = share(&|w| g.ways[w as usize].bike);
        let mut via: Vec<String> = vec![];
        for &w in ways {
            let n = &g.ways[w as usize].name;
            if !n.is_empty() && via.last() != Some(n) && !via.contains(n) {
                via.push(n.clone());
            }
        }
        Eval {
            one_way,
            total,
            gain,
            loss,
            max_ele: ele.iter().cloned().fold(f64::MIN, f64::max),
            min_ele: ele.iter().cloned().fold(f64::MAX, f64::min),
            lc_shares,
            lc_variety,
            open_high: alpine_share * high,
            passed,
            camps,
            road,
            bike,
            via,
            line: full,
        }
    }
}

// The weights for scoring a hike.
const W_APPEAL: f64 = 2.0;
const W_POPULARITY: f64 = 1.5;
const W_KINDS: f64 = 0.6;
const W_LC_VARIETY: f64 = 0.7;
const W_OPEN_HIGH: f64 = 2.5;
const W_RELIEF: f64 = 0.8;
const W_LOOP: f64 = 0.5;
const W_ROAD: f64 = -2.5;
const W_BIKE: f64 = -2.0;
const W_TOO_LONG: f64 = -1.5;
const W_TOO_MUCH_GAIN: f64 = -1.0;

pub fn score(ev: &Eval, d: &Dest, is_loop: bool) -> (f64, Vec<(&'static str, f64)>) {
    let pop = d.qrank.max(1.0).log10();
    let mut appeal = d.kind.appeal();
    if d.kind == Kind::Lake {
        // bigger lakes are bigger draws: 0.1 km² -> 0.7, 10 km² -> 1.0
        appeal = (0.7 + 0.15 * (d.area.max(1e5) / 1e5).log10()).min(1.1);
    }
    let mut kinds: HashSet<Kind> = ev.passed.iter().map(|(k, _)| *k).collect();
    kinds.insert(d.kind);
    let relief = ((ev.max_ele - ev.min_ele) / 1200.0).min(1.0);
    let parts = vec![
        ("appeal", W_APPEAL * appeal),
        ("popularity", W_POPULARITY * pop / 5.0),
        (
            "feature_kinds",
            W_KINDS * kinds.len().min(5) as f64 / 5.0 * 3.0,
        ),
        (
            "lc_variety",
            W_LC_VARIETY * ev.lc_variety.min(4.0) / 4.0 * 2.0,
        ),
        ("open_high", W_OPEN_HIGH * ev.open_high),
        ("relief", W_RELIEF * relief),
        ("loop", if is_loop { W_LOOP } else { 0.0 }),
        ("road", W_ROAD * ev.road),
        ("bike_trails", W_BIKE * ev.bike),
        (
            "too_long",
            W_TOO_LONG * ((ev.total - 20000.0) / 10000.0).max(0.0),
        ),
        (
            "too_much_gain",
            W_TOO_MUCH_GAIN * ((ev.gain - 1700.0) / 600.0).max(0.0),
        ),
    ];
    let s = parts.iter().map(|p| p.1).sum();
    (
        s,
        parts
            .into_iter()
            .filter(|p| p.1.abs() > 0.005)
            .map(|(k, v)| (k, (v * 100.0).round() / 100.0))
            .collect(),
    )
}

pub struct Start {
    pub name: String,
    pub node: u32,
    pub pt: P,
    pub trailhead: bool,
}

impl<'a> Ctx<'a> {
    /// Destinations, each with the network nodes it can be reached from.
    pub fn targets(&self, park_m: f64) -> Vec<(usize, Vec<u32>)> {
        let mut out = vec![];
        for (i, d) in self.dests.iter().enumerate() {
            if self.area.park_dist(d.pt, park_m).is_none() && d.lake.is_none() {
                continue;
            }
            if let Some(id) = d.lake {
                if let Some(ns) = self.lake_access.get(&id) {
                    // a lake counts if any of its access points is near the park
                    if ns.iter().any(|&n| {
                        self.area
                            .park_dist(self.area.graph.pts[n as usize], park_m)
                            .is_some()
                    }) {
                        out.push((i, ns.clone()));
                    }
                }
                continue;
            }
            if let Some(n) = self.nearest(d.pt, 150.0) {
                out.push((i, vec![n]));
            }
        }
        out
    }

    /// Day hikes from `src`: an out-and-back to each destination reached
    /// between min_m and max_m, and a loop back by a different way where
    /// there's a good one.
    pub fn candidates(
        &self,
        src: u32,
        src_name: &str,
        targets: &[(usize, Vec<u32>)],
        min_m: f64,
        max_m: f64,
    ) -> Vec<Cand> {
        let mut out = vec![];
        for ob in self.out_and_backs(src, src_name, targets, min_m, max_m) {
            let lp = self.loop_of(&ob);
            out.push(ob.cand);
            out.extend(lp);
        }
        out
    }

    /// The out-and-backs of `candidates`, each with what its loop needs.
    pub fn out_and_backs(
        &self,
        src: u32,
        src_name: &str,
        targets: &[(usize, Vec<u32>)],
        min_m: f64,
        max_m: f64,
    ) -> Vec<OutBack> {
        let s = self.search(src, max_m, None);
        let mut best: HashMap<&str, (usize, f64, u32)> = HashMap::default();
        for (di, access) in targets {
            let Some((node, m)) = access
                .iter()
                .filter_map(|&n| s.reached(n).map(|m| (n, m)))
                .filter(|&(_, m)| m >= min_m && m <= max_m)
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            else {
                continue;
            };
            let name = self.dests[*di].name.as_str();
            if best.get(name).is_some_and(|b| b.1 <= m) {
                continue;
            }
            best.insert(name, (*di, m, node));
        }
        let g = &self.area.graph;
        let mut out = vec![];
        for (_, (di, m, node)) in best {
            let d = &self.dests[di];
            let (path, ways) = s.path(node);
            let ev = self.evaluate(&path, &ways, true);
            let (sc, parts) = score(&ev, d, false);
            let cand = Cand {
                from: src_name.to_string(),
                start: g.pts[src as usize],
                dest: d.name.clone(),
                dest_kind: d.kind,
                dest_pt: g.pts[node as usize],
                kind: "out-and-back",
                score: sc,
                parts,
                edges: edge_set(&path),
                ev,
            };
            out.push(OutBack {
                cand,
                di,
                src,
                node,
                m,
                path,
                ways,
            });
        }
        out
    }

    /// The loop for an out-and-back: back by a different way, if there's a
    /// good one.
    pub fn loop_of(&self, ob: &OutBack) -> Option<Cand> {
        let (src, node, m, pen) = (ob.src, ob.node, ob.m, &ob.cand.edges);
        let b = self.search_to(node, m * 1.7, Some(pen), src);
        let bm = b.reached(src)?;
        let (back, bways) = b.path(src);
        let be = edge_set(&back);
        let shared = be.intersection(pen).count() as f64 / be.len().max(1) as f64;
        if shared >= 0.3 || bm > m * 1.6 {
            return None;
        }
        let full: Vec<u32> = ob.path.iter().chain(back.iter().skip(1)).copied().collect();
        let fw: Vec<u32> = ob.ways.iter().chain(bways.iter()).copied().collect();
        let ev = self.evaluate(&full, &fw, false);
        let (sc, parts) = score(&ev, &self.dests[ob.di], true);
        let c = &ob.cand;
        Some(Cand {
            from: c.from.clone(),
            start: c.start,
            dest: c.dest.clone(),
            dest_kind: c.dest_kind,
            dest_pt: c.dest_pt,
            kind: "loop",
            score: sc,
            parts,
            edges: edge_set(&full),
            ev,
        })
    }

    /// The first named trail (else road) reached walking out from a node.
    fn trail_name_near(&self, node: u32) -> Option<String> {
        let g = &self.area.graph;
        let s = self.search(node, 1500.0, None);
        let mut order: Vec<(f64, u32)> = s.all_reached().map(|(n, m)| (m, n)).collect();
        order.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut road = None;
        for (_, n) in order {
            for e in &g.adj[n as usize] {
                let w = &g.ways[e.way as usize];
                if w.name.is_empty() {
                    continue;
                }
                if matches!(w.class, Class::Trail | Class::Track) && !w.bike {
                    return Some(format!("{} trailhead", w.name));
                }
                road = road.or_else(|| Some(format!("Parking off {}", w.name)));
            }
        }
        road
    }

    /// Trailheads: parking lots and trailhead nodes within 5 km of the park,
    /// snapped to the network and merged when within 400 m of each other.
    ///
    /// A trail area (a trail network outside parks) is already a 500 m buffer
    /// around its trails, so its starts must be inside it: 5 km around the
    /// Skerwink Trail reached a lot across Trinity East, and a 19 km road
    /// walk from there outscored nothing.
    pub fn starts(&self, parkings: &[Place], trail_area: bool) -> Vec<Start> {
        let (lot_reach, junction_reach) = if trail_area {
            (0.0, 0.0)
        } else {
            (5000.0, 2000.0)
        };
        let urban = urban_lots(parkings);
        let mut out: Vec<Start> = parkings
            .par_iter()
            .enumerate()
            .filter(|&(i, p)| {
                // Outside the park, skip lots in cities (see urban_lots), unless
                // they're mapped as trailheads: a city park otherwise gets a
                // start at every lot in the neighbourhood.
                match self.area.park_dist(p.pt, lot_reach) {
                    None => false,
                    Some(d) => d == 0.0 || p.trailhead || !urban[i],
                }
            })
            .map(|(_, p)| p)
            .filter_map(|p| {
                let node = self.nearest(p.pt, 250.0)?;
                let name = if p.name.is_empty() {
                    self.trail_name_near(node)
                        .unwrap_or_else(|| "Parking".into())
                } else {
                    p.name.clone()
                };
                Some(Start {
                    name,
                    node,
                    pt: p.pt,
                    trailhead: p.trailhead,
                })
            })
            .collect();
        out.sort_by_key(|s| (!s.trailhead, s.name == "Parking"));
        let mut merged: Vec<Start> = vec![];
        for s in out {
            if merged.iter().all(|m| metres(m.pt, s.pt) >= 400.0) {
                merged.push(s);
            }
        }
        // Where a trail meets a drivable road near the park. The basemap only
        // has parking lots inside parks, so a trailhead whose lot is just
        // outside (Joffre Lakes), or isn't mapped at all, is found this way.
        // Kept only 1 km or more from every other start, so a park crossed by
        // many roads doesn't get a start at every crossing.
        let g = &self.area.graph;
        let is_trail = |e: &crate::area::Edge| {
            let w = &g.ways[e.way as usize];
            w.class == Class::Trail && !w.bike
        };
        let mut junctions: Vec<(f64, u32)> = (0..g.pts.len() as u32)
            .into_par_iter()
            .filter_map(|n| {
                let adj = &g.adj[n as usize];
                if !adj.iter().any(is_trail)
                    || !adj
                        .iter()
                        .any(|e| g.ways[e.way as usize].class == Class::Road)
                {
                    return None;
                }
                self.area
                    .park_dist(g.pts[n as usize], junction_reach)
                    .map(|d| (d, n))
            })
            .collect();
        junctions.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for (_, n) in junctions {
            let pt = g.pts[n as usize];
            if merged.iter().any(|m| metres(m.pt, pt) < 1000.0) {
                continue;
            }
            let name = match self.trail_name_near(n) {
                Some(t) if !t.starts_with("Parking off ") => t,
                Some(r) => r.replacen("Parking off ", "Trail access off ", 1),
                None => "Trail access".into(),
            };
            merged.push(Start {
                name,
                node: n,
                pt,
                trailhead: false,
            });
        }
        merged
    }
}

/// Coverage first (each start's best), then greedily by score x novelty.
pub fn select(cands: &[Cand], n: usize, by_start: bool) -> Vec<Cand> {
    let mut chosen: Vec<Cand> = vec![];
    let mut covered: HashSet<u64> = HashSet::default();
    let novelty = |c: &Cand, covered: &HashSet<u64>| {
        c.edges.difference(covered).count() as f64 / c.edges.len().max(1) as f64
    };
    if by_start {
        let mut best: HashMap<&str, &Cand> = HashMap::default();
        for c in cands {
            if best.get(c.from.as_str()).is_none_or(|b| c.score > b.score) {
                best.insert(&c.from, c);
            }
        }
        let mut v: Vec<&Cand> = best.into_values().collect();
        v.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        for c in v {
            if chosen.len() < n
                && novelty(c, &covered) > 0.5
                && chosen.iter().all(|o| o.dest != c.dest)
            {
                covered.extend(c.edges.iter().copied());
                chosen.push(c.clone());
            }
        }
    }
    let mut done: HashSet<String> = chosen.iter().map(|c| c.dest.clone()).collect();
    // A route to a destination we don't have yet is worth showing even if it
    // shares trail with one we do (Garibaldi Lake on the way to the Black Tusk).
    while chosen.len() < n {
        let pick = cands
            .iter()
            .filter(|c| !done.contains(&c.dest))
            .map(|c| (c.score * novelty(c, &covered).max(0.6).powf(1.5), c))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let Some((_, c)) = pick else { break };
        covered.extend(c.edges.iter().copied());
        done.insert(c.dest.clone());
        chosen.push(c.clone());
    }
    chosen.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    chosen
}

pub struct BaseCamp {
    pub camp: String,
    pub camp_pt: P,
    pub from: String,
    pub approach: Eval,
    pub hikes: Vec<Cand>,
    pub score: f64,
}

pub struct Through {
    pub from: String,
    pub to: String,
    /// "point-to-point", or "loop" back to `from`.
    pub kind: &'static str,
    pub score: f64,
    pub ev: Eval,
    edges: HashSet<u64>,
}

pub struct Curated {
    pub day: Vec<Cand>,
    pub bases: Vec<BaseCamp>,
    pub through: Vec<Through>,
    pub stats: String,
    /// Whether the deadline passed, so some trailheads or campsites were
    /// skipped: the routes are what was found by then.
    pub timed_out: bool,
}

const LOOPS_PER_DEST: usize = 8;
const MULTI_STARTS_PER_CAMP: usize = 6;

/// Curate a park's routes. With a `deadline`, trailheads and campsites not
/// started by then are skipped, so a pathological park can't hold up a whole
/// run: it's reported (Curated::timed_out) instead.
pub fn curate(
    area: &Area,
    dem: &Dem,
    n_day: usize,
    trail_area: bool,
    deadline: Option<std::time::Instant>,
) -> Curated {
    let late_flag = std::sync::atomic::AtomicBool::new(false);
    let late = || {
        let l = deadline.is_some_and(|d| std::time::Instant::now() > d);
        if l {
            late_flag.store(true, Relaxed);
        }
        l
    };
    let dests = merge_dests(
        area.dests
            .iter()
            .map(|d| Dest {
                name: d.name.clone(),
                kind: d.kind,
                pt: d.pt,
                area: d.area,
                wikidata: d.wikidata.clone(),
                qrank: d.qrank,
                lake: d.lake,
            })
            .collect(),
    );
    let mut camps: Vec<Place> = vec![];
    for c in &area.camps {
        // A hut and the campground beside it are one stopover.
        if camps
            .iter()
            .all(|u| u.name != c.name && metres(u.pt, c.pt) >= 800.0)
        {
            camps.push(Place {
                name: c.name.clone(),
                pt: c.pt,
                trailhead: false,
            });
        }
    }
    let mut ctx = Ctx::new(area, dem, dests, camps);
    ctx.lake_access = area.lake_access().into_iter().collect();
    let targets = ctx.targets(1000.0);
    let starts = ctx.starts(&area.parkings, trail_area);
    let t = std::time::Instant::now();
    let mut stats = format!(
        "{} nodes, {} destinations ({} reachable), {} trailheads, {} backcountry campsites",
        area.graph.pts.len(),
        ctx.dests.len(),
        targets.len(),
        starts.len(),
        ctx.camps.len()
    );
    // day hikes: an out-and-back from every trailhead to each destination,
    // then loops (the expensive part: another search each) only for each
    // destination's LOOPS_PER_DEST best out-and-backs. A city park has
    // hundreds of trailheads (the Mississippi NRRA 642), and a loop from a
    // trailhead with a poor way to a destination is rarely the best.
    let obs: Vec<OutBack> = starts
        .par_iter()
        .flat_map(|s| {
            if late() {
                return vec![];
            }
            ctx.out_and_backs(s.node, &s.name, &targets, 1500.0, 15000.0)
        })
        .collect();
    let mut by_dest: HashMap<&str, Vec<usize>> = HashMap::default();
    for (i, ob) in obs.iter().enumerate() {
        by_dest.entry(ob.cand.dest.as_str()).or_default().push(i);
    }
    let mut loop_from: Vec<usize> = vec![];
    for v in by_dest.values_mut() {
        v.sort_by(|&a, &b| {
            obs[b]
                .cand
                .score
                .partial_cmp(&obs[a].cand.score)
                .unwrap()
                .then(a.cmp(&b))
        });
        loop_from.extend(v.iter().take(LOOPS_PER_DEST));
    }
    loop_from.sort();
    let loops: Vec<Option<Cand>> = loop_from
        .par_iter()
        .map(|&i| if late() { None } else { ctx.loop_of(&obs[i]) })
        .collect();
    let mut cands: Vec<Cand> = vec![];
    let mut loops = loop_from.iter().zip(loops).peekable();
    for (i, ob) in obs.into_iter().enumerate() {
        cands.push(ob.cand);
        if loops.peek().is_some_and(|(j, _)| **j == i) {
            cands.extend(loops.next().unwrap().1);
        }
    }
    let day = select(&cands, n_day, true);
    stats += &format!("; day {:.1}s", t.elapsed().as_secs_f64());

    // base camps: campsites in the park, reached from the nearest trailhead,
    // with day hikes from them
    let start_nodes: HashMap<u32, &Start> = starts.iter().map(|s| (s.node, s)).collect();
    let mut bases: Vec<BaseCamp> = ctx
        .camps
        .par_iter()
        .filter(|c| area.park_dist(c.pt, 500.0).is_some())
        .filter_map(|c| {
            if late() {
                return None;
            }
            let cnode = ctx.nearest(c.pt, 200.0)?;
            let s = ctx.search(cnode, 15000.0, None);
            let (snode, m) = start_nodes
                .keys()
                .filter_map(|&n| s.reached(n).map(|m| (n, m)))
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?;
            if m < 2000.0 {
                return None;
            }
            let (mut path, mut ways) = s.path(snode);
            path.reverse();
            ways.reverse();
            let approach = ctx.evaluate(&path, &ways, false); // one way: the walk in
            let hikes = ctx.candidates(cnode, &c.name, &targets, 1000.0, 8000.0);
            let picks = select(&hikes, 4, false);
            if picks.len() < 2 {
                return None;
            }
            let score = picks.iter().map(|h| h.score).sum::<f64>() / picks.len() as f64
                + 0.3 * picks.len() as f64;
            Some(BaseCamp {
                camp: c.name.clone(),
                camp_pt: c.pt,
                from: start_nodes[&snode].name.clone(),
                approach,
                hikes: picks,
                score,
            })
        })
        .collect();
    bases.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    bases.truncate((n_day / 3).max(1) + 1);
    stats += &format!(", base {:.1}s", t.elapsed().as_secs_f64());

    // multi-day point-to-points: trailhead to trailhead via 2+ campsites. A
    // park with fewer than 2 backcountry campsites can't have one: skip the
    // (100 km) searches.
    // A multi-day route starts at a trailhead near its first campsite, so
    // search only from each campsite's MULTI_STARTS_PER_CAMP nearest
    // trailheads: a park with many trailheads and a few campsites (the
    // Mississippi NRRA: 642 and 2) otherwise searched 100 km from them all.
    let multi_starts: Vec<&Start> = if ctx.camps.len() >= 2 {
        let mut keep = vec![false; starts.len()];
        for c in &ctx.camps {
            let mut by_dist: Vec<(f64, usize)> = starts
                .iter()
                .enumerate()
                .map(|(i, s)| (metres(s.pt, c.pt), i))
                .collect();
            by_dist.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for &(_, i) in by_dist.iter().take(MULTI_STARTS_PER_CAMP) {
                keep[i] = true;
            }
        }
        starts
            .iter()
            .zip(keep)
            .filter(|(_, k)| *k)
            .map(|(s, _)| s)
            .collect()
    } else {
        vec![]
    };
    let mut thru: Vec<Through> = multi_starts
        .par_iter()
        .flat_map(|a| {
            let mut out = vec![];
            if late() || area.park_dist(a.pt, 2000.0).is_none() {
                return out;
            }
            // Up to 100 km: the West Coast Trail is ~75 km.
            let s = ctx.search(a.node, 100000.0, None);
            // Along the search tree: campsites passed (roughly: entering a
            // campsite's surroundings) and metres in the park.
            let mut camps: HashMap<u32, (u32, u32, f64)> = HashMap::default(); // node -> (count, last camp, metres in park)
            camps.insert(a.node, (0, u32::MAX, 0.0));
            for &v in &s.order[1..] {
                let (u, _) = s.prev(v);
                let (mut n, mut last, mut inp) = camps[&u];
                if let Some(cs) = ctx.camp_near.get(&v) {
                    if !cs.contains(&last) {
                        n += 1;
                        last = cs[0];
                    }
                }
                if ctx.in_park[v as usize] {
                    inp += s.reached(v).unwrap() - s.reached(u).unwrap();
                }
                camps.insert(v, (n, last, inp));
            }
            // Evaluating a route (elevation, land cover) is the expensive part,
            // and the 100 km search reaches many trailheads: evaluate only the
            // 8 most promising, by campsites passed and share in the park. Only
            // 3 multi-day routes per park are kept.
            let mut pairs: Vec<(&Start, u32, f64)> = starts
                .iter()
                .filter_map(|b| {
                    let m = s.reached(b.node)?;
                    if b.node == a.node || !(18000.0..=100000.0).contains(&m) {
                        return None;
                    }
                    let &(n, _, inp) = camps.get(&b.node)?;
                    (n >= 2 && inp / m >= 0.6).then_some((b, n, inp / m))
                })
                .collect();
            pairs.sort_by(|x, y| (y.1, y.2).partial_cmp(&(x.1, x.2)).unwrap());
            for &(b, _, _) in pairs.iter().take(8) {
                let (path, ways) = s.path(b.node);
                let ev = ctx.evaluate(&path, &ways, false);
                let kinds: HashSet<Kind> = ev.passed.iter().map(|(k, _)| *k).collect();
                let sc =
                    1.0 * kinds.len() as f64 + 2.5 * ev.open_high + 0.7 * ev.lc_variety.min(4.0)
                        - 2.5 * ev.road
                        - 2.0 * ev.bike;
                out.push(Through {
                    from: a.name.clone(),
                    to: b.name.clone(),
                    kind: "point-to-point",
                    score: sc,
                    ev,
                    edges: edge_set(&path),
                });
            }
            // Multi-day loops back to this trailhead (e.g. Killarney's La
            // Cloche Silhouette), built as day-hike loops are (see candidates):
            // out to a far backcountry campsite, back by a different way.
            let g = &area.graph;
            let mut far: Vec<(u32, f64)> = ctx
                .camps
                .iter()
                .filter_map(|c| ctx.nearest(c.pt, 200.0))
                .filter_map(|n| s.reached(n).map(|m| (n, m)))
                .filter(|&(_, m)| (9000.0..=50000.0).contains(&m))
                .collect();
            far.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());
            far.dedup_by_key(|x| x.0);
            // The return search is the expensive part: bound it by the distance
            // a loop may still add (100 km in all, back at most 1.6x the way
            // out), and try the 3 farthest campsites.
            for &(turn, m) in far.iter().take(3) {
                let back_max = (m * 1.6).min(100000.0 - m);
                if back_max < 18000.0 - m {
                    continue;
                }
                let (path, ways) = s.path(turn);
                let pen = edge_set(&path);
                let b = ctx.search_to(turn, back_max, Some(&pen), a.node);
                let Some(bm) = b.reached(a.node) else {
                    continue;
                };
                let (back, bways) = b.path(a.node);
                let be = edge_set(&back);
                let shared = be.intersection(&pen).count() as f64 / be.len().max(1) as f64;
                let total = m + bm;
                if shared >= 0.3 || bm > m * 1.6 || !(18000.0..=100000.0).contains(&total) {
                    continue;
                }
                let full: Vec<u32> = path.iter().chain(back.iter().skip(1)).copied().collect();
                let fw: Vec<u32> = ways.iter().chain(bways.iter()).copied().collect();
                // Campsites passed, and metres in the park.
                let mut passed: HashSet<u32> = HashSet::default();
                let mut inp = 0.0;
                for w in full.windows(2) {
                    if let Some(cs) = ctx.camp_near.get(&w[1]) {
                        passed.extend(cs.iter().copied());
                    }
                    if ctx.in_park[w[1] as usize] {
                        inp += metres(g.pts[w[0] as usize], g.pts[w[1] as usize]);
                    }
                }
                if passed.len() < 2 || inp / total < 0.6 {
                    continue;
                }
                let ev = ctx.evaluate(&full, &fw, false);
                let kinds: HashSet<Kind> = ev.passed.iter().map(|(k, _)| *k).collect();
                let sc =
                    1.0 * kinds.len() as f64 + 2.5 * ev.open_high + 0.7 * ev.lc_variety.min(4.0)
                        - 2.5 * ev.road
                        - 2.0 * ev.bike;
                out.push(Through {
                    from: a.name.clone(),
                    to: a.name.clone(),
                    kind: "loop",
                    score: sc,
                    ev,
                    edges: edge_set(&full),
                });
            }
            out
        })
        .collect();
    thru.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let mut through: Vec<Through> = vec![];
    let mut seen: HashSet<(String, String)> = HashSet::default();
    for t in thru {
        let key = if t.from < t.to {
            (t.from.clone(), t.to.clone())
        } else {
            (t.to.clone(), t.from.clone())
        };
        if seen.contains(&key)
            || through
                .iter()
                .any(|u| t.edges.intersection(&u.edges).count() as f64 / t.edges.len() as f64 > 0.7)
        {
            continue;
        }
        seen.insert(key);
        through.push(t);
        if through.len() >= 3 {
            break;
        }
    }
    stats += &format!(", multi {:.1}s", t.elapsed().as_secs_f64());
    let timed_out = late_flag.load(Relaxed);
    if timed_out {
        stats += " (TIMEOUT: some trailheads skipped)";
    }
    Curated {
        day,
        bases,
        through,
        stats,
        timed_out,
    }
}

/// Whether each lot is in a city: 12 or more other lots within 1 km. Remote
/// trailheads have a handful nearby (Joffre Lakes: at most 6); city parks'
/// neighbourhoods have dozens (Middlesex Fells: a median of 34).
pub const URBAN_LOTS: usize = 12;
pub fn urban_lots(lots: &[Place]) -> Vec<bool> {
    // Bucket into cells at least 1 km wide (in world units; metres per unit
    // vary with latitude), then count neighbours in the 3x3 cells around.
    let size = lots
        .iter()
        .map(|l| 1000.0 / mpu(l.pt.1))
        .fold(0.0, f64::max)
        .max(1e-9);
    let cell = |p: P| ((p.0 / size).floor() as i64, (p.1 / size).floor() as i64);
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::default();
    for (i, l) in lots.iter().enumerate() {
        grid.entry(cell(l.pt)).or_default().push(i);
    }
    lots.par_iter()
        .enumerate()
        .map(|(i, l)| {
            let (cx, cy) = cell(l.pt);
            let mut n = 0;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &j in grid
                        .get(&(cx + dx, cy + dy))
                        .map(|v| v.as_slice())
                        .unwrap_or(&[])
                    {
                        if j != i && metres(l.pt, lots[j].pt) <= 1000.0 {
                            n += 1;
                        }
                    }
                }
            }
            n >= URBAN_LOTS
        })
        .collect()
}

// ------------------------------------------------------------------ output

fn line_json(line: &[P]) -> Value {
    let simple = geo::simplify(line, 3.0 / mpu(line[0].1));
    Value::Array(
        simple
            .into_iter()
            .map(|p| json!(geo::round_ll(p)))
            .collect(),
    )
}

pub fn eval_json(ev: &Eval) -> Value {
    json!({
        "km": (ev.total / 100.0).round() / 10.0,
        "one_way_km": (ev.one_way / 100.0).round() / 10.0,
        "gain": ev.gain.round(),
        "loss": ev.loss.round(),
        "max_ele": ev.max_ele.round(),
        "min_ele": ev.min_ele.round(),
        "landcover": ev.lc_shares.iter().map(|(k, v)| json!([k, (v * 100.0).round() / 100.0])).collect::<Vec<_>>(),
        "passes": ev.passed.iter().map(|(k, v)| json!([k.name(), v])).collect::<Vec<_>>(),
        "stopovers": ev.camps.iter().map(|(n, m, p)| json!({"name": n, "km": (m / 100.0).round() / 10.0, "at": geo::round_ll(*p)})).collect::<Vec<_>>(),
        "via": ev.via,
        "road_share": (ev.road * 100.0).round() / 100.0,
        "line": line_json(&ev.line),
    })
}

pub fn cand_json(c: &Cand) -> Value {
    json!({
        "from": c.from,
        "start": geo::round_ll(c.start),
        "dest": c.dest,
        "dest_kind": c.dest_kind.name(),
        "dest_at": geo::round_ll(c.dest_pt),
        "kind": c.kind,
        "score": (c.score * 100.0).round() / 100.0,
        "why": c.parts.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "route": eval_json(&c.ev),
    })
}

impl Curated {
    pub fn to_json(&self) -> Value {
        json!({
            "day_hikes": self.day.iter().map(cand_json).collect::<Vec<_>>(),
            "base_camps": self.bases.iter().map(|b| json!({
                "camp": b.camp,
                "at": geo::round_ll(b.camp_pt),
                "from": b.from,
                "score": (b.score * 100.0).round() / 100.0,
                "approach": eval_json(&b.approach),
                "hikes": b.hikes.iter().map(cand_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "point_to_points": self.through.iter().map(|t| json!({
                "from": t.from,
                "to": t.to,
                "kind": t.kind,
                "score": (t.score * 100.0).round() / 100.0,
                "route": eval_json(&t.ev),
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lot(m_east: f64, m_north: f64) -> Place {
        // Around 45°N: place lots by metres, converted to world units there.
        let y0 = 0.36 * geo::EXT * 16384.0; // a world y at roughly mid-latitudes
        let u = 1.0 / mpu(y0);
        Place {
            name: String::new(),
            pt: (1000.0 + m_east * u, y0 + m_north * u),
            trailhead: false,
        }
    }

    #[test]
    fn urban_lots_counts_neighbours_within_a_km() {
        // A remote trailhead: 6 lots within a few hundred metres.
        let remote: Vec<Place> = (0..7).map(|i| lot(i as f64 * 80.0, 0.0)).collect();
        assert!(urban_lots(&remote).iter().all(|&u| !u));
        // A city block grid: 30 lots within ~600 m of each other.
        let city: Vec<Place> = (0..30)
            .map(|i| lot((i % 6) as f64 * 120.0, (i / 6) as f64 * 120.0))
            .collect();
        assert!(urban_lots(&city).iter().all(|&u| u));
        // A lot 3 km from the city isn't counted as in it.
        let mut both: Vec<Place> = (0..30)
            .map(|i| lot((i % 6) as f64 * 120.0, (i / 6) as f64 * 120.0))
            .collect();
        both.push(lot(3000.0, 0.0));
        assert!(!urban_lots(&both)[30]);
    }
}
