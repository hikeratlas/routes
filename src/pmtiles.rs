// A minimal PMTiles v3 reader, for a local file or a URL (HTTP range requests).
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

enum Backend {
    File(File),
    Http { url: String, agent: ureq::Agent },
}

pub struct PmTiles {
    backend: Backend,
    internal_compression: u8,
    tile_compression: u8,
    leaf_offset: u64,
    data_offset: u64,
    root: Vec<Entry>,
    /// min lon, min lat, max lon, max lat
    pub bounds: [f64; 4],
    /// The archive's vector layers, from its metadata (empty if it has none).
    pub layers: Vec<String>,
    leaves: Mutex<HashMap<u64, Arc<Vec<Entry>>>>,
    // For a URL: tiles are cached here, so reruns don't refetch them.
    cache: Option<PathBuf>,
}

fn decompress(data: Vec<u8>, compression: u8) -> std::io::Result<Vec<u8>> {
    match compression {
        0 | 1 => Ok(data),
        2 => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&data[..]).read_to_end(&mut out)?;
            Ok(out)
        }
        4 => zstd::stream::decode_all(&data[..]),
        c => Err(std::io::Error::other(format!(
            "unsupported compression {c}"
        ))),
    }
}

fn varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn parse_dir(buf: &[u8]) -> Vec<Entry> {
    let mut p = 0;
    let n = varint(buf, &mut p) as usize;
    let mut e = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 0
        };
        n
    ];
    let mut last = 0;
    for x in e.iter_mut() {
        last += varint(buf, &mut p);
        x.tile_id = last;
    }
    for x in e.iter_mut() {
        x.run_length = varint(buf, &mut p) as u32;
    }
    for x in e.iter_mut() {
        x.length = varint(buf, &mut p) as u32;
    }
    for i in 0..n {
        let v = varint(buf, &mut p);
        e[i].offset = if v == 0 && i > 0 {
            e[i - 1].offset + e[i - 1].length as u64
        } else {
            v.wrapping_sub(1)
        };
    }
    e
}

fn find(dir: &[Entry], id: u64) -> Option<Entry> {
    let i = dir.partition_point(|e| e.tile_id <= id);
    if i == 0 {
        return None;
    }
    let e = dir[i - 1];
    if e.run_length == 0 || id - e.tile_id < e.run_length as u64 {
        Some(e)
    } else {
        None
    }
}

