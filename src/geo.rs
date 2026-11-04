// Geometry in "world" coordinates: Web Mercator, in z14 tile units of
// 1/4096 of a tile (the vector tiles' own resolution), so x and y run 0..2^26.
pub const Z: u8 = 14;
pub const EXT: f64 = 4096.0;
pub const WORLD: f64 = (1u64 << 26) as f64;
const CIRCUMFERENCE: f64 = 2.0 * std::f64::consts::PI * 6_371_008.8;

pub type P = (f64, f64);

pub fn lonlat(p: P) -> (f64, f64) {
    let lon = p.0 / WORLD * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * p.1 / WORLD))
        .sinh()
        .atan()
        .to_degrees();
    (lon, lat)
}

/// Metres per world unit at world y.
pub fn mpu(y: f64) -> f64 {
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / WORLD))
        .sinh()
        .atan();
    CIRCUMFERENCE * lat.cos() / WORLD
}

pub fn metres(a: P, b: P) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1) * mpu((a.1 + b.1) / 2.0)
}

pub fn tile_of(p: P) -> (u32, u32) {
    ((p.0 / EXT).floor() as u32, (p.1 / EXT).floor() as u32)
}

pub fn round_ll(p: P) -> [f64; 2] {
    let (lon, lat) = lonlat(p);
    [(lon * 1e5).round() / 1e5, (lat * 1e5).round() / 1e5]
}

fn seg_dist2(p: P, a: P, b: P) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let l2 = dx * dx + dy * dy;
    let t = if l2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0)
    };
    let (x, y) = (a.0 + t * dx - p.0, a.1 + t * dy - p.1);
    x * x + y * y
}

/// A polygon cut from one tile: its rings, in world coordinates. Tiles clip
/// polygons a little outside their edge, so the ring edges along that cut
/// aren't real boundary; `tile` lets distance checks skip them.
pub struct Poly {
    pub rings: Vec<Vec<P>>,
    pub min: P,
    pub max: P,
    tile: (f64, f64, f64, f64),
}

impl Poly {
    pub fn new(rings: Vec<Vec<P>>, tx: u32, ty: u32) -> Poly {
        let mut min = (f64::MAX, f64::MAX);
        let mut max = (f64::MIN, f64::MIN);
        for r in &rings {
            for p in r {
                min = (min.0.min(p.0), min.1.min(p.1));
                max = (max.0.max(p.0), max.1.max(p.1));
            }
        }
        let (x0, y0) = (tx as f64 * EXT, ty as f64 * EXT);
        Poly {
            rings,
            min,
            max,
            tile: (x0, y0, x0 + EXT, y0 + EXT),
        }
    }

    pub fn contains(&self, p: P) -> bool {
        if p.0 < self.min.0 || p.0 > self.max.0 || p.1 < self.min.1 || p.1 > self.max.1 {
            return false;
        }
        let mut inside = false;
        for r in &self.rings {
            for i in 0..r.len().saturating_sub(1) {
                let (a, b) = (r[i], r[i + 1]);
                if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0
                {
                    inside = !inside;
                }
            }
        }
        inside
    }

    fn is_cut(&self, a: P, b: P) -> bool {
        let (x0, y0, x1, y1) = self.tile;
        (a.0 <= x0 && b.0 <= x0)
            || (a.0 >= x1 && b.0 >= x1)
            || (a.1 <= y0 && b.1 <= y0)
            || (a.1 >= y1 && b.1 >= y1)
    }

    /// Distance (world units) from p to the polygon's real boundary, ignoring
    /// edges along the tile's cut; f64::MAX if there's none within `within`.
    pub fn boundary_dist(&self, p: P, within: f64) -> f64 {
        if p.0 < self.min.0 - within
            || p.0 > self.max.0 + within
            || p.1 < self.min.1 - within
            || p.1 > self.max.1 + within
        {
            return f64::MAX;
        }
        let mut best = f64::MAX;
        for r in &self.rings {
            for w in r.windows(2) {
                if self.is_cut(w[0], w[1]) {
                    continue;
                }
                best = best.min(seg_dist2(p, w[0], w[1]));
            }
        }
        best.sqrt()
    }
}

/// Rings from a vector tile polygon, in world coordinates.
pub fn rings(geom: Vec<Vec<(i32, i32)>>, tx: u32, ty: u32, extent: u32) -> Vec<Vec<P>> {
    let s = EXT / extent as f64;
    geom.into_iter()
        .filter(|r| r.len() >= 4)
        .map(|r| {
            r.into_iter()
                .map(|(x, y)| {
                    (
                        tx as f64 * EXT + x as f64 * s,
                        ty as f64 * EXT + y as f64 * s,
                    )
                })
                .collect()
        })
        .collect()
}

/// Points every `step` metres along a line, plus its ends.
pub fn sample(line: &[P], step: f64) -> Vec<P> {
    let mut out = vec![line[0]];
    let (mut acc, mut next) = (0.0, step);
    for w in line.windows(2) {
        let d = metres(w[0], w[1]);
        while d > 0.0 && next <= acc + d {
            let t = (next - acc) / d;
            out.push((
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            ));
            next += step;
        }
        acc += d;
    }
    out.push(*line.last().unwrap());
    out
}

pub fn length(line: &[P]) -> f64 {
    line.windows(2).map(|w| metres(w[0], w[1])).sum()
}

/// Distance along the line (metres) of the point on it nearest p.
pub fn project(line: &[P], p: P) -> f64 {
    let (mut best, mut at, mut acc) = (f64::MAX, 0.0, 0.0);
    for w in line.windows(2) {
        let d2 = seg_dist2(p, w[0], w[1]);
        if d2 < best {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let l2 = dx * dx + dy * dy;
            let t = if l2 == 0.0 {
                0.0
            } else {
                (((p.0 - w[0].0) * dx + (p.1 - w[0].1) * dy) / l2).clamp(0.0, 1.0)
            };
            best = d2;
            at = acc + t * metres(w[0], w[1]);
        }
        acc += metres(w[0], w[1]);
    }
    at
}

/// Douglas-Peucker, tolerance in world units.
pub fn simplify(line: &[P], tol: f64) -> Vec<P> {
    if line.len() < 3 {
        return line.to_vec();
    }
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    let mut stack = vec![(0, line.len() - 1)];
    let t2 = tol * tol;
    while let Some((a, b)) = stack.pop() {
        let (mut best, mut idx) = (0.0, 0);
        for i in a + 1..b {
            let d = seg_dist2(line[i], line[a], line[b]);
            if d > best {
                best = d;
                idx = i;
            }
        }
        if best > t2 {
            keep[idx] = true;
            stack.push((a, idx));
            stack.push((idx, b));
        }
    }
    line.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(p, _)| *p)
        .collect()
}
