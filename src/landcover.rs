// Land cover where OSM maps none, from ESA WorldCover 2021 (10 m, CC BY 4.0;
// hikeratlas/basemap#167). OSM often maps a mountain's forest and leaves what's
// above the trees blank, so without it most alpine hikes look like they never
// leave the forest (open_high 0), and the website's terrain bar shows only
// the part OSM maps.
//
// basemap's bin/worldcover.py bakes it into z12 512 px PNG tiles of
// WorldCover's own class codes (12-16 m a pixel across North America), read
// here like nl-dem's archive. Area::landcover asks it only where no OSM land
// polygon covers a point: OSM still wins wherever it says anything.
use crate::area::OTHER;
use crate::pmtiles::PmTiles;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub const URL: &str = "https://public.hikeratlas.com/worldcover/north-america.pmtiles";
const Z: u8 = 12;
const TILE: usize = 512;

/// A WorldCover class code as one of area.rs's LANDCOVER classes (OTHER for
/// none: cropland, built-up, no data).
pub fn class(code: u8) -> u8 {
    match code {
        10 => 6,       // tree cover: forest
        20 => 4,       // shrubland: scrub
        30 | 100 => 3, // grassland, moss and lichen (alpine tundra): meadow
        60 => 2,       // bare / sparse vegetation: alpine rock
        70 => 0,       // snow and ice: glacier
        80 => 1,       // permanent water
        90 | 95 => 5,  // herbaceous wetland, mangroves: wetland
        _ => OTHER,
    }
}

/// A tile's pixels, as LANDCOVER classes. An error for bytes that aren't an
/// 8-bit greyscale 512 px PNG.
fn decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .map_err(|e| format!("png: {e}"))?;
    let info = r.info();
    if info.width as usize != TILE || info.height as usize != TILE {
        return Err(format!(
            "tile is {}x{}, not {TILE}",
            info.width, info.height
        ));
    }
    if info.color_type != png::ColorType::Grayscale || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "tile is {:?} {:?}, not 8-bit greyscale",
            info.color_type, info.bit_depth
        ));
    }
    let mut buf = vec![0; TILE * TILE];
    r.next_frame(&mut buf).map_err(|e| format!("png: {e}"))?;
    for v in &mut buf {
        *v = class(*v);
    }
    Ok(buf)
}

pub struct WorldCover {
    archive: PmTiles,
}

impl WorldCover {
    /// `src`: the archive's URL or file. A URL's tiles are cached under
    /// `cache`/worldcover/. None, with a warning, if it can't be opened:
    /// routes then uses OSM's land cover alone.
    pub fn open(src: &str, cache: Option<PathBuf>) -> Option<Arc<WorldCover>> {
        match PmTiles::open(src, cache.map(|c| c.join("worldcover"))) {
            Ok(archive) => Some(Arc::new(WorldCover { archive })),
            Err(e) => {
                eprintln!("WorldCover unavailable ({e}); using OSM's land cover alone");
                None
            }
        }
    }

    /// The z12 tile's classes, or None where the archive has none (open sea).
    fn tile(&self, x: u32, y: u32) -> Option<Arc<[u8]>> {
        let b = self
            .archive
            .get(Z, x, y)
            .unwrap_or_else(|e| panic!("WorldCover {Z}/{x}/{y}: {e}"))?;
        Some(
            decode(&b)
                .unwrap_or_else(|e| panic!("WorldCover {Z}/{x}/{y}: {e}"))
                .into(),
        )
    }
}

/// One park's WorldCover tiles, read as its routes first sample them, and
/// dropped with its Area.
pub struct Lookup {
    wc: Arc<WorldCover>,
    tiles: RwLock<HashMap<(u32, u32), Option<Arc<[u8]>>>>,
}

impl Lookup {
    pub fn new(wc: Arc<WorldCover>) -> Lookup {
        Lookup {
            wc,
            tiles: RwLock::new(HashMap::new()),
        }
    }

    /// The LANDCOVER class at a point, or OTHER.
    pub fn at(&self, (lon, lat): (f64, f64)) -> u8 {
        let n = (1u64 << Z) as f64 * TILE as f64;
        let fx = (lon + 180.0) / 360.0 * n;
        let fy = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
        if !(0.0..n).contains(&fx) || !(0.0..n).contains(&fy) {
            return OTHER;
        }
        let (px, py) = (fx as usize, fy as usize);
        let key = ((px / TILE) as u32, (py / TILE) as u32);
        let at = |t: &Option<Arc<[u8]>>| {
            t.as_ref()
                .map_or(OTHER, |g| g[(py % TILE) * TILE + px % TILE])
        };
        if let Some(t) = self.tiles.read().unwrap().get(&key) {
            return at(t);
        }
        // Read outside the lock: another thread may fetch it too, harmlessly.
        let t = self.wc.tile(key.0, key.1);
        let v = at(&t);
        self.tiles.write().unwrap().insert(key, t);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey_png(px: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, TILE as u32, TILE as u32);
        e.set_color(png::ColorType::Grayscale);
        e.set_depth(png::BitDepth::Eight);
        let data: Vec<u8> = (0..TILE * TILE).map(|i| px(i % TILE, i / TILE)).collect();
        e.write_header().unwrap().write_image_data(&data).unwrap();
        out
    }

    #[test]
    fn decodes_worldcover_codes_as_classes() {
        let g = decode(&grey_png(|x, _| if x < 256 { 10 } else { 60 })).unwrap();
        assert_eq!(g[0], 6);
        assert_eq!(g[300], 2);
        assert_eq!(g[TILE * TILE - 1], 2);
    }

    #[test]
    fn maps_every_code() {
        let want = [
            (0, OTHER),
            (10, 6),
            (20, 4),
            (30, 3),
            (40, OTHER),
            (50, OTHER),
            (60, 2),
            (70, 0),
            (80, 1),
            (90, 5),
            (95, 5),
            (100, 3),
        ];
        for (code, c) in want {
            assert_eq!(class(code), c, "code {code}");
        }
    }

    #[test]
    fn rejects_other_tiles() {
        assert!(decode(b"not a png").is_err());
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, 256, 256);
        e.set_color(png::ColorType::Grayscale);
        e.write_header()
            .unwrap()
            .write_image_data(&[0; 256 * 256])
            .unwrap();
        assert!(decode(&out).is_err());
    }
}