pub fn tile_id(z: u8, x: u32, y: u32) -> u64 {
    let acc: u64 = (0..z).map(|i| 1u64 << (2 * i)).sum();
    let (mut x, mut y) = (x as u64, y as u64);
    let mut d = 0u64;
    let mut s = if z == 0 { 0 } else { 1u64 << (z - 1) };
    while s > 0 {
        let rx = ((x & s) > 0) as u64;
        let ry = ((y & s) > 0) as u64;
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - (x & (s - 1));
                y = s - 1 - (y & (s - 1));
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    acc + d
}

impl PmTiles {
    pub fn open(src: &str, cache: Option<PathBuf>) -> std::io::Result<PmTiles> {
        let backend = if src.starts_with("http://") || src.starts_with("https://") {
            Backend::Http {
                url: src.to_string(),
                // As the DEM's (dem.rs): a stalled request fails, not hangs.
                agent: ureq::AgentBuilder::new()
                    .try_proxy_from_env(true)
                    .timeout_connect(std::time::Duration::from_secs(10))
                    .timeout(std::time::Duration::from_secs(60))
                    .build(),
            }
        } else {
            Backend::File(File::open(src)?)
        };
        let mut t = PmTiles {
            backend,
            internal_compression: 0,
            tile_compression: 0,
            leaf_offset: 0,
            data_offset: 0,
            root: vec![],
            bounds: [0.0; 4],
            layers: vec![],
            leaves: Mutex::new(HashMap::new()),
            cache: if src.starts_with("http") { cache } else { None },
        };
        let h = t.read(0, 16384)?;
        if &h[0..7] != b"PMTiles" || h[7] != 3 {
            return Err(std::io::Error::other(format!(
                "{src}: not a PMTiles v3 archive"
            )));
        }
        let u = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
        let i = |o: usize| i32::from_le_bytes(h[o..o + 4].try_into().unwrap()) as f64 / 1e7;
        t.bounds = [i(102), i(106), i(110), i(114)];
        t.internal_compression = h[97];
        t.tile_compression = h[98];
        t.leaf_offset = u(40);
        t.data_offset = u(56);
        let (ro, rl) = (u(8) as usize, u(16) as usize);
        let root = if ro + rl <= h.len() {
            h[ro..ro + rl].to_vec()
        } else {
            t.read(ro as u64, rl)?
        };
        t.root = parse_dir(&decompress(root, t.internal_compression)?);
        let (mo, ml) = (u(24), u(32) as usize);
        if ml > 0 {
            let meta = decompress(t.read(mo, ml)?, t.internal_compression)?;
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&meta) {
                t.layers = v["vector_layers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["id"].as_str().map(String::from))
                    .collect();
            }
        }
        Ok(t)
    }

    fn read(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        match &self.backend {
            Backend::File(f) => {
                let mut buf = vec![0; len];
                let n = f.read_at(&mut buf, offset)?;
                buf.truncate(n);
                Ok(buf)
            }
            Backend::Http { url, agent } => {
                let mut tries = 0;
                loop {
                    let r = agent
                        .get(url)
                        .set(
                            "Range",
                            &format!("bytes={}-{}", offset, offset + len as u64 - 1),
                        )
                        .call();
                    match r {
                        Ok(resp) => {
                            let mut buf = Vec::with_capacity(len);
                            resp.into_reader().read_to_end(&mut buf)?;
                            return Ok(buf);
                        }
                        Err(e) if tries < 4 => {
                            tries += 1;
                            eprintln!("retrying {url} ({e})");
                            std::thread::sleep(std::time::Duration::from_millis(500 << tries));
                        }
                        Err(e) => return Err(std::io::Error::other(e.to_string())),
                    }
                }
            }
        }
    }

    /// The tile's bytes, decompressed, or None if the archive has no such tile.
    pub fn get(&self, z: u8, x: u32, y: u32) -> std::io::Result<Option<Vec<u8>>> {
        let cached = self
            .cache
            .as_ref()
            .map(|c| c.join(format!("{z}/{x}/{y}.mvt")));
        if let Some(p) = &cached {
            if let Ok(b) = std::fs::read(p) {
                // Read now: a cache kept between runs is pruned to the
                // tiles a run read (as dem.rs's).
                crate::dem::touch(p, std::time::SystemTime::now());
                return Ok(if b.is_empty() { None } else { Some(b) });
            }
        }
        let r = self.fetch(z, x, y)?;
        if let Some(p) = &cached {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(p, r.as_deref().unwrap_or(&[]))?;
        }
        Ok(r)
    }

    fn fetch(&self, z: u8, x: u32, y: u32) -> std::io::Result<Option<Vec<u8>>> {
        let id = tile_id(z, x, y);
        let mut dir: Arc<Vec<Entry>> = Arc::new(self.root.clone());
        for _ in 0..4 {
            let Some(e) = find(&dir, id) else {
                return Ok(None);
            };
            if e.run_length > 0 {
                let data = self.read(self.data_offset + e.offset, e.length as usize)?;
                return Ok(Some(decompress(data, self.tile_compression)?));
            }
            let key = e.offset;
            let hit = self.leaves.lock().unwrap().get(&key).cloned();
            dir = match hit {
                Some(d) => d,
                None => {
                    let raw = self.read(self.leaf_offset + e.offset, e.length as usize)?;
                    let d = Arc::new(parse_dir(&decompress(raw, self.internal_compression)?));
                    self.leaves.lock().unwrap().insert(key, d.clone());
                    d
                }
            };
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::tile_id;
    #[test]
    fn ids() {
        assert_eq!(tile_id(0, 0, 0), 0);
        assert_eq!(tile_id(1, 0, 0), 1);
        assert_eq!(tile_id(1, 0, 1), 2);
        assert_eq!(tile_id(1, 1, 1), 3);
        assert_eq!(tile_id(1, 1, 0), 4);
        assert_eq!(tile_id(2, 0, 0), 5);
    }
}
