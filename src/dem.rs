// Elevation from Mapterhorn's Terrarium-encoded DEM tiles (the same source
// as the website's hillshade and contours, hikeratlas/basemap#187): 512 px
// WebP, lidar resolution where it exists (US 1 m, Canada's HRDEM 2 m), USGS
// 10 m across the US, and Copernicus 30 m elsewhere. Its z12 is global; past
// a region's coverage tiles 404, and a point is sampled from the nearest
// ancestor instead.
//
// Over Newfoundland, hikeratlas/nl-dem's tiles of the province's 5 m DEM come
// first, as on the website: Mapterhorn only has 30 m data there.
use crate::geo::{P, lonlat};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const URL: &str = "https://tiles.mapterhorn.com";
pub const NL_DEM: &str = "https://public.hikeratlas.com/nl-dem/nl-dem.pmtiles";
const NL_ZOOMS: std::ops::RangeInclusive<u8> = 10..=13;
const TILE: usize = 512;
const MAX_TILES: usize = 1000; // decoded tiles kept in memory, 1 MB each
// Downloads at once per park in prefetch(); curate runs a park per core.
const PREFETCH_THREADS: usize = 8;

// For the log (stats()): what the DEM cost a run.
static PREFETCHED: AtomicU64 = AtomicU64::new(0);
static PREFETCH_MS: AtomicU64 = AtomicU64::new(0);
static DOWNLOADED: AtomicU64 = AtomicU64::new(0);
static DOWNLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);

/// What the DEM cost this run: tiles read from the cache, prefetched, and
/// downloaded while curating (by a curate thread, which waits for it).
pub fn stats() -> String {
    let (p, d) = (PREFETCHED.load(Relaxed), DOWNLOADED.load(Relaxed));
    format!(
        "DEM: {} tiles from the cache, {} prefetched ({:.0}s wall, summed over parks), {} downloaded while curating; {:.2} GB downloaded",
        CACHE_HITS.load(Relaxed),
        p,
        PREFETCH_MS.load(Relaxed) as f64 / 1e3,
        d - p.min(d),
        DOWNLOAD_BYTES.load(Relaxed) as f64 / 1e9
    )
}

type Grid = Arc<Vec<f32>>;

/// Sets a file's modification time, if it can.
pub fn touch(p: &std::path::Path, t: std::time::SystemTime) {
    if let Ok(f) = std::fs::File::options().write(true).open(p) {
        let _ = f.set_modified(t);
    }
}

pub struct Dem {
    z: u8,
    cache: Option<PathBuf>,
    // None: the tile doesn't exist (404).
    tiles: Mutex<HashMap<(u8, u32, u32), Option<Grid>>>,
    agent: ureq::Agent,
    nl: Option<crate::pmtiles::PmTiles>,
}

/// Terrarium heights from a WebP tile: h = R * 256 + G + B / 256 - 32768.
fn decode(bytes: &[u8]) -> Vec<f32> {
    let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).expect("DEM webp");
    let (w, h) = dec.dimensions();
    assert!(w as usize == TILE && h as usize == TILE, "DEM tile is {w}x{h}, not {TILE}");
    let ch = if dec.has_alpha() { 4 } else { 3 };
    let mut buf = vec![0; dec.output_buffer_size().expect("DEM webp size")];
    dec.read_image(&mut buf).expect("DEM webp");
    (0..TILE * TILE)
        .map(|i| {
            let p = &buf[i * ch..];
            p[0] as f32 * 256.0 + p[1] as f32 + p[2] as f32 / 256.0 - 32768.0
        })
        .collect()
}

impl Dem {
    /// `nl`: nl-dem's archive URL, or None for Mapterhorn alone.
    pub fn new(z: u8, cache: Option<PathBuf>, nl: Option<&str>) -> Dem {
        let nl_cache = cache.as_ref().map(|c| c.join("nl-dem"));
        let nl = nl.and_then(|u| match crate::pmtiles::PmTiles::open(u, nl_cache) {
            Ok(a) => Some(a),
            Err(e) => {
                eprintln!("nl-dem unavailable ({e}); using Mapterhorn alone");
                None
            }
        });
        // Timeouts, so a stalled download fails and is retried rather than
        // holding a curate thread: with none, Hubbel Pond WMA (110 tiles,
        // 0.0 s with its DEM cached) took 482 s in basemap run 42428077515,
        // past --park-timeout, which isn't checked while a tile downloads.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build();
        Dem { z, cache, tiles: Mutex::new(HashMap::new()), agent, nl }
    }

    /// nl-dem's tile, if it has one (the reader caches tiles under nl-dem/).
    fn fetch_nl(&self, z: u8, x: u32, y: u32) -> Option<Vec<u8>> {
        let a = self.nl.as_ref()?;
        if !NL_ZOOMS.contains(&z) {
            return None;
        }
        a.get(z, x, y).unwrap_or_else(|e| panic!("nl-dem {z}/{x}/{y}: {e}"))
    }

