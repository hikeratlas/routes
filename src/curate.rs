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
    /// Edges (ekey) along an OSM hiking route: the scoring bump for
    /// following one (W_KNOWN), unless switched off (KNOWN_BONUS).
    on_known: HashSet<u64>,
    /// The fewest metres per world unit anywhere in the graph (at its
    /// poleward edge): straight-line units times this undercuts any walk, for
    /// search_to's A*.
    mpu_min: f64,
    /// The graph's edges in one array, node by node: node u's are
    /// adj_e[adj_off[u]..adj_off[u + 1]]. Searches walk these rather than
    /// Graph::adj's one allocation per node, which cost a cache miss per
    /// node settled (reading edges was ~18% of search's time).
    adj_off: Vec<u32>,
    adj_e: Vec<crate::area::Edge>,
}

/// Whether routes get a bump for following an OSM hiking route (routes
/// --no-known-bonus turns it off). With it, the known-hikes check partly
/// measures the bump itself, so the fixture CI reports recall both ways.
pub static KNOWN_BONUS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// Whether to keep every day-hike candidate, not just those picked (routes
/// --candidates), to see why a named regression's hike isn't offered.
pub static CANDIDATES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// An edge whose midpoint is this close to an OSM hiking route is on it.
const ON_KNOWN_M: f64 = 30.0;

