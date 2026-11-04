// Elevation from Terrarium DEM tiles (the same source the website uses).
use crate::geo::{P, lonlat};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const URL: &str = "https://elevation-tiles-prod.s3.amazonaws.com/terrarium";
const MAX_TILES: usize = 3000; // decoded tiles kept in memory, 256 KB each

pub struct Dem {
    z: u8,
    cache: Option<PathBuf>,
    tiles: Mutex<HashMap<(u32, u32), Arc<Vec<f32>>>>,
    agent: ureq::Agent,
}

impl Dem {
    pub fn new(z: u8, cache: Option<PathBuf>) -> Dem {
        Dem {
            z,
            cache,
            tiles: Mutex::new(HashMap::new()),
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    fn fetch(&self, x: u32, y: u32) -> Vec<f32> {
        let z = self.z;
        let path = self
            .cache
            .as_ref()
            .map(|c| c.join(format!("{z}-{x}-{y}.png")));
        let bytes = match path.as_ref().and_then(|p| std::fs::read(p).ok()) {
            Some(b) => b,
            None => {
                let mut tries = 0;
                let b = loop {
                    match self.agent.get(&format!("{URL}/{z}/{x}/{y}.png")).call() {
                        Ok(r) => {
                            let mut b = Vec::new();
                            r.into_reader()
                                .read_to_end(&mut b)
                                .expect("reading DEM tile");
                            break b;
                        }
                        Err(ureq::Error::Status(404, _)) => break Vec::new(),
                        Err(e) if tries < 5 => {
                            tries += 1;
                            eprintln!("DEM {z}/{x}/{y}: {e}, retrying");
                            std::thread::sleep(std::time::Duration::from_millis(500 << tries));
                        }
                        Err(e) => panic!("DEM {z}/{x}/{y}: {e}"),
                    }
                };
                if let Some(p) = &path {
                    let _ = std::fs::create_dir_all(p.parent().unwrap());
                    let _ = std::fs::write(p, &b);
                }
                b
            }
        };
        if bytes.is_empty() {
            return vec![0.0; 256 * 256];
        }
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::EXPAND);
        let mut r = dec.read_info().expect("DEM png");
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).expect("DEM png");
        let ch = info.color_type.samples();
        (0..256 * 256)
            .map(|i| {
                let p = &buf[i * ch..];
                p[0] as f32 * 256.0 + p[1] as f32 + p[2] as f32 / 256.0 - 32768.0
            })
            .collect()
    }

    fn tile(&self, x: u32, y: u32) -> Arc<Vec<f32>> {
        if let Some(t) = self.tiles.lock().unwrap().get(&(x, y)) {
            return t.clone();
        }
        let t = Arc::new(self.fetch(x, y));
        let mut m = self.tiles.lock().unwrap();
        if m.len() >= MAX_TILES {
            // Crude eviction: drop about half.
            let drop: Vec<_> = m.keys().step_by(2).cloned().collect();
            for k in drop {
                m.remove(&k);
            }
        }
        m.insert((x, y), t.clone());
        t
    }

    /// Elevations at many points; tiles are looked up once per call.
    pub fn profile(&self, pts: &[P]) -> Vec<f64> {
        let mut local: HashMap<(u32, u32), Arc<Vec<f32>>> = HashMap::new();
        pts.iter()
            .map(|&p| {
                self.sample(p, &mut |x, y| {
                    local
                        .entry((x, y))
                        .or_insert_with(|| self.tile(x, y))
                        .clone()
                })
            })
            .collect()
    }

    fn sample(&self, p: P, tile: &mut dyn FnMut(u32, u32) -> Arc<Vec<f32>>) -> f64 {
        let (lon, lat) = lonlat(p);
        let n = (1u64 << self.z) as f64 * 256.0;
        let fx = (lon + 180.0) / 360.0 * n - 0.5;
        let fy = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n - 0.5;
        let (x0, y0) = (fx.floor() as i64, fy.floor() as i64);
        let (u, w) = (fx - x0 as f64, fy - y0 as f64);
        let mut v = |x: i64, y: i64| -> f64 {
            let t = tile((x / 256) as u32, (y / 256) as u32);
            t[(y % 256) as usize * 256 + (x % 256) as usize] as f64
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