    fn path(&self, z: u8, x: u32, y: u32) -> Option<PathBuf> {
        self.cache
            .as_ref()
            .map(|c| c.join("mapterhorn").join(format!("{z}-{x}-{y}.webp")))
    }

    /// A tile's bytes, or None if it doesn't exist. Cached on disk (a 404 as
    /// an empty file) under mapterhorn/, apart from older Terrarium caches.
    /// A cached tile's modification time is set to now when it's read, so a
    /// cache kept between runs can be pruned to the tiles a run used.
    fn fetch(&self, z: u8, x: u32, y: u32) -> Option<Vec<u8>> {
        let path = self.path(z, x, y);
        let bytes = match path.as_ref().and_then(|p| std::fs::read(p).ok()) {
            Some(b) => {
                touch(path.as_ref().unwrap(), std::time::SystemTime::now());
                CACHE_HITS.fetch_add(1, Relaxed);
                b
            }
            None => {
                let b = self.download(z, x, y);
                if let Some(p) = &path {
                    let _ = std::fs::create_dir_all(p.parent().unwrap());
                    let _ = std::fs::write(p, &b);
                }
                b
            }
        };
        if bytes.is_empty() { None } else { Some(bytes) }
    }

    /// A tile from Mapterhorn: its bytes, or empty if it doesn't exist.
    fn download(&self, z: u8, x: u32, y: u32) -> Vec<u8> {
        // Retry a failed request, and a connection dropped while reading the
        // body: one reset mid-download (basemap run 42134348228) panicked a
        // whole shard.
        let mut tries = 0;
        loop {
            let got = match self.agent.get(&format!("{URL}/{z}/{x}/{y}.webp")).call() {
                Ok(r) => {
                    let mut b = Vec::new();
                    r.into_reader()
                        .read_to_end(&mut b)
                        .map(|_| b)
                        .map_err(|e| e.to_string())
                }
                Err(ureq::Error::Status(404, _)) => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            };
            match got {
                Ok(b) => {
                    DOWNLOADED.fetch_add(1, Relaxed);
                    DOWNLOAD_BYTES.fetch_add(b.len() as u64, Relaxed);
                    return b;
                }
                Err(e) if tries < 6 => {
                    tries += 1;
                    eprintln!("DEM {z}/{x}/{y}: {e}, retrying");
                    std::thread::sleep(std::time::Duration::from_millis(500 << tries));
                }
                Err(e) => panic!("DEM {z}/{x}/{y}: {e}"),
            }
        }
    }