/// A priority queue entry, smallest key first. The key is a non-negative
/// cost, kept as its bits: those order like the numbers for non-negative
/// floats, and compare as integers, cheaper than f64's partial_cmp.
#[derive(PartialEq, Eq)]
struct Q(u64, u32);
impl Q {
    fn new(key: f64, node: u32) -> Q {
        debug_assert!(key >= 0.0);
        Q(key.to_bits(), node)
    }
    fn key(&self) -> f64 {
        f64::from_bits(self.0)
    }
}
impl Ord for Q {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.cmp(&self.0)
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
    // Costs under this are exact: the cost of the first node the search
    // left unexpanded (past its limit), as every cheaper node was reached
    // through expanded ones. Infinite if none (or a search to a target).
    exact_below: f64,
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
    /// A lower bound on the cost of the cheapest walk between the source and
    /// n, by any costs at least the ones this search used (edge costs are the
    /// same both ways): its cost where that's exact, else at least
    /// exact_below. None: no bound beyond what the caller has.
    fn cost_bound(&self, n: u32) -> Option<f64> {
        let sc = self.sc();
        if sc.has(n) && sc.dist[n as usize] < self.exact_below {
            Some(sc.dist[n as usize])
        } else if self.exact_below.is_finite() {
            Some(self.exact_below)
        } else {
            None
        }
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
    /// Share of the route along an OSM hiking route (see Ctx::on_known).
    pub known: f64,
    pub via: Vec<String>,
    /// Hardest SAC grade along the route (1-4; 0 if none is graded): 4 is
    /// a scramble (#45).
    pub max_sac: u8,
    pub line: Vec<P>,
}

#[derive(Clone)]
pub struct Cand {
    pub from: String,
    pub start: P,
    pub dest: String,
    pub dest_kind: Kind,
    pub dest_pt: P,
    /// The destination's OSM ids (empty for a named trail walked for itself).
    pub dest_osm_ids: Vec<String>,
    /// The start's OSM id: a trailhead or parking node, or a campsite; ""
    /// for a start found where a trail meets a road.
    pub from_osm_id: String,
    pub kind: &'static str,
    pub score: f64,
    pub parts: Vec<(&'static str, f64)>,
    pub edges: HashSet<u64>,
    pub ev: Eval,
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
        let dup = out.iter().position(|o| {
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
        match dup {
            // The kept copy stands for both: it takes the other's OSM ids.
            Some(k) => {
                for id in d.osm_ids {
                    if !out[k].osm_ids.contains(&id) {
                        out[k].osm_ids.push(id);
                    }
                }
            }
            None => out.push(d),
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
            on_known: HashSet::default(),
            mpu_min: area
                .graph
                .pts
                .iter()
                .map(|p| mpu(p.1))
                .fold(f64::INFINITY, f64::min),
            adj_off: std::iter::once(0)
                .chain(g.adj.iter().scan(0u32, |n, es| {
                    *n += es.len() as u32;
                    Some(*n)
                }))
                .collect(),
            adj_e: g.adj.iter().flatten().copied().collect(),
        };
        if KNOWN_BONUS.load(Relaxed) && !area.known.is_empty() {
            ctx.on_known = on_known_edges(area);
        }
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
        // Unnamed ones are named for the trail they're on, so routes can say
        // where you stop; ones on no named trail are left out.
        let unnamed: Vec<usize> = (0..ctx.camps.len())
            .filter(|&i| ctx.camps[i].name.is_empty())
            .collect();
        for i in unnamed {
            ctx.camps[i].name = ctx
                .trail_on(ctx.camps[i].pt, 150.0)
                .map(|t| format!("Campsite on {t}"))
                .unwrap_or_default();
        }
        ctx.camps.retain(|c| !c.name.is_empty());
        for (ci, c) in ctx.camps.iter().enumerate() {
            for n in ctx.nodes_near(c.pt, 150.0) {
                ctx.camp_near.entry(n).or_default().push(ci as u32);
            }
        }
        ctx
    }

    /// The name of the nearest named trail (or track) within r metres of p.
    fn trail_on(&self, p: P, r: f64) -> Option<String> {
        let g = &self.area.graph;
        self.nodes_near(p, r)
            .into_iter()
            .map(|n| (metres(p, g.pts[n as usize]), n))
            .filter(|&(m, _)| m <= r)
            .collect::<Vec<_>>()
            .into_iter()
            .fold(None::<(f64, String)>, |best, (m, n)| {
                let name = g.adj[n as usize].iter().find_map(|e| {
                    let w = &g.ways[e.way as usize];
                    (matches!(w.class, Class::Trail | Class::Track) && !w.name.is_empty())
                        .then(|| w.name.clone())
                });
                match (best, name) {
                    (Some(b), Some(nm)) if m < b.0 => Some((m, nm)),
                    (None, Some(nm)) => Some((m, nm)),
                    (b, _) => b,
                }
            })
            .map(|(_, n)| n)
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
        self.search_until(src, limit, penal, None, 1.0, None)
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
        self.search_until(src, limit, penal, Some(target), 1.0, None)
    }

    /// As search_to, back to `target` = `back.src` from a node `back` (a
    /// full search, without `penal`) reached: `back` bounds each node's cost
    /// to the target from below, a far closer estimate for the A* than the
    /// straight line, so it settles far fewer nodes, at the same cost.
    pub fn search_back(
        &self,
        src: u32,
        limit: f64,
        penal: Option<&HashSet<u64>>,
        back: &Search,
    ) -> Search {
        self.search_until(src, limit, penal, Some(back.src), 1.0, Some(back))
    }

    /// As search, with roads ROAD_X_MULTI times their cost, for multi-day
    /// routes: between two trailheads the shortest way is often a road
    /// around (Jasper's Tonquin Valley trailheads are 17.5 km apart by road,
    /// 43 km through the valley), but a multi-day hike is the trail.
    pub fn search_trails(&self, src: u32, limit: f64) -> Search {
        self.search_until(src, limit, None, None, ROAD_X_MULTI, None)
    }

    fn search_until(
        &self,
        src: u32,
        limit: f64,
        penal: Option<&HashSet<u64>>,
        target: Option<u32>,
        road_x: f64,
        hint: Option<&Search>,
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
        // Searching to a target is an A*: queued by cost so far plus the
        // straight line to the target (at mpu_min), which no walk there
        // undercuts (an edge costs at least its metres; a zero-cost join is
        // under 2 m, and a node reached more cheaply later is queued again),
        // so the target
        // is settled at the same cost as by Dijkstra, having settled far
        // fewer nodes. Without a target, h is 0: Dijkstra.
        let tp = target.map(|t| g.pts[t as usize]);
        let k = self.mpu_min * (1.0 - 1e-6);
        let h = |v: u32| {
            tp.map_or(0.0, |t| {
                let p = g.pts[v as usize];
                // Not hypot: it guards against overflow that world units
                // (under 2^26) can't reach, and was 13% of curating's CPU.
                let (dx, dy) = (p.0 - t.0, p.1 - t.1);
                let line = (dx * dx + dy * dy).sqrt() * k;
                // Shaved like k, for rounding: hint summed its costs in
                // another order.
                hint.and_then(|b| b.cost_bound(v))
                    .map_or(line, |c| line.max(c * (1.0 - 1e-9)))
            })
        };
        let mut exact_below = f64::INFINITY;
        let mut pq = BinaryHeap::new();
        pq.push(Q::new(h(src), src));
        while let Some(q) = pq.pop() {
            let (f, u) = (q.key(), q.1);
            let d = sc.dist[u as usize];
            if f > d + h(u) {
                continue;
            }
            if target == Some(u) {
                break;
            }
            let mu = sc.metres[u as usize];
            if mu > limit {
                if target.is_none() && exact_below.is_infinite() {
                    exact_below = d;
                }
                continue;
            }
            order.push(u);
            let es = &self.adj_e[self.adj_off[u as usize] as usize..self.adj_off[u as usize + 1] as usize];
            for e in es {
                let mut c = e.cost as f64;
                if penal.is_some_and(|p| p.contains(&ekey(u, e.to))) {
                    c *= 8.0;
                }
                if road_x != 1.0 && g.ways[e.way as usize].class == Class::Road {
                    c *= road_x;
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
                    pq.push(Q::new(nd + h(e.to), e.to));
                }
            }
        }
        let s = Search {
            sc: Some(sc),
            touched,
            src,
            order,
            exact_below,
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
        let known = if self.on_known.is_empty() {
            0.0
        } else {
            path.windows(2)
                .filter(|w| self.on_known.contains(&ekey(w[0], w[1])))
                .map(|w| metres(g.pts[w[0] as usize], g.pts[w[1] as usize]))
                .sum::<f64>()
                / one_way.max(1.0)
        };
        let mut via: Vec<String> = vec![];
        for &w in ways {
            let n = &g.ways[w as usize].name;
            if !n.is_empty() && via.last() != Some(n) && !via.contains(n) {
                via.push(n.clone());
            }
        }
        let max_sac = ways.iter().map(|&w| g.ways[w as usize].sac).max().unwrap_or(0);
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
            known,
            via,
            max_sac,
            line: full,
        }
    }
}

// The weights for scoring a hike.
const W_APPEAL: f64 = 2.0;
/// See Dest::minor.
const MINOR_PEAK_APPEAL: f64 = 0.7;
const W_POPULARITY: f64 = 1.5;
const W_KINDS: f64 = 0.6;
const W_LC_VARIETY: f64 = 0.7;
const W_OPEN_HIGH: f64 = 2.5;
const W_RELIEF: f64 = 0.8;
const W_LOOP: f64 = 0.5;
const W_ROAD: f64 = -2.5;
const W_BIKE: f64 = -2.0;
/// For the share of a route along an OSM hiking route (Eval::known). On the
/// oracle sample, 0.5 / 1.0 / 2.0 found +2 / -1 / +5 known hikes of 270 on
/// the tuning half; 2.0 found +5 of 191 on the held-out half too. That partly
/// measures the bonus itself: the fixture CI also reports recall without it.
const W_KNOWN: f64 = 2.0;
/// A route with more of its length on bike trails than this isn't offered:
/// mountain-bike trails belong on the map, not in hikes, and in a bike park
/// the best of a bad lot still scored as a hike.
pub const MAX_BIKE: f64 = 0.5;
/// A day hike with more of its length on roads open to cars than this isn't
/// offered (#66): Terra Nova's Ochre Hill Fire Tower, a viewpoint with a
/// road to its door, was a 10.6 km walk up that road (98% of it), as the
/// trail from the road's end is under the 1.5 km a day hike needs. Roads closed
/// to cars are tracks (area.rs), so fire roads and the like still count.
pub const MAX_ROAD: f64 = 0.5;
const W_TOO_LONG: f64 = -1.5;
const W_TOO_MUCH_GAIN: f64 = -1.0;

pub fn score(ev: &Eval, d: &Dest, is_loop: bool) -> (f64, Vec<(&'static str, f64)>) {
    let pop = d.qrank.max(1.0).log10();
    let mut appeal = d.kind.appeal();
    if d.minor {
        appeal = MINOR_PEAK_APPEAL;
    }
    if d.kind == Kind::Lake {
        // bigger lakes are bigger draws: 0.1 km² -> 0.7, 10 km² -> 1.0
        appeal = (0.7 + 0.15 * (d.area.max(1e5) / 1e5).log10()).min(1.1);
    }
    if d.kind == Kind::Trail {
        // A named trail walked for itself is worth at least the best sight
        // it passes: Cape Split's Minas Basin Trail passes its lookoffs on
        // the way to the tip, and lost to hikes stopping at them (#109).
        appeal = ev
            .passed
            .iter()
            .map(|(k, _)| k.appeal())
            .fold(appeal, f64::max);
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
        ("known_route", W_KNOWN * ev.known),
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
    /// The lot's or trailhead's OSM id ("" where none: a lot mapped only by
    /// its aisles, or where a trail meets a road).
    pub osm_id: String,
    /// A trailhead, or a parking lot with a name, in OSM. False for an
    /// unnamed lot and for where a trail meets a road (see Ctx::starts):
    /// those are named here, after the trail or road nearby (#63).
    pub known: bool,
    /// Where a trail leaves driveways (see Ctx::starts, routes#156): a
    /// named trail's hike starts here only where it's LOT_EXIT_HANDICAP_M
    /// closer than a lot.
    pub lot_exit: bool,
}

/// Shortest and longest named trail (in the park) to offer for itself.
/// Long named trails end to end, as multi-day point-to-points (#39): the
/// shortest path between a trail's two trailheads often isn't the trail
/// (Sleeping Giant's Kabeyun Trail rounds the peninsula past Tee Harbour;
/// the shortest way between its trailheads cuts inland, 23 km to its 36, and
/// passes one campsite), so trailhead-to-trailhead searches miss it. A named
/// line of 18-100 km in the park that passes 2+ places to sleep, walked from
/// the trailhead nearest one end to the trailhead nearest the other.
/// A line's nodes in order, and the ways between them.
type Line = (Vec<u32>, Vec<u32>);

/// How far outside the park a destination may be on a trail named for it
/// that's partly in the park (see Ctx::targets).
const TRAIL_DEST_M: f64 = 3000.0;

/// How far apart a trail's pieces may be (by search cost: roads count
/// extra) to be chained end to end by chain_pieces. A long trail's name can
/// lapse for a few km where it shares another trail's ways: Killarney's La
/// Cloche Silhouette is three pieces, two of them 2.3 km apart as the crow
/// flies (over 3 km by search cost) where it follows The Crack's trail.
const CHAIN_JOIN_M: f64 = 5000.0;
/// Most pieces chain_pieces orders (k! x 2^k chains).
const CHAIN_MAX_PIECES: usize = 5;

/// A trail's pieces (2 to CHAIN_MAX_PIECES of them) as one line: all of
/// them, in the order and directions whose gaps (each within CHAIN_JOIN_M)
/// add up least, of those whose two ends reach different trailheads
/// (`start_at`: a node's nearest trailhead). The trailheads say where the
/// trip ends: La Cloche Silhouette is a loop but for the 770 m from George
/// Lake to the Cranberry Bog lot, and joining its pieces by the nearest
/// gaps closes that, leaving the line's ends in the backcountry (routes#129).
/// None if there's no such chain (the greedy join then applies).
fn chain_pieces(ctx: &Ctx, ps: &[Line], start_at: &mut dyn FnMut(u32) -> Option<u32>) -> Option<Line> {
    let k = ps.len();
    if !(2..=CHAIN_MAX_PIECES).contains(&k) {
        return None;
    }
    // End e of piece i is node end(i, e): 0 its first, 1 its last.
    let end = |i: usize, e: usize| if e == 0 { ps[i].0[0] } else { *ps[i].0.last().unwrap() };
    // gaps[(i, e)][(j, f)]: metres from end (i, e) to end (j, f).
    let mut gaps: HashMap<(usize, usize), HashMap<(usize, usize), f64>> = HashMap::default();
    for i in 0..k {
        for e in 0..2 {
            let sr = ctx.search(end(i, e), CHAIN_JOIN_M, None);
            for j in (0..k).filter(|&j| j != i) {
                for f in 0..2 {
                    if let Some(m) = sr.reached(end(j, f)) {
                        gaps.entry((i, e)).or_default().insert((j, f), m);
                    }
                }
            }
        }
    }
    // Chains as (pieces in order, each reversed or not); a chain and its
    // reverse are the same line, both tried.
    let mut best: Option<(f64, Vec<(usize, bool)>)> = None;
    let mut order: Vec<(usize, bool)> = vec![];
    fn walk(
        k: usize,
        order: &mut Vec<(usize, bool)>,
        gap: f64,
        gaps: &HashMap<(usize, usize), HashMap<(usize, usize), f64>>,
        done: &mut dyn FnMut(&[(usize, bool)], f64),
    ) {
        if order.len() == k {
            done(order, gap);
            return;
        }
        for i in 0..k {
            if order.iter().any(|&(j, _)| j == i) {
                continue;
            }
            for rev in [false, true] {
                // Into piece i at its end 0 (forward) or 1 (reversed), from
                // the previous piece's exit.
                let add = match order.last() {
                    None => 0.0,
                    Some(&(j, jrev)) => {
                        let exit = (j, if jrev { 0 } else { 1 });
                        let entry = (i, if rev { 1 } else { 0 });
                        match gaps.get(&exit).and_then(|m| m.get(&entry)) {
                            Some(&m) => m,
                            None => continue,
                        }
                    }
                };
                order.push((i, rev));
                walk(k, order, gap + add, gaps, done);
                order.pop();
            }
        }
    }
    let mut ends_ok: HashMap<(u32, u32), bool> = HashMap::default();
    walk(k, &mut order, 0.0, &gaps, &mut |o, gap| {
        if best.as_ref().is_some_and(|b| b.0 <= gap) {
            return;
        }
        let (f, l) = (o[0], o[k - 1]);
        let a = end(f.0, if f.1 { 1 } else { 0 });
        let b = end(l.0, if l.1 { 0 } else { 1 });
        let ok = *ends_ok.entry((a, b)).or_insert_with(|| {
            matches!((start_at(a), start_at(b)), (Some(x), Some(y)) if x != y)
        });
        if ok {
            best = Some((gap, o.to_vec()));
        }
    });
    let (_, o) = best?;
    // The line: each piece in its direction, joined by the shortest way
    // between them.
    let (mut path, mut ways): Line = (vec![], vec![]);
    for (n, &(i, rev)) in o.iter().enumerate() {
        let (mut p, mut w) = ps[i].clone();
        if rev {
            p.reverse();
            w.reverse();
        }
        if n > 0 {
            let sr = ctx.search(*path.last().unwrap(), CHAIN_JOIN_M, None);
            let (gp, gw) = sr.path(p[0]);
            path.extend(gp.iter().skip(1));
            ways.extend(gw);
            path.extend(p.iter().skip(1));
        } else {
            path.extend(p.iter());
        }
        ways.extend(w);
    }
    Some((path, ways))
}

fn named_throughs(ctx: &Ctx, starts: &[Start]) -> Vec<Through> {
    let g = &ctx.area.graph;
    // The trailhead nearest a node, within NAMED_ACCESS_M: the path from
    // the node to it.
    let nearest_start = |n: u32| -> Option<(&Start, Vec<u32>, Vec<u32>)> {
        let sr = ctx.search(n, NAMED_ACCESS_M, None);
        let s = starts
            .iter()
            .filter_map(|s| sr.reached(s.node).map(|m| (m, s)))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())?
            .1;
        let (p, w) = sr.path(s.node);
        Some((s, p, w))
    };
    let mut out = vec![];
    // A trail's line pieces, joined end to end where they're within
    // JOIN_M of each other: the Kabeyun Trail is two pieces (25.8 and 9.3
    // km), split where it leaves the trail network for a few metres.
    const JOIN_M: f64 = 1000.0;
    let mut by_name: HashMap<&str, Vec<Line>> = HashMap::default();
    for (name, path, ways, is_loop) in ctx.named_pieces(NAMED_MIN_M, 100000.0, false) {
        if !is_loop {
            by_name.entry(name).or_default().push((path, ways));
        }
    }
    let mut lines: Vec<(&str, Line)> = vec![];
    // Each piece end's nearest trailhead (its node), for chain_pieces.
    let mut start_at: HashMap<u32, Option<u32>> = HashMap::default();
    for (trail, mut ps) in by_name {
        if let Some(line) = chain_pieces(ctx, &ps, &mut |n| {
            *start_at
                .entry(n)
                .or_insert_with(|| nearest_start(n).map(|(s, _, _)| s.node))
        }) {
            lines.push((trail, line));
            continue;
        }
        // Join any two pieces with ends close together, until none are.
        'join: loop {
            for i in 0..ps.len() {
                for end in [0usize, 1] {
                    let e = if end == 0 {
                        ps[i].0[0]
                    } else {
                        *ps[i].0.last().unwrap()
                    };
                    let sr = ctx.search(e, JOIN_M, None);
                    for k in 0..ps.len() {
                        if k == i {
                            continue;
                        }
                        for kend in [0usize, 1] {
                            let f = if kend == 0 {
                                ps[k].0[0]
                            } else {
                                *ps[k].0.last().unwrap()
                            };
                            if sr.reached(f).is_none() {
                                continue;
                            }
                            // i oriented to end at e, then e to f, then k from f.
                            let (gap, gw) = sr.path(f);
                            let (mut ip, mut iw) = ps[i].clone();
                            if end == 0 {
                                ip.reverse();
                                iw.reverse();
                            }
                            let (mut kp, mut kw) = ps[k].clone();
                            if kend == 1 {
                                kp.reverse();
                                kw.reverse();
                            }
                            ip.extend(gap.iter().skip(1));
                            iw.extend(gw);
                            ip.extend(kp.iter().skip(1));
                            iw.extend(kw);
                            let (a, b) = (i.max(k), i.min(k));
                            ps.remove(a);
                            ps.remove(b);
                            ps.push((ip, iw));
                            continue 'join;
                        }
                    }
                }
            }
            break;
        }
        lines.extend(ps.into_iter().map(|p| (trail, p)));
    }
    for (trail, (path, ways)) in lines {
        let len: f64 = path
            .windows(2)
            .map(|w| metres(g.pts[w[0] as usize], g.pts[w[1] as usize]))
            .sum();
        if !(18000.0..=100000.0).contains(&len) {
            continue;
        }
        let mut passed: HashSet<u32> = HashSet::default();
        for n in &path {
            if let Some(cs) = ctx.camp_near.get(n) {
                passed.extend(cs.iter().copied());
            }
        }
        if passed.len() < 2 {
            continue;
        }
        let (Some((a, ap, aw)), Some((b, bp, bw))) =
            (nearest_start(path[0]), nearest_start(*path.last().unwrap()))
        else {
            continue;
        };
        if a.node == b.node {
            continue;
        }
        // Trailhead a to the trail's start, along it, then to trailhead b.
        let full: Vec<u32> = ap
            .iter()
            .rev()
            .chain(path.iter().skip(1))
            .chain(bp.iter().skip(1))
            .copied()
            .collect();
        let fw: Vec<u32> = aw
            .iter()
            .rev()
            .chain(ways.iter())
            .chain(bw.iter())
            .copied()
            .collect();
        let (mut m, mut inp) = (0.0, 0.0);
        for w in full.windows(2) {
            let d = metres(g.pts[w[0] as usize], g.pts[w[1] as usize]);
            m += d;
            if ctx.in_park[w[1] as usize] {
                inp += d;
            }
        }
        if !(18000.0..=100000.0).contains(&m) || inp / m < 0.6 {
            continue;
        }
        let ev = ctx.evaluate(&full, &fw, false);
        if ev.bike > MAX_BIKE {
            continue;
        }
        let kinds: HashSet<Kind> = ev.passed.iter().map(|(k, _)| *k).collect();
        // A long named trail walked end to end, past campsites, is a trip
        // people know, as much as an OSM hiking route is: the same bump
        // (W_KNOWN), by the share of the trip on the trail. Jasper's
        // Tonquin Valley trail (no hiking route relation) scored 3.97, under
        // the three trips chosen then (#54).
        let on_trail = (len / m).min(1.0);
        let sc = 1.0 * kinds.len() as f64 + 2.5 * ev.open_high + 0.7 * ev.lc_variety.min(4.0)
            - 2.5 * ev.road
            - 2.0 * ev.bike
            + W_KNOWN * on_trail;
        out.push(Through {
            from: a.name.clone(),
            to: b.name.clone(),
            kind: "point-to-point",
            score: sc,
            ev,
            edges: edge_set(&full),
            name: Some(trail.to_string()),
        });
    }
    out
}

/// How near a route must pass a known trailhead to pass it, and how far
/// before a made-up end it must be to count as running past it (#63).
const PASS_KNOWN_M: f64 = 300.0;
const PAST_KNOWN_M: f64 = 1500.0;

/// Whether a line, walked back from its last point, comes within
/// PASS_KNOWN_M of a known start PAST_KNOWN_M or more before its end, not
/// counting starts by its first point (its other end).
fn runs_past_known(line: &[P], known: &[P]) -> bool {
    let Some(&first) = line.first() else {
        return false;
    };
    let others: Vec<P> = known
        .iter()
        .copied()
        .filter(|&p| metres(p, first) > PASS_KNOWN_M)
        .collect();
    let mut acc = 0.0;
    for i in (1..line.len()).rev() {
        acc += metres(line[i], line[i - 1]);
        if others.iter().any(|&p| metres(p, line[i - 1]) <= PASS_KNOWN_M) {
            return acc >= PAST_KNOWN_M;
        }
    }
    false
}

/// Road cost multiplier in multi-day searches (see Ctx::search_trails).
const ROAD_X_MULTI: f64 = 8.0;

const NAMED_MIN_M: f64 = 1000.0;
/// A named trail shorter than NAMED_MIN_M is a day hike too if a trailhead
/// named for it (highway=trailhead, "Bog Trail") is within SIGNED_NEAR_M of
/// it, down to this long: the park signed it for visitors (routes#155).
const SIGNED_MIN_M: f64 = 300.0;
const SIGNED_NEAR_M: f64 = 100.0;
const NAMED_MAX_M: f64 = 15000.0;
/// How far a trailhead may be from a named trail to start it.
const NAMED_ACCESS_M: f64 = 3000.0;
/// Trail tours (Ctx::trail_tour): other trails cost this many times their
/// length, roads this, the way out's edges this much more on the way back;
/// a tour reaches this far; it's used when it walks this many times more of
/// the trail than the piece (its longest line, or simple loop) does.
const TOUR_OTHER_X: f64 = 4.0;
const TOUR_ROAD_X: f64 = 20.0;
const TOUR_BACK_X: f64 = 20.0;
const TOUR_LIMIT_M: f64 = 15000.0;
/// Up to this much unnamed trail joins a trail's pieces for its tour (a
/// footbridge without the trail's name).
const TOUR_GAP_M: f64 = 30.0;
const TOUR_GAIN: f64 = 1.1;
/// ...or when it's a loop where the piece is a line, walking this much of it.
const TOUR_KEEP: f64 = 0.95;
/// How much of a trail a tour from where the way in reaches it must walk, of
/// what the tour from the piece's entry walks, to start there instead.
const TOUR_REACH_KEEP: f64 = 0.95;

impl<'a> Ctx<'a> {
    /// Named trails as hikes in themselves: a trail with a name, in the park,
    /// that's a simple loop or a simple line of NAMED_MIN_M to NAMED_MAX_M,
    /// walked from the nearest trailhead within NAMED_ACCESS_M: around a loop
    /// and back, or out to a line's far end and back. Destination-first
    /// candidates miss these when nothing on them is a destination
    /// (interpretive loops, ridge and creek trails), and they're much of the
    /// OSM hiking routes the oracle sample (fixtures/oracle.json) checks.
    /// Each trail name's connected pieces in the park that are a simple
    /// loop or line (or a forked trail's longest line), between min_m and
    /// max_m long, as (name, nodes in order, ways between them, is a loop).
    #[allow(clippy::type_complexity)]
    /// With `signed`, a trail shorter than min_m counts too where the park
    /// signed a trailhead for it (see SIGNED_MIN_M).
    fn named_pieces(
        &self,
        min_m: f64,
        max_m: f64,
        signed: bool,
    ) -> Vec<(&'a str, Vec<u32>, Vec<u32>, bool)> {
        let g = &self.area.graph;
        // Trailheads OSM names, for short trails the park signed.
        let signs: Vec<&crate::area::Place> = if signed {
            self.area.parkings.iter().filter(|p| p.trailhead && !p.name.is_empty()).collect()
        } else {
            vec![]
        };
        // In-park, non-bike trail edges, by name.
        let mut by_name: HashMap<&'a str, Vec<(u32, u32, u32)>> = HashMap::default();
        for (u, adj) in g.adj.iter().enumerate() {
            let u = u as u32;
            for e in adj {
                let w = &g.ways[e.way as usize];
                if u < e.to
                    && w.class == Class::Trail
                    && !w.bike
                    && !w.name.is_empty()
                    && self.in_park[u as usize]
                    && self.in_park[e.to as usize]
                {
                    by_name
                        .entry(w.name.as_str())
                        .or_default()
                        .push((u, e.to, e.way));
                }
            }
        }
        // Each name's connected pieces that are a simple loop or line, as
        // (name, nodes in order, ways between them, is a loop).
        let mut pieces: Vec<(&'a str, Vec<u32>, Vec<u32>, bool)> = vec![];
        for (name, edges) in &by_name {
            // All of the trail, in every piece: a short trail is often
            // mapped in pieces (Lone Shieling Trail, three).
            let name_m: f64 = edges
                .iter()
                .map(|&(u, v, _)| metres(g.pts[u as usize], g.pts[v as usize]))
                .sum();
            let mut nbrs: HashMap<u32, Vec<(u32, u32)>> = HashMap::default();
            for &(u, v, w) in edges {
                nbrs.entry(u).or_default().push((v, w));
                nbrs.entry(v).or_default().push((u, w));
            }
            let mut seen: HashSet<u32> = HashSet::default();
            let mut nodes: Vec<u32> = nbrs.keys().copied().collect();
            nodes.sort();
            for &n0 in &nodes {
                if seen.contains(&n0) {
                    continue;
                }
                // The piece's nodes.
                let mut comp = vec![n0];
                seen.insert(n0);
                let mut i = 0;
                while i < comp.len() {
                    for &(v, _) in &nbrs[&comp[i]] {
                        if seen.insert(v) {
                            comp.push(v);
                        }
                    }
                    i += 1;
                }
                let branched = comp.iter().any(|n| nbrs[n].len() > 2);
                // All of the piece's trail, each edge once: a forked trail's
                // longest line can be half of it (Algonquin's lollipops:
                // Hardwood Lookout's 1.0 km is a 532 m line, routes#134).
                let total: f64 = comp
                    .iter()
                    .flat_map(|&u| nbrs[&u].iter().filter(move |&&(v, _)| u < v).map(move |&(v, _)| (u, v)))
                    .map(|(u, v)| metres(g.pts[u as usize], g.pts[v as usize]))
                    .sum();
                let ends: Vec<u32> = comp
                    .iter()
                    .copied()
                    .filter(|n| nbrs[n].len() == 1)
                    .collect();
                let is_loop = ends.is_empty() && !branched;
                let (path, ways): (Vec<u32>, Vec<u32>) = if branched || ends.len() != 2 && !is_loop
                {
                    // A trail that forks (side spurs, a lollipop): its
                    // longest end-to-end line, found as the farthest node
                    // from the farthest node from any.
                    let far = |from: u32| -> (u32, HashMap<u32, (u32, u32)>) {
                        let mut dist: HashMap<u32, f64> = HashMap::default();
                        let mut prev: HashMap<u32, (u32, u32)> = HashMap::default();
                        let mut pq = BinaryHeap::new();
                        dist.insert(from, 0.0);
                        pq.push(Q::new(0.0, from));
                        while let Some(q) = pq.pop() {
                            let (d, u) = (q.key(), q.1);
                            if d > dist[&u] {
                                continue;
                            }
                            for &(v, w) in &nbrs[&u] {
                                let nd = d + metres(g.pts[u as usize], g.pts[v as usize]);
                                if nd < *dist.get(&v).unwrap_or(&f64::MAX) {
                                    dist.insert(v, nd);
                                    prev.insert(v, (u, w));
                                    pq.push(Q::new(nd, v));
                                }
                            }
                        }
                        let f = *dist
                            .iter()
                            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap().then(b.0.cmp(a.0)))
                            .unwrap()
                            .0;
                        (f, prev)
                    };
                    let (a, _) = far(n0);
                    let (b, prev) = far(a);
                    let (mut p, mut w) = (vec![b], vec![]);
                    while *p.last().unwrap() != a {
                        let (u, wy) = prev[p.last().unwrap()];
                        p.push(u);
                        w.push(wy);
                    }
                    (p, w)
                } else {
                    // Walk it in order from an end (or anywhere, for a loop).
                    let first = if is_loop {
                        *comp.iter().min().unwrap()
                    } else {
                        ends[0].min(ends[1])
                    };
                    let (mut path, mut ways) = (vec![first], vec![]);
                    let mut prev = u32::MAX;
                    let mut cur = first;
                    loop {
                        let next = nbrs[&cur].iter().find(|&&(v, _)| v != prev);
                        let Some(&(v, w)) = next else { break };
                        if v == first {
                            if is_loop {
                                path.push(v);
                                ways.push(w);
                            }
                            break;
                        }
                        if path.contains(&v) {
                            break;
                        }
                        path.push(v);
                        ways.push(w);
                        prev = cur;
                        cur = v;
                    }
                    if path.len() != comp.len() + is_loop as usize {
                        continue;
                    }
                    (path, ways)
                };
                let m: f64 = path
                    .windows(2)
                    .map(|p| metres(g.pts[p[0] as usize], g.pts[p[1] as usize]))
                    .sum();
                // A forked trail is long enough by all of it: its longest line
                // stands for it, and named_trails walks it whole (trail_tour).
                let long_enough = if branched { total >= min_m } else { m >= min_m };
                // A short trail with its own trailhead sign: the park put it
                // there for visitors (Cape Breton's Bog Trail, a 0.5 km
                // boardwalk, routes#155). Its piece by the sign is enough:
                // trail_tour walks the rest.
                let long_enough = long_enough
                    || (name_m >= SIGNED_MIN_M
                        && signs.iter().any(|t| {
                            named_for(&t.name, name)
                                && comp
                                    .iter()
                                    .any(|&n| metres(g.pts[n as usize], t.pt) <= SIGNED_NEAR_M)
                        }));
                if long_enough && m <= max_m {
                    pieces.push((name, path, ways, is_loop));
                }
            }
        }
        pieces
    }

    /// A named trail walked whole from `entry`: out to its farthest point and
    /// back another way where there is one, as (nodes, ways, whether it's a
    /// loop, metres of the trail it walks). Edges of the trail cost their
    /// length; other trails and tracks TOUR_OTHER_X that (short connectors
    /// where the trail is mapped in pieces, or shares another's ways); roads
    /// TOUR_ROAD_X. The way back costs TOUR_BACK_X more on the way out's
    /// edges, so it comes back the other side of a loop, and the same way
    /// only when it must (a spur, a line).
    fn trail_tour(&self, name: &str, entry: u32) -> Option<(Vec<u32>, Vec<u32>, bool, f64)> {
        let g = &self.area.graph;
        let is_trail = |w: u32| g.ways[w as usize].name == name;
        let cost = |w: u32, m: f64, back: Option<&HashSet<u64>>, k: u64| -> f64 {
            let way = &g.ways[w as usize];
            let base = if way.name == name || way.class == Class::Join {
                m
            } else if matches!(way.class, Class::Trail | Class::Track) && !way.bike {
                m * TOUR_OTHER_X
            } else {
                m * TOUR_ROAD_X
            };
            if back.is_some_and(|b| b.contains(&k)) { base * TOUR_BACK_X } else { base }
        };
        let dijkstra = |src: u32, back: Option<&HashSet<u64>>, limit: f64, target: Option<u32>| {
            let mut dist: HashMap<u32, f64> = HashMap::default();
            let mut prev: HashMap<u32, (u32, u32)> = HashMap::default();
            let mut pq = BinaryHeap::new();
            dist.insert(src, 0.0);
            pq.push(Q::new(0.0, src));
            while let Some(q) = pq.pop() {
                let (d, u) = (q.key(), q.1);
                if Some(u) == target {
                    break;
                }
                if d > dist[&u] || d > limit {
                    continue;
                }
                for e in &g.adj[u as usize] {
                    let nd = d + cost(e.way, e.m as f64, back, ekey(u, e.to));
                    if nd < *dist.get(&e.to).unwrap_or(&f64::MAX) {
                        dist.insert(e.to, nd);
                        prev.insert(e.to, (u, e.way));
                        pq.push(Q::new(nd, e.to));
                    }
                }
            }
            (dist, prev)
        };
        let path_to = |prev: &HashMap<u32, (u32, u32)>, src: u32, dst: u32| {
            let (mut p, mut w) = (vec![dst], vec![]);
            while *p.last().unwrap() != src {
                let (u, wy) = prev[p.last().unwrap()];
                p.push(u);
                w.push(wy);
            }
            p.reverse();
            w.reverse();
            (p, w)
        };
        // The trail's nodes joined to the entry by the trail itself: the
        // tour turns at one of these, not at a stub of the trail mapped
        // apart (Granite Ridge Trail's 160 m piece), which it would have to
        // walk back from.
        // Across short unnamed bits of trail too: a footbridge often lacks
        // the trail's name (MacIntosh Brook Trail's, 11-17 m, routes#155).
        // Nodes by the least unnamed trail walked since the trail itself.
        let unnamed = |w: u32| {
            let way = &g.ways[w as usize];
            way.name.is_empty() && way.class == Class::Trail
        };
        let mut gap_at: HashMap<u32, f64> = HashMap::default();
        let mut stack = vec![(entry, 0.0)];
        gap_at.insert(entry, 0.0);
        let mut trail_edges: HashSet<u64> = HashSet::default();
        while let Some((u, gu)) = stack.pop() {
            if gap_at[&u] < gu {
                continue;
            }
            for e in &g.adj[u as usize] {
                let gv = if is_trail(e.way) {
                    trail_edges.insert(ekey(u, e.to));
                    0.0
                } else if unnamed(e.way) && gu + e.m as f64 <= TOUR_GAP_M {
                    gu + e.m as f64
                } else {
                    continue;
                };
                if gap_at.get(&e.to).is_none_or(|&g0| gv < g0) {
                    gap_at.insert(e.to, gv);
                    stack.push((e.to, gv));
                }
            }
        }
        // The trail's own nodes: on its edges (not the end of a gap that
        // leads nowhere).
        let own: HashSet<u32> = gap_at.iter().filter(|&(_, &d)| d == 0.0).map(|(&n, _)| n).collect();
        let own_m: f64 = trail_edges
            .iter()
            .map(|&k| metres(g.pts[(k >> 32) as usize], g.pts[(k & 0xffff_ffff) as usize]))
            .sum();
        // Searches bounded by the trail's own length, as a city park has
        // hundreds of named trails.
        let limit = (own_m * 1.5 + 500.0).min(TOUR_LIMIT_M);
        let (dist, prev) = dijkstra(entry, None, limit, None);
        // The farthest of them, within reach.
        let far = dist
            .iter()
            .filter(|&(&n, &d)| d <= limit && n != entry && own.contains(&n))
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap().then(b.0.cmp(a.0)))
            .map(|(&n, _)| n)?;
        if far == entry {
            return None;
        }
        let (out_p, out_w) = path_to(&prev, entry, far);
        let out_edges = edge_set(&out_p);
        let (dist2, prev2) = dijkstra(far, Some(&out_edges), limit * TOUR_BACK_X, Some(entry));
        dist2.get(&entry)?;
        let (back_p, back_w) = path_to(&prev2, far, entry);
        let back_edges = edge_set(&back_p);
        let shared =
            back_edges.intersection(&out_edges).count() as f64 / back_edges.len().max(1) as f64;
        let is_loop = shared < 0.5;
        let mut tour = out_p.clone();
        tour.extend(back_p.iter().skip(1));
        let mut tour_w = out_w;
        tour_w.extend(back_w);
        // Metres of the trail walked, each edge once.
        let mut seen: HashSet<u64> = HashSet::default();
        let mut covered = 0.0;
        for (k, w) in tour.windows(2).zip(&tour_w) {
            if is_trail(*w) && seen.insert(ekey(k[0], k[1])) {
                covered += metres(g.pts[k[0] as usize], g.pts[k[1] as usize]);
            }
        }
        Some((tour, tour_w, is_loop, covered))
    }

    pub fn named_trails(&self, starts: &[Start]) -> Vec<Cand> {
        let g = &self.area.graph;
        let pieces = self.named_pieces(NAMED_MIN_M, NAMED_MAX_M, true);
        if pieces.is_empty() {
            return vec![];
        }
        // The nearest trailhead to each piece: to any node of a loop, to
        // either end of a line.
        let entries: Vec<Vec<u32>> = pieces
            .iter()
            .map(|(_, p, _, l)| {
                if *l {
                    p.clone()
                } else {
                    vec![p[0], *p.last().unwrap()]
                }
            })
            .collect();
        let near: Vec<Vec<(f64, usize, u32)>> = starts
            .par_iter()
            .enumerate()
            .map(|(si, s)| {
                let sr = self.search(s.node, NAMED_ACCESS_M, None);
                entries
                    .iter()
                    .map(|es| {
                        es.iter()
                            .filter_map(|&n| {
                                sr.reached(n).map(|m| {
                                    // A lot over the driveways a trail
                                    // leaves, where they're comparable:
                                    // Killarney's Cranberry Bog Trail from
                                    // its lot, not a campsite driveway 190 m
                                    // up the trail (routes#156).
                                    let h = if s.lot_exit { LOT_EXIT_HANDICAP_M } else { 0.0 };
                                    (m + h, si, n)
                                })
                            })
                            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
                            .unwrap_or((f64::MAX, si, u32::MAX))
                    })
                    .collect()
            })
            .collect();
        let mut out = vec![];
        for (pi, (name, path, ways, is_loop)) in pieces.iter().enumerate() {
            let Some(&(m, si, entry)) = near
                .iter()
                .map(|v| &v[pi])
                .filter(|x| x.0 < f64::MAX)
                .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
            else {
                continue;
            };
            let _ = m;
            let s = &starts[si];
            let sr = self.search_to(s.node, NAMED_ACCESS_M, None, entry);
            // The whole trail as a tour where that covers more of it than the
            // piece does: a loop with a spur (Chikanishing Trail), a loop
            // closed over a stretch of another trail (Cranberry Bog Trail, on
            // the La Cloche Silhouette), a trail that forks (#101-#103).
            let piece_m: f64 = path
                .windows(2)
                .map(|w| metres(g.pts[w[0] as usize], g.pts[w[1] as usize]))
                .sum();
            // The tour starts where the way in first reaches the trail, if it
            // walks as much of the trail from there: a trailhead on a trail's
            // stick, with the piece's line ending part way round its loop,
            // would otherwise walk the stick and half the loop to start there
            // (Algonquin's Two Rivers Trail, 3.4 km for 2.1, routes#133). From
            // part way along a line, the tour would walk only one side of it.
            let reach = {
                let (p, _) = sr.path(entry);
                p.into_iter()
                    .find(|&n| {
                        g.adj[n as usize].iter().any(|e| g.ways[e.way as usize].name == *name)
                    })
                    .unwrap_or(entry)
            };
            let from_entry = self.trail_tour(name, entry);
            let (entry_at, tour) = match (reach != entry)
                .then(|| self.trail_tour(name, reach))
                .flatten()
            {
                Some(t) if from_entry.as_ref().is_none_or(|e| t.3 >= e.3 * TOUR_REACH_KEEP) => {
                    (reach, Some(t))
                }
                _ => (entry, from_entry),
            };
            if let Some((tour, tour_w, is_loop_tour, covered)) = tour {
                // More of the trail, or as much as a loop where the piece is a
                // line (Cranberry Bog Trail, closed by 261 m of another trail).
                if covered > piece_m * TOUR_GAIN
                    || (is_loop_tour && !*is_loop && covered >= piece_m * TOUR_KEEP)
                {
                    let (mut full, mut fw) = sr.path(entry_at);
                    full.extend(tour.iter().skip(1));
                    fw.extend(tour_w.iter());
                    let (back, bw) = sr.path(entry_at);
                    full.extend(back.iter().rev().skip(1));
                    fw.extend(bw.iter().rev());
                    let far = g.pts[tour[tour.len() / 2] as usize];
                    let ev = self.evaluate(&full, &fw, false);
                    let d = Dest {
                        name: name.to_string(),
                        kind: Kind::Trail,
                        pt: far,
                        area: 0.0,
                        wikidata: String::new(),
                        osm_ids: vec![],
                        qrank: 1.0,
                        lake: None,
                        minor: false,
                    };
                    let (sc, parts) = score(&ev, &d, is_loop_tour);
                    out.push(Cand {
                        from: s.name.clone(),
                        start: g.pts[s.node as usize],
                        dest: name.to_string(),
                        dest_kind: Kind::Trail,
                        dest_pt: far,
                        dest_osm_ids: vec![],
                        from_osm_id: s.osm_id.clone(),
                        kind: if is_loop_tour { "loop" } else { "out-and-back" },
                        score: sc,
                        parts,
                        edges: edge_set(&full),
                        ev,
                    });
                    continue;
                }
            }
            let (mut full, mut fw) = sr.path(entry);
            // Along the trail from where we joined it.
            let (tp, tw): (Vec<u32>, Vec<u32>) = if *is_loop {
                // Rotate the loop to start and end at the entry.
                let k = path.iter().position(|&n| n == entry).unwrap();
                let ring = &path[..path.len() - 1];
                let rp: Vec<u32> = (0..=ring.len())
                    .map(|i| ring[(k + i) % ring.len()])
                    .collect();
                let rw: Vec<u32> = (0..ring.len())
                    .map(|i| ways[(k + i) % ring.len()])
                    .collect();
                (rp, rw)
            } else if entry == path[0] {
                (path.clone(), ways.clone())
            } else {
                (
                    path.iter().rev().copied().collect(),
                    ways.iter().rev().copied().collect(),
                )
            };
            full.extend(tp.iter().skip(1));
            fw.extend(tw.iter());
            let far = g.pts[tp[tp.len() / 2] as usize];
            let ev = if *is_loop {
                // Back to the trailhead the way we came.
                let (back, bw) = sr.path(entry);
                full.extend(back.iter().rev().skip(1));
                fw.extend(bw.iter().rev());
                self.evaluate(&full, &fw, false)
            } else {
                self.evaluate(&full, &fw, true)
            };
            let d = Dest {
                name: name.to_string(),
                kind: Kind::Trail,
                pt: far,
                area: 0.0,
                wikidata: String::new(),
                osm_ids: vec![],
                qrank: 1.0,
                lake: None,
                minor: false,
            };
            let (sc, parts) = score(&ev, &d, *is_loop);
            out.push(Cand {
                from: s.name.clone(),
                start: g.pts[s.node as usize],
                dest: name.to_string(),
                dest_kind: Kind::Trail,
                dest_pt: far,
                dest_osm_ids: vec![],
                from_osm_id: s.osm_id.clone(),
                kind: if *is_loop { "loop" } else { "out-and-back" },
                score: sc,
                parts,
                edges: edge_set(&full),
                ev,
            });
        }
        out
    }
}

impl<'a> Ctx<'a> {
    /// Destinations, each with the network nodes it can be reached from:
    /// within park_m of the park, or within TRAIL_DEST_M on a named trail
    /// that's partly in the park and named for it (a summit's own trail from
    /// a park trailhead: Castle's Table Mountain Trail to Table Mountain, 1.6
    /// km outside the park, routes#130). Not any destination on a park's
    /// trail: Pinkham Notch's trails to Mount Washington and the like took
    /// the place of the area's own known hikes.
    pub fn targets(&self, park_m: f64) -> Vec<(usize, Vec<u32>)> {
        let g = &self.area.graph;
        // Named trails with an edge in the park.
        let mut in_park: HashSet<&str> = HashSet::default();
        for (u, adj) in g.adj.iter().enumerate() {
            for e in adj {
                let w = &g.ways[e.way as usize];
                if w.class == Class::Trail
                    && !w.name.is_empty()
                    && self.in_park[u]
                    && self.in_park[e.to as usize]
                {
                    in_park.insert(w.name.as_str());
                }
            }
        }
        let on_own_park_trail = |pt: P, dest: &str| {
            self.nearest(pt, 150.0).is_some_and(|n| {
                g.adj[n as usize].iter().any(|e| {
                    let t = g.ways[e.way as usize].name.as_str();
                    in_park.contains(t) && named_for(t, dest)
                })
            })
        };
        let mut out = vec![];
        for (i, d) in self.dests.iter().enumerate() {
            if self.area.park_dist(d.pt, park_m).is_none()
                && d.lake.is_none()
                && !(self.area.park_dist(d.pt, TRAIL_DEST_M).is_some()
                    && on_own_park_trail(d.pt, &d.name))
            {
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

    /// Day hikes from src to the targets it reaches between min_m and max_m:
    /// out-and-backs, and with `loops`, loops back by another way.
    pub fn candidates(
        &self,
        src: u32,
        src_name: &str,
        targets: &[(usize, Vec<u32>)],
        min_m: f64,
        max_m: f64,
        loops: bool,
    ) -> Vec<Cand> {
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
            let pen = edge_set(&path);
            let mk = |kind, sc, parts, edges, ev| Cand {
                from: src_name.to_string(),
                start: g.pts[src as usize],
                dest: d.name.clone(),
                dest_kind: d.kind,
                dest_pt: g.pts[node as usize],
                dest_osm_ids: d.osm_ids.clone(),
                from_osm_id: String::new(), // set by the caller, which knows the start
                kind,
                score: sc,
                parts,
                edges,
                ev,
            };
            if ev.bike <= MAX_BIKE && ev.road <= MAX_ROAD {
                out.push(mk("out-and-back", sc, parts, pen.clone(), ev));
            }
            if !loops {
                continue;
            }
            // a loop: back by a different way, if there's a good one
            let b = self.search_back(node, m * 1.7, Some(&pen), &s);
            if let Some(bm) = b.reached(src) {
                let (back, bways) = b.path(src);
                let be = edge_set(&back);
                let shared = be.intersection(&pen).count() as f64 / be.len().max(1) as f64;
                if shared < 0.3 && bm <= m * 1.6 {
                    let full: Vec<u32> = path.iter().chain(back.iter().skip(1)).copied().collect();
                    let fw: Vec<u32> = ways.iter().chain(bways.iter()).copied().collect();
                    let ev2 = self.evaluate(&full, &fw, false);
                    if ev2.bike <= MAX_BIKE && ev2.road <= MAX_ROAD {
                        let (s2, p2) = score(&ev2, d, true);
                        out.push(mk("loop", s2, p2, edge_set(&full), ev2));
                    }
                }
            }
        }
        out
    }

    /// The first named trail (else road) reached walking out from a node.
    fn trail_name_near(&self, node: u32) -> Option<String> {
        self.trail_near(node).map(|(name, _)| name)
    }

    /// The first named trail (else road) reached walking out from a node,
    /// named for a start ("<trail> trailhead", "Parking off <road>"), and
    /// the walk to it in metres (infinite for a road: no trail within reach).
    fn trail_near(&self, node: u32) -> Option<(String, f64)> {
        let g = &self.area.graph;
        let s = self.search(node, 1500.0, None);
        let mut order: Vec<(f64, u32)> = s.all_reached().map(|(n, m)| (m, n)).collect();
        order.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut road = None;
        for (m, n) in order {
            for e in &g.adj[n as usize] {
                let w = &g.ways[e.way as usize];
                if w.name.is_empty() {
                    continue;
                }
                if matches!(w.class, Class::Trail | Class::Track) && !w.bike {
                    return Some((format!("{} trailhead", w.name), m));
                }
                road = road.or_else(|| Some((format!("Parking off {}", w.name), f64::INFINITY)));
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
    pub fn starts(&self, parkings: &[Place], aisles: &[P], trail_area: bool) -> Vec<Start> {
        let (lot_reach, junction_reach) = if trail_area {
            (0.0, 0.0)
        } else {
            (5000.0, 2000.0)
        };
        let urban = urban_lots(parkings);
        let lot = |p: &Place| -> Option<Lot> {
            let node = self.nearest(p.pt, 250.0)?;
            let near = self.trail_near(node);
            let walk = near.as_ref().map_or(f64::INFINITY, |n| n.1);
            // The trail it reaches first ("" for none: a road's name only).
            let trail = match &near {
                Some((label, w)) if w.is_finite() => {
                    label.strip_suffix(" trailhead").unwrap_or(label).to_string()
                }
                _ => String::new(),
            };
            let name = if p.name.is_empty() {
                near.map_or_else(|| "Parking".into(), |n| n.0)
            } else {
                p.name.clone()
            };
            Some(Lot {
                start: Start {
                    name,
                    node,
                    pt: p.pt,
                    trailhead: p.trailhead,
                    osm_id: p.osm_id.clone(),
                    known: p.trailhead || !p.name.is_empty(),
                    lot_exit: false,
                },
                walk,
                trail,
            })
        };
        let mut out: Vec<Lot> = parkings
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
            .filter_map(|(_, p)| lot(p))
            .collect();
        // Lots mapped only by their driving aisles (service=parking_aisle,
        // no amenity=parking): Killarney's Granite Ridge trailhead is a loop
        // of aisle off Highway 637 (routes#102). In or just by the park, and
        // not inside a mapped lot, which stands for itself.
        let aisle_reach = if trail_area { 0.0 } else { AISLE_REACH_M };
        out.par_extend(
            aisles
                .par_iter()
                .filter(|&&pt| self.area.park_dist(pt, aisle_reach).is_some())
                .filter(|&&pt| parkings.iter().all(|p| metres(p.pt, pt) > AISLE_IN_LOT_M))
                .filter_map(|&pt| {
                    lot(&Place {
                        name: String::new(),
                        pt,
                        trailhead: false,
                        osm_id: String::new(),
                    })
                }),
        );
        let mut merged = merge_lots(out);
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
                osm_id: String::new(),
                known: false,
                lot_exit: false,
            });
        }
        // Where a trail leaves a lodge's or visitor centre's driveways in the
        // park: somewhere a car can get to, though no lot is mapped there.
        // Mt Norquay's day lodge, 550 m from the ski area's lots, is where
        // the trail to Cascade Amphitheatre starts (routes#156). The trail
        // may meet them by a short track (the lodge's gravel tracks). Kept
        // LOT_EXIT_SEP_M from every other start, not 1 km: the lot's own
        // start already stands for a trail leaving its aisles.
        if !trail_area {
            let mut dist: HashMap<u32, f64> = HashMap::default();
            let mut pq = BinaryHeap::new();
            for (n, adj) in g.adj.iter().enumerate() {
                if adj.iter().any(|e| g.ways[e.way as usize].lot) {
                    dist.insert(n as u32, 0.0);
                    pq.push(Q::new(0.0, n as u32));
                }
            }
            while let Some(q) = pq.pop() {
                let (d, u) = (q.key(), q.1);
                if d > dist[&u] {
                    continue;
                }
                for e in &g.adj[u as usize] {
                    if !matches!(g.ways[e.way as usize].class, Class::Road | Class::Track) {
                        continue;
                    }
                    let nd = d + e.m as f64;
                    if nd <= LOT_EXIT_M && nd < *dist.get(&e.to).unwrap_or(&f64::MAX) {
                        dist.insert(e.to, nd);
                        pq.push(Q::new(nd, e.to));
                    }
                }
            }
            let mut exits: Vec<(f64, u32)> = dist
                .into_iter()
                .filter(|&(n, _)| {
                    g.adj[n as usize].iter().any(is_trail)
                        && self.area.park_dist(g.pts[n as usize], 0.0) == Some(0.0)
                })
                .map(|(n, d)| (d, n))
                .collect();
            exits.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for (_, n) in exits {
                let pt = g.pts[n as usize];
                if merged.iter().any(|m| metres(m.pt, pt) < LOT_EXIT_SEP_M) {
                    continue;
                }
                let name = match self.trail_name_near(n) {
                    Some(t) if !t.starts_with("Parking off ") => t,
                    _ => "Trail access".into(),
                };
                merged.push(Start {
                    name,
                    node: n,
                    pt,
                    trailhead: false,
                    osm_id: String::new(),
                    known: false,
                    lot_exit: true,
                });
            }
        }
        merged
    }
}

/// A trail leaving driveways or a lot's aisles, or a road or track this short
/// from them, is a start where no other start is LOT_EXIT_SEP_M away
/// (routes#156): the Cascade Amphitheatre path leaves Mt Norquay's lodge
/// tracks ~225 m from its driveways, 220 m from where Mount Norquay Route
/// leaves them.
const LOT_EXIT_M: f64 = 300.0;
const LOT_EXIT_SEP_M: f64 = 150.0;
const LOT_EXIT_HANDICAP_M: f64 = 200.0;

/// How far outside the park a lot mapped only by its aisles may be.
const AISLE_REACH_M: f64 = 500.0;
/// An aisle this close to a mapped lot's point is in that lot, which stands
/// for itself. Not much more: the Granite Ridge trailhead's aisle loop is
/// 140 m from the park office's lot (routes#102).
const AISLE_IN_LOT_M: f64 = 60.0;
/// Lots closer than this are one start.
const LOT_MERGE_M: f64 = 400.0;

/// A candidate start: a lot, its walk to the nearest named trail, and that
/// trail ("" if none is within reach).
struct Lot {
    start: Start,
    walk: f64,
    trail: String,
}

/// Lots merged into starts: of lots within LOT_MERGE_M of each other that
/// reach the same trail first (or one reaches none), one is kept. Mapped
/// trailheads first, then lots named for something (in OSM, or here for the
/// trail or road nearby) over plain "Parking", then the shortest walk to a
/// trail: of Killarney's George Lake campground lot and the Granite Ridge
/// trailhead's, 230 m apart, the trailhead's, from which the trail starts,
/// not the campground's, 350 m of campground road from it (routes#102).
/// Lots that reach different trails are different trailheads, however
/// close: the George Lake lot, at the La Cloche Silhouette Trail, 290 m from
/// the Granite Ridge trailhead (routes#123).
fn merge_lots(mut lots: Vec<Lot>) -> Vec<Start> {
    lots.sort_by(|a, b| {
        (!a.start.trailhead, a.start.name == "Parking")
            .cmp(&(!b.start.trailhead, b.start.name == "Parking"))
            .then(a.walk.total_cmp(&b.walk))
    });
    let mut kept: Vec<Lot> = vec![];
    for l in lots {
        let same = |k: &Lot| k.trail == l.trail || k.trail.is_empty() || l.trail.is_empty();
        if kept
            .iter()
            .all(|k| metres(k.start.pt, l.start.pt) >= LOT_MERGE_M || !same(k))
        {
            kept.push(l);
        }
    }
    kept.into_iter().map(|l| l.start).collect()
}

/// Coverage first (each start's best), then greedily by score x novelty.
/// Whether a hike is its start's own named trail: walked for itself (dest
/// kind Trail) from a start named for it ("Granite Ridge Trail trailhead",
/// "Cranberry Bog Trail").
pub fn own_trail(c: &Cand) -> bool {
    c.dest_kind == Kind::Trail && named_for(&c.from, &c.dest)
}

/// Whether a start's name is a trail's, or the trail's and more ("Granite
/// Ridge Trail trailhead" for the Granite Ridge Trail), ignoring case.
fn named_for(start: &str, trail: &str) -> bool {
    let (f, d) = (start.to_lowercase(), trail.to_lowercase());
    !d.is_empty() && (f == d || f.strip_prefix(&d).is_some_and(|rest| rest.starts_with(' ')))
}

/// Room for starts' own trails beyond the day-hike budget, as a share of it
/// (see select).
const OWN_EXTRA: f64 = 1.0;

/// Score added to a multi-day loop from a trailhead named for the trail it
/// mostly walks (see the loops in curate): a tie-break between trailheads on
/// the same loop, small against the road penalty (2.5 x road share).
const W_OWN_LOOP: f64 = 0.25;

/// The named trail a route walks the most of, by length (ways[i] is the way
/// from path[i] to path[i + 1]).
fn main_trail<'g>(g: &'g crate::area::Graph, path: &[u32], ways: &[u32]) -> Option<&'g str> {
    let mut by: HashMap<&str, f64> = HashMap::default();
    for (w, &way) in path.windows(2).zip(ways) {
        let way = &g.ways[way as usize];
        if way.class == Class::Trail && !way.name.is_empty() {
            *by.entry(way.name.as_str()).or_default() +=
                metres(g.pts[w[0] as usize], g.pts[w[1] as usize]);
        }
    }
    by.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(n, _)| n)
}

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
        // And a start's own trail, where it walks new ground: "Granite Ridge
        // Trail trailhead" offers Granite Ridge Trail besides its best hike,
        // the 22 km to Threenarrows Lake (#102). Not instead of it: routes
        // names many starts for the trail they meet ("Paget North Ridge
        // Route trailhead", for Paget Peak), and a trail the best hike walks
        // adds nothing new. Up to OWN_EXTRA x n more than n: in a big park
        // each start's best hike fills n, and the park's own short trails
        // got no room (Algonquin's Highway 60 trails, routes#133).
        let own_n = n + (n as f64 * OWN_EXTRA).round() as usize;
        let mut own: Vec<&Cand> = cands.iter().filter(|c| own_trail(c)).collect();
        own.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        for c in own {
            if chosen.len() < own_n
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
    pub camp_osm_id: String,
    pub from: String,
    pub from_osm_id: String,
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
    /// The named trail it walks end to end (named_throughs): its title,
    /// unless an OSM hiking route names it (recall::name_routes).
    pub name: Option<String>,
}

pub struct Curated {
    pub day: Vec<Cand>,
    /// Short walks to a sight from where a car can get to (#70).
    pub stops: Vec<Cand>,
    pub bases: Vec<BaseCamp>,
    pub through: Vec<Through>,
    pub stats: String,
    /// Whether the deadline passed, so some trailheads or campsites were
    /// skipped: the routes are what was found by then.
    pub timed_out: bool,
    /// Every day-hike candidate, each marked `picked` or not: only with
    /// --candidates (CANDIDATES), else empty.
    pub candidates: Vec<Value>,
}

const MULTI_STARTS_PER_CAMP: usize = 6;

/// How far a stop may be from where the car is (#70).
const STOP_MAX_M: f64 = 1000.0;
/// Under this, a stop's walk may be along the road (a pull-out viewpoint).
const STOP_ROADSIDE_M: f64 = 200.0;
/// Sights this close together are one stop.
const STOP_SAME_M: f64 = 200.0;
/// At most this many stops a park: they're quick to take in, so a park can
/// list plenty (#72).
const STOP_MAX_N: usize = 12;

/// A stop's name without notes in brackets, for telling two names for one
/// sight apart: "Keys View (accessible)" is Keys View.
fn stop_name(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for ch in name.chars() {
        match ch {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_lowercase()
}

/// A base camp's hike with this share of its edges on the walk in goes back
/// the way you came (#61).
const BASE_BACKTRACK_MAX: f64 = 0.5;

/// Stops (#70): a viewpoint, peak or waterfall a short walk (STOP_MAX_M)
/// from where a car can get to, which a day hike can't be, as those start
/// 1.5 km or more away. Terra Nova's Ochre Hill Fire Tower is 100 m from the
/// turning circle at the end of its road; Banff's Peyto Lake View, 600 m
/// from the Bow Summit lot. Cars get to the trailheads and parking lots
/// (starts) and the ends of roads open to them (a turning circle often has
/// no lot mapped). One stop per sight, from the nearest of those; the most
/// appealing and best known first (ties to the better stop overall), up to
/// STOP_MAX_N.
fn stops(ctx: &Ctx, starts: &[Start], targets: &[(usize, Vec<u32>)]) -> Vec<Cand> {
    let g = &ctx.area.graph;
    let sights: Vec<(usize, Vec<u32>)> = targets
        .iter()
        .filter(|(di, _)| {
            let d = &ctx.dests[*di];
            matches!(d.kind, Kind::Viewpoint | Kind::Peak | Kind::Waterfall) && !d.minor
        })
        .cloned()
        .collect();
    if sights.is_empty() {
        return vec![];
    }
    // Where cars get to: the starts, and the dead ends of roads near the
    // park, named for the road.
    let mut cars: Vec<(u32, String)> = starts.iter().map(|s| (s.node, s.name.clone())).collect();
    for (n, adj) in g.adj.iter().enumerate() {
        let n = n as u32;
        // A road's end: one road edge, whatever paths also meet it there
        // (Ochre Hill's turning circle is where its lookout loop starts).
        let mut roads = adj.iter().filter(|e| g.ways[e.way as usize].class == Class::Road);
        let (Some(e), None) = (roads.next(), roads.next()) else {
            continue;
        };
        if ctx.area.park_dist(g.pts[n as usize], 1000.0).is_none() {
            continue;
        }
        let road = &g.ways[e.way as usize].name;
        cars.push((n, if road.is_empty() { "Road's end".into() } else { format!("End of {road}") }));
    }
    let cands: Vec<Cand> = cars
        .par_iter()
        .flat_map(|(n, name)| ctx.candidates(*n, name, &sights, 0.0, STOP_MAX_M, false))
        .filter(|c| c.kind == "out-and-back" && c.ev.bike <= MAX_BIKE)
        .filter(|c| c.ev.one_way < STOP_ROADSIDE_M || c.ev.road <= MAX_ROAD)
        .collect();
    // Per sight, the shortest walk; then by appeal and how well known.
    let mut best: HashMap<&str, &Cand> = HashMap::default();
    for c in &cands {
        if best.get(c.dest.as_str()).is_none_or(|b| c.ev.one_way < b.ev.one_way) {
            best.insert(&c.dest, c);
        }
    }
    let worth = |c: &Cand| {
        c.parts.iter().filter(|(k, _)| matches!(*k, "appeal" | "popularity")).map(|(_, v)| v).sum::<f64>()
    };
    let mut v: Vec<&Cand> = best.into_values().collect();
    // Ties (most viewpoints have the same appeal and no popularity) go to the
    // better stop overall: open views, lakes in sight, little road walking.
    // By name, Banff's Pedestrian Bridge beat Peyto Lake (#72).
    v.sort_by(|a, b| {
        worth(b)
            .partial_cmp(&worth(a))
            .unwrap()
            .then_with(|| b.score.partial_cmp(&a.score).unwrap())
            .then_with(|| a.dest.cmp(&b.dest))
    });
    // A sight beside one already chosen is the same stop (Ochre Hill's fire
    // tower and the lookout platform 30 m from it).
    let mut out: Vec<Cand> = vec![];
    for c in v {
        if out.len() >= STOP_MAX_N {
            break;
        }
        // ...and so is one with the same name but for a note in brackets
        // (Joshua Tree's "Keys View (accessible)", 550 m from Keys View).
        if out.iter().all(|o| metres(o.dest_pt, c.dest_pt) > STOP_SAME_M && stop_name(&o.dest) != stop_name(&c.dest)) {
            out.push(c.clone());
        }
    }
    out
}

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
                osm_ids: d.osm_ids.clone(),
                qrank: d.qrank,
                lake: d.lake,
                minor: d.minor,
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
                osm_id: c.osm_id.clone(),
            });
        }
    }
    let mut ctx = Ctx::new(area, dem, dests, camps);
    ctx.lake_access = area.lake_access().into_iter().collect();
    let targets = ctx.targets(1000.0);
    let starts = ctx.starts(&area.parkings, &area.aisles, trail_area);
    let t = std::time::Instant::now();
    let mut stats = format!(
        "{} nodes, {} destinations ({} reachable), {} trailheads, {} backcountry campsites",
        area.graph.pts.len(),
        ctx.dests.len(),
        targets.len(),
        starts.len(),
        ctx.camps.len()
    );
    // day hikes
    let cands: Vec<Cand> = starts
        .par_iter()
        .flat_map(|s| {
            if late() {
                return vec![];
            }
            let mut cs = ctx.candidates(s.node, &s.name, &targets, 1500.0, 15000.0, true);
            for c in &mut cs {
                c.from_osm_id = s.osm_id.clone();
            }
            cs
        })
        .collect();
    let mut cands = cands;
    if !late() {
        // A named trail that a destination hike from the same trailhead
        // already walks (Wapta Falls Trail to Wapta Falls) is the same hike:
        // keep the one named for where it goes.
        let named: Vec<Cand> = ctx
            .named_trails(&starts)
            .into_iter()
            .filter(|n| {
                // The same hike both ways: each walks most of the other. A
                // loop that only shares a stretch with a destination hike
                // stays (#103): Cranberry Bog Trail, though the hike to
                // Freeland Lake from its trailhead walks much of it.
                // And a loop that's its start's own trail stays whatever
                // walks it: "Cranberry Bog Trail trailhead" is there for it.
                (n.kind == "loop" && own_trail(n))
                    || !cands.iter().any(|c| {
                    let shared = n.edges.intersection(&c.edges).count() as f64;
                    c.from == n.from
                        && shared >= 0.7 * n.edges.len().max(1) as f64
                        && shared >= 0.7 * c.edges.len().max(1) as f64
                })
            })
            .collect();
        cands.extend(named);
    }
    let day = select(&cands, n_day, true);
    let candidates: Vec<Value> = if CANDIDATES.load(Relaxed) {
        cands
            .iter()
            .map(|c| {
                let mut j = cand_json(c);
                j["picked"] = json!(day.iter().any(|d| d.from == c.from && d.edges == c.edges));
                j
            })
            .collect()
    } else {
        vec![]
    };
    stats += &format!("; day {:.1}s", t.elapsed().as_secs_f64());

    let stops = if late() { vec![] } else { stops(&ctx, &starts, &targets) };

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
            if approach.bike > MAX_BIKE {
                return None;
            }
            // A hike that goes back along the walk in is one from the
            // trailhead, not from camp: Gros Morne's Snug Harbour offered
            // Jerrys Pond, 4.7 km back towards the trailhead it's 1 km
            // from (#61).
            let walk_in = edge_set(&path);
            let hikes: Vec<Cand> = ctx
                .candidates(cnode, &c.name, &targets, 1000.0, 8000.0, true)
                .into_iter()
                .map(|mut h| {
                    h.from_osm_id = c.osm_id.clone();
                    h
                })
                .filter(|h| {
                    let back = h.edges.intersection(&walk_in).count();
                    (back as f64) < BASE_BACKTRACK_MAX * h.edges.len().max(1) as f64
                })
                .collect();
            let picks = select(&hikes, 4, false);
            // One good hike from camp is still a trip (an overnight to
            // Gros Morne's North Rim via Snug Harbour, too far for a day
            // hike): base camps with more rank higher (score, below).
            if picks.is_empty() {
                return None;
            }
            let score = picks.iter().map(|h| h.score).sum::<f64>() / picks.len() as f64
                + 0.3 * picks.len() as f64;
            Some(BaseCamp {
                camp: c.name.clone(),
                camp_pt: c.pt,
                camp_osm_id: c.osm_id.clone(),
                from: start_nodes[&snode].name.clone(),
                from_osm_id: start_nodes[&snode].osm_id.clone(),
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
            let s = ctx.search_trails(a.node, 100000.0);
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
                if ev.bike > MAX_BIKE {
                    continue;
                }
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
                    name: None,
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
                if ev.bike > MAX_BIKE {
                    continue;
                }
                let kinds: HashSet<Kind> = ev.passed.iter().map(|(k, _)| *k).collect();
                // From a trailhead named for the trail the loop mostly walks:
                // the same loop from another trailhead it passes scores
                // within a few hundredths (Killarney's La Cloche Silhouette
                // from the Cranberry Bog lot, 2.67, and from George Lake,
                // where the park puts its trailhead, 2.66; routes#123).
                let own = main_trail(g, &full, &fw).is_some_and(|t| named_for(&a.name, t));
                let sc =
                    1.0 * kinds.len() as f64 + 2.5 * ev.open_high + 0.7 * ev.lc_variety.min(4.0)
                        - 2.5 * ev.road
                        - 2.0 * ev.bike
                        + if own { W_OWN_LOOP } else { 0.0 };
                out.push(Through {
                    from: a.name.clone(),
                    to: a.name.clone(),
                    kind: "loop",
                    score: sc,
                    ev,
                    edges: edge_set(&full),
                    name: None,
                });
            }
            out
        })
        .collect();
    if ctx.camps.len() >= 2 && !late() {
        thru.extend(named_throughs(&ctx, &starts));
    }
    // A point-to-point that ends at a start named here (an unnamed lot, or
    // where a trail meets a road), after passing a known trailhead well
    // before it, runs past the trip's real end (#63): Pacific Rim's West
    // Coast Trail went on from the trail's end at Gordon River along
    // Deering and Pacific Marine Roads to an unnamed lot by Fairy Lake, the
    // "F-0100 trailhead", 81.4 km to its 74.9. Such trips go; the one ending
    // at the known trailhead is a candidate of its own. A trail whose only
    // access is a road (no known trailhead passed) keeps its road end.
    let known: Vec<P> = starts.iter().filter(|s| s.known).map(|s| s.pt).collect();
    let made_up: HashSet<&str> = starts
        .iter()
        .filter(|s| !s.known)
        .map(|s| s.name.as_str())
        .filter(|n| !starts.iter().any(|s| s.known && s.name == *n))
        .collect();
    thru.retain(|t| {
        if t.kind != "point-to-point" {
            return true;
        }
        let line = &t.ev.line;
        let rev: Vec<P> = line.iter().rev().copied().collect();
        !((made_up.contains(t.to.as_str()) && runs_past_known(line, &known))
            || (made_up.contains(t.from.as_str()) && runs_past_known(&rev, &known)))
    });
    thru.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    // Multi-day trips: 3, or more in big parks, by the day-hike budget (a
    // third of it, as for base camps): Jasper (15 day hikes) gets 5, and
    // had room for only three of its well-known trips (#54).
    let n_multi = (n_day / 3).clamp(3, 5);
    let mut through: Vec<Through> = vec![];
    let mut seen: HashSet<(String, String)> = HashSet::default();
    for t in thru {
        let key = if t.from < t.to {
            (t.from.clone(), t.to.clone())
        } else {
            (t.to.clone(), t.from.clone())
        };
        // The same trip as one already chosen: mostly the same trail, or
        // mostly the same campsites (Jasper's Skyline Trail, from a dozen
        // trailheads near town and to two at Maligne Lake, took two of the
        // three places, and one more went to the Skyline extended to
        // Poboktan Creek).
        let camps =
            |x: &Through| -> HashSet<String> { x.ev.camps.iter().map(|c| c.0.clone()).collect() };
        let tc = camps(&t);
        if seen.contains(&key)
            || through.iter().any(|u| {
                let uc = camps(u);
                let shared = tc.intersection(&uc).count() as f64;
                t.edges.intersection(&u.edges).count() as f64 / t.edges.len() as f64 > 0.7
                    || (!tc.is_empty()
                        && !uc.is_empty()
                        && shared / tc.len().min(uc.len()) as f64 >= 0.5)
            })
            // Or mostly campsites the chosen trips already pass, between
            // them: with more places (n_multi), Jasper's fifth was a 79 km
            // loop from town through the Skyline's and Signal loop's camps.
            || (!tc.is_empty()
                && tc.iter().filter(|c| through.iter().any(|u| camps(u).contains(*c))).count()
                    as f64
                    / tc.len() as f64
                    >= 0.5)
        {
            continue;
        }
        seen.insert(key);
        through.push(t);
        if through.len() >= n_multi {
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
        stops,
        bases,
        through,
        stats,
        timed_out,
        candidates,
    }
}

/// The graph's edges along an area's OSM hiking routes: those whose
/// midpoint is within ON_KNOWN_M of one.
fn on_known_edges(area: &Area) -> HashSet<u64> {
    let g = &area.graph;
    let cell = ON_KNOWN_M / mpu(g.pts.first().map(|p| p.1).unwrap_or(0.0));
    let key = |p: P| ((p.0 / cell).floor() as i64, (p.1 / cell).floor() as i64);
    let mut grid: HashMap<(i64, i64), Vec<P>> = HashMap::default();
    for k in area.known.iter().filter(|k| !crate::recall::is_bike(k)) {
        for &(a, b) in &k.segs {
            let n = (metres(a, b) / (ON_KNOWN_M / 3.0)).ceil().max(1.0) as usize;
            for i in 0..=n {
                let t = i as f64 / n as f64;
                let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                grid.entry(key(p)).or_default().push(p);
            }
        }
    }
    let near = |p: P| {
        let (cx, cy) = key(p);
        (-1..=1).any(|dx| {
            (-1..=1).any(|dy| {
                grid.get(&(cx + dx, cy + dy))
                    .is_some_and(|v| v.iter().any(|&q| metres(p, q) <= ON_KNOWN_M))
            })
        })
    };
    (0..g.adj.len() as u32)
        .into_par_iter()
        .flat_map_iter(|u| {
            g.adj[u as usize]
                .iter()
                .filter(move |e| u < e.to)
                .filter(|e| {
                    let (a, b) = (g.pts[u as usize], g.pts[e.to as usize]);
                    near(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0))
                })
                .map(move |e| ekey(u, e.to))
                .collect::<Vec<_>>()
        })
        .collect()
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
        "max_sac": ev.max_sac,
        "line": line_json(&ev.line),
    })
}