    /// Downloads, PREFETCH_THREADS at a time, the tiles at this DEM's zoom
    /// under any of `pts` (world units): a park's graph nodes, which the
    /// routes it evaluates follow. Otherwise each curate thread fetched its
    /// tiles one at a time as it evaluated, and on north-america the CPUs
    /// sat ~40% idle (routes#86).
    ///
    /// Only for a park with none of its tiles cached yet: once a cache is
    /// kept between runs (pruned to the tiles a run read), the rest come from
    /// there, and refetching what was pruned would only load Mapterhorn. The
    /// tiles are written with an old modification time, so a prune drops any
    /// that no route read. Needs a --dem-cache.
    pub fn prefetch(&self, pts: impl Iterator<Item = P>) {
        if self.cache.is_none() {
            return;
        }
        let shift = 26 - self.z as u32; // world units are z14 x 4096 = 2^26
        let mut tiles: Vec<(u32, u32)> = pts
            .map(|p| ((p.0 as u32) >> shift, (p.1 as u32) >> shift))
            .collect();
        tiles.sort_unstable();
        tiles.dedup();
        // nl-dem's tiles come first where it has them (see tile()).
        if let Some(nl) = &self.nl {
            let n = (1u64 << self.z) as f64;
            let [w, s, e, nn] = nl.bounds;
            tiles.retain(|&(x, y)| {
                let (lon, lat) = lonlat(((x as f64 + 0.5) / n * crate::geo::WORLD, (y as f64 + 0.5) / n * crate::geo::WORLD));
                !(lon >= w && lon <= e && lat >= s && lat <= nn)
            });
        }
        if tiles.iter().any(|&(x, y)| self.path(self.z, x, y).is_some_and(|p| p.exists())) {
            return;
        }
        let t0 = std::time::Instant::now();
        PREFETCHED.fetch_add(tiles.len() as u64, Relaxed);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(86400);
        std::thread::scope(|sc| {
            for _ in 0..PREFETCH_THREADS.min(tiles.len()) {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(x, y)) = tiles.get(i) else { break };
                        let p = self.path(self.z, x, y).unwrap();
                        let b = self.download(self.z, x, y);
                        let _ = std::fs::create_dir_all(p.parent().unwrap());
                        if std::fs::write(&p, &b).is_ok() {
                            touch(&p, old);
                        }
                    }
                });
            }
        });
        PREFETCH_MS.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
    }

    fn tile(&self, z: u8, x: u32, y: u32) -> Option<Grid> {
        if let Some(t) = self.tiles.lock().unwrap().get(&(z, x, y)) {
            return t.clone();
        }
        // Past nl-dem's max zoom, where it covers: none here, so the point is
        // sampled from nl-dem's tile, not Mapterhorn's blurrier one.
        let nl_max = *NL_ZOOMS.end();
        if z > nl_max && self.fetch_nl(nl_max, x >> (z - nl_max), y >> (z - nl_max)).is_some() {
            self.tiles.lock().unwrap().insert((z, x, y), None);
            return None;
        }
        let t = self.fetch_nl(z, x, y).or_else(|| self.fetch(z, x, y)).map(|b| Arc::new(decode(&b)));
        let mut m = self.tiles.lock().unwrap();
        if m.len() >= MAX_TILES {
            // Crude eviction: drop about half.
            let drop: Vec<_> = m.keys().step_by(2).cloned().collect();
            for k in drop {
                m.remove(&k);
            }
        }
        m.insert((z, x, y), t.clone());
        t
    }

    /// Elevations at many points; tiles are looked up once per call.
    pub fn profile(&self, pts: &[P]) -> Vec<f64> {
        let mut local: HashMap<(u8, u32, u32), Option<Grid>> = HashMap::new();
        pts.iter()
            .map(|&p| {
                self.sample(p, &mut |z, x, y| {
                    local.entry((z, x, y)).or_insert_with(|| self.tile(z, x, y)).clone()
                })
            })
            .collect()
    }

    fn sample(&self, p: P, tile: &mut dyn FnMut(u8, u32, u32) -> Option<Grid>) -> f64 {
        let (lon, lat) = lonlat(p);
        let t = TILE as i64;
        let n = (1u64 << self.z) as f64 * TILE as f64;
        let fx = (lon + 180.0) / 360.0 * n - 0.5;
        let fy = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n - 0.5;
        let (x0, y0) = (fx.floor() as i64, fy.floor() as i64);
        let (u, w) = (fx - x0 as f64, fy - y0 as f64);
        // A pixel at self.z, from the nearest zoom that has its tile.
        let mut v = |x: i64, y: i64| -> f64 {
            for up in 0..=self.z {
                let (px, py) = (x >> up, y >> up);
                if let Some(g) = tile(self.z - up, (px / t) as u32, (py / t) as u32) {
                    return g[(py % t) as usize * TILE + (px % t) as usize] as f64;
                }
            }
            0.0
        };
        (v(x0, y0) * (1.0 - u) + v(x0 + 1, y0) * u) * (1.0 - w)
            + (v(x0, y0 + 1) * (1.0 - u) + v(x0 + 1, y0 + 1) * u) * w
    }
}

pub fn gain_loss(ele: &[f64], thr: f64) -> (f64, f64) {
    let (mut up, mut down, mut r) = (0.0, 0.0, ele[0]);
    for &e in &ele[1..] {
        if e - r >= thr {
            up += e - r;
            r = e;
        } else if r - e >= thr {
            down += r - e;
            r = e;
        }
    }
    (up, down)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terrarium_webp(h: impl Fn(usize, usize) -> f32) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(TILE * TILE * 3);
        for y in 0..TILE {
            for x in 0..TILE {
                let v = (h(x, y) + 32768.0) as f64;
                let r = (v / 256.0).floor();
                let g = v.floor() - r * 256.0;
                let b = ((v - v.floor()) * 256.0).floor();
                rgb.extend([r as u8, g as u8, b as u8]);
            }
        }
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(&rgb, TILE as u32, TILE as u32, image_webp::ColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn decodes_terrarium_webp() {
        let h = decode(&terrarium_webp(|x, y| x as f32 * 0.5 + y as f32 - 20.25));
        assert_eq!(h.len(), TILE * TILE);
        assert!((h[0] - -20.25).abs() < 1.0 / 256.0);
        assert!((h[3 * TILE + 10] - (5.0 + 3.0 - 20.25)).abs() < 1.0 / 256.0);
    }

    // A point whose tile 404s at the DEM's zoom is sampled from its parent.
    #[test]
    fn samples_from_the_parent_past_coverage() {
        let dem = Dem::new(13, None, None);
        let (lon, lat) = (-116.2, 51.42);
        let wx = (lon + 180.0) / 360.0 * crate::geo::WORLD;
        let wy = (1.0 - f64::to_radians(lat).tan().asinh() / std::f64::consts::PI) / 2.0 * crate::geo::WORLD;
        let p: P = (wx, wy);
        let parent = Arc::new(vec![1234.0f32; TILE * TILE]);
        let mut asked = Vec::new();
        let e = dem.sample(p, &mut |z, x, y| {
            asked.push((z, x, y));
            if z == 12 { Some(parent.clone()) } else { None }
        });
        assert!((e - 1234.0).abs() < 1e-6);
        assert!(asked.iter().any(|a| a.0 == 13) && asked.iter().any(|a| a.0 == 12));
    }
}