pub fn cand_json(c: &Cand) -> Value {
    let mut j = json!({
        "from": c.from,
        "start": geo::round_ll(c.start),
        "dest": c.dest,
        "dest_kind": c.dest_kind.name(),
        "dest_at": geo::round_ll(c.dest_pt),
        "kind": c.kind,
        "score": (c.score * 100.0).round() / 100.0,
        "why": c.parts.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "route": eval_json(&c.ev),
    });
    // OSM ids, for joining routes to other data about the same places (e.g.
    // hikeratlas/cc-crawler's trip reports). Left out when unknown.
    if !c.dest_osm_ids.is_empty() {
        j["dest_osm_ids"] = json!(c.dest_osm_ids);
    }
    if !c.from_osm_id.is_empty() {
        j["from_osm_id"] = json!(c.from_osm_id);
    }
    j
}

impl Curated {
    pub fn to_json(&self) -> Value {
        json!({
            "day_hikes": self.day.iter().map(cand_json).collect::<Vec<_>>(),
            "stops": self.stops.iter().map(cand_json).collect::<Vec<_>>(),
            "base_camps": self.bases.iter().map(|b| {
                let mut j = json!({
                    "camp": b.camp,
                    "at": geo::round_ll(b.camp_pt),
                    "from": b.from,
                    "score": (b.score * 100.0).round() / 100.0,
                    "approach": eval_json(&b.approach),
                    "hikes": b.hikes.iter().map(cand_json).collect::<Vec<_>>(),
                });
                if !b.camp_osm_id.is_empty() {
                    j["camp_osm_id"] = json!(b.camp_osm_id);
                }
                if !b.from_osm_id.is_empty() {
                    j["from_osm_id"] = json!(b.from_osm_id);
                }
                j
            }).collect::<Vec<_>>(),
            "point_to_points": self.through.iter().map(|t| {
                let mut j = json!({
                    "from": t.from,
                    "to": t.to,
                    "kind": t.kind,
                    "score": (t.score * 100.0).round() / 100.0,
                    "route": eval_json(&t.ev),
                });
                if let Some(n) = &t.name {
                    j["name"] = json!(n);
                }
                j
            }).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    // A hike is its start's own trail when it walks a named trail (dest kind
    // Trail) from a start named for it (#102, #103).
    #[test]
    fn a_start_named_for_a_trail_owns_it() {
        use super::*;
        let ev = Eval {
            one_way: 0.0, total: 0.0, gain: 0.0, loss: 0.0, max_ele: 0.0, min_ele: 0.0,
            lc_shares: vec![], lc_variety: 0.0, open_high: 0.0, passed: vec![], camps: vec![],
            road: 0.0, bike: 0.0, known: 0.0, via: vec![], max_sac: 0, line: vec![],
        };
        let c = |from: &str, dest: &str, k: Kind| Cand {
            from: from.into(), start: (0.0, 0.0), dest: dest.into(), dest_kind: k,
            dest_pt: (0.0, 0.0), dest_osm_ids: vec![], from_osm_id: String::new(),
            kind: "loop", score: 0.0, parts: vec![],
            edges: HashSet::default(), ev: ev.clone(),
        };
        assert!(own_trail(&c("Granite Ridge Trail trailhead", "Granite Ridge Trail", Kind::Trail)));
        assert!(own_trail(&c("Skyline Trail", "Skyline Trail", Kind::Trail)));
        assert!(!own_trail(&c("Granite Ridge Trail trailhead", "Threenarrows Lake", Kind::Lake)));
        assert!(!own_trail(&c("Granite Ridge Trailhead", "Granite Ridge Trail", Kind::Trail)));
        assert!(!own_trail(&c("Cranberry Bog Trail trailhead", "Granite Ridge Trail", Kind::Trail)));
    }

    // A named trail is worth at least the best sight it passes (#109): Cape
    // Split's Minas Basin Trail passes lookoffs on the way to the tip.
    #[test]
    fn a_named_trail_counts_the_sights_it_passes() {
        use super::*;
        let ev = |passed: Vec<(Kind, Vec<String>)>| Eval {
            one_way: 6000.0, total: 12000.0, gain: 100.0, loss: 100.0,
            max_ele: 100.0, min_ele: 0.0, lc_shares: vec![], lc_variety: 1.0,
            open_high: 0.0, passed, camps: vec![], road: 0.0, bike: 0.0, known: 0.0,
            via: vec![], max_sac: 0, line: vec![],
        };
        let trail = Dest {
            name: "Minas Basin Trail".into(), kind: Kind::Trail, pt: (0.0, 0.0), area: 0.0,
            wikidata: String::new(), osm_ids: vec![], qrank: 1.0, lake: None, minor: false,
        };
        let appeal = |e: &Eval| score(e, &trail, false).1.iter().find(|p| p.0 == "appeal").unwrap().1;
        assert_eq!(appeal(&ev(vec![])), (W_APPEAL * crate::area::TRAIL_APPEAL * 100.0).round() / 100.0);
        let past = ev(vec![(Kind::Viewpoint, vec!["Big Cove Look Off".into()]), (Kind::Lake, vec!["X".into()])]);
        assert_eq!(appeal(&past), (W_APPEAL * Kind::Viewpoint.appeal() * 100.0).round() / 100.0);
    }

    #[test]
    fn stop_names() {
        assert_eq!(stop_name("Keys View (accessible)"), stop_name("Keys View"));
        assert_eq!(stop_name("Peyto Lake - Viewpoint"), "peyto lake - viewpoint");
        assert_ne!(stop_name("Lower Falls"), stop_name("Upper Falls"));
    }

    use super::*;

    // A line along y, 100 m a step (see geo::metres), as a route's points.
    fn line_m(n: usize) -> Vec<P> {
        let step = 100.0 / metres((0.0, 0.0), (0.0, 1.0));
        (0..n).map(|i| (0.0, i as f64 * step)).collect()
    }

    // #63: past a known trailhead 6.5 km before a made-up end.
    #[test]
    fn a_trip_running_past_a_known_trailhead() {
        let line = line_m(700); // 70 km
        let end_of_trail = line[635];
        assert!(runs_past_known(&line, &[end_of_trail]));
    }

    // A known trailhead next to the made-up end, or none at all (a trail
    // whose only access is a road), keeps the end.
    #[test]
    fn a_road_end_with_no_known_trailhead_before_it() {
        let line = line_m(700);
        assert!(!runs_past_known(&line, &[line[695]]));
        assert!(!runs_past_known(&line, &[]));
        // A trailhead off to the side, 1 km from the route, isn't passed.
        let aside = (line[600].0 + (line[10].1 - line[0].1), line[600].1);
        assert!(!runs_past_known(&line, &[aside]));
    }

    // The trip's own start (the other end) doesn't count.
    #[test]
    fn its_own_start_does_not_count() {
        let line = line_m(700);
        assert!(!runs_past_known(&line, &[line[0], line[1]]));
    }

    fn lot(m_east: f64, m_north: f64) -> Place {
        // Around 45°N: place lots by metres, converted to world units there.
        let y0 = 0.36 * geo::EXT * 16384.0; // a world y at roughly mid-latitudes
        let u = 1.0 / mpu(y0);
        Place {
            name: String::new(),
            pt: (1000.0 + m_east * u, y0 + m_north * u),
            trailhead: false,
            osm_id: String::new(),
        }
    }

    #[test]
    fn of_lots_near_each_other_the_one_nearest_its_trail_is_kept() {
        let lot_ = |name: &str, m_north: f64, trailhead: bool, walk: f64, trail: &str| Lot {
            start: Start {
                name: name.into(),
                node: 0,
                pt: lot(0.0, m_north).pt,
                trailhead,
                osm_id: String::new(),
                known: trailhead,
                lot_exit: false,
            },
            walk,
            trail: trail.into(),
        };
        let names = |v: Vec<Start>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        // Killarney (routes#102): the campground's lot, 350 m of road from
        // the Granite Ridge Trail, and the trailhead's aisle loop 230 m
        // away, which the trail starts from. Either order in.
        let gr = "Granite Ridge Trail";
        let camp = || lot_("Granite Ridge Trail trailhead", 230.0, false, 350.0, gr);
        let loop_ = || lot_("Granite Ridge Trail trailhead", 0.0, false, 20.0, gr);
        let at = |v: Vec<Start>| v.into_iter().map(|s| s.pt).collect::<Vec<_>>();
        assert_eq!(at(merge_lots(vec![camp(), loop_()])), [loop_().start.pt]);
        assert_eq!(at(merge_lots(vec![loop_(), camp()])), [loop_().start.pt]);
        // A mapped trailhead still comes first, however far its walk; and a
        // lot named for a trail over plain "Parking".
        let th = lot_("Lot 9", 230.0, true, 900.0, gr);
        assert_eq!(names(merge_lots(vec![loop_(), th])), ["Lot 9"]);
        let plain = lot_("Parking", 0.0, false, f64::INFINITY, "");
        let named = lot_("Parking off Highway 637", 100.0, false, f64::INFINITY, "");
        assert_eq!(names(merge_lots(vec![plain, named])), ["Parking off Highway 637"]);
        // 400 m or more apart: both kept.
        let far = lot_("Parking", 500.0, false, f64::INFINITY, "");
        assert_eq!(merge_lots(vec![loop_(), far]).len(), 2);
        // Near, but at different trails: different trailheads (routes#123,
        // the George Lake lot at the La Cloche Silhouette Trail, 290 m from
        // the Granite Ridge trailhead).
        let lc = lot_("La Cloche Silhouette Trail trailhead", 290.0, false, 270.0, "La Cloche Silhouette Trail");
        assert_eq!(merge_lots(vec![loop_(), lc]).len(), 2);
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
