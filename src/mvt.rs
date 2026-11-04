// A minimal Mapbox Vector Tile decoder.

#[derive(Clone, Debug)]
pub enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl Value {
    pub fn as_str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            _ => "",
        }
    }
    pub fn as_f64(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            Value::Str(s) => s.parse().unwrap_or(0.0),
            Value::Bool(b) => *b as u8 as f64,
        }
    }
}

pub struct Feature {
    pub typ: u8, // 1 point, 2 line, 3 polygon
    tags: Vec<u32>,
    geometry: Vec<u32>,
}

pub struct Layer {
    pub name: String,
    pub extent: u32,
    keys: Vec<String>,
    values: Vec<Value>,
    pub features: Vec<Feature>,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> u64 {
        let mut v = 0u64;
        let mut s = 0;
        loop {
            let x = self.b[self.p];
            self.p += 1;
            v |= ((x & 0x7f) as u64) << s;
            if x & 0x80 == 0 {
                return v;
            }
            s += 7;
        }
    }
    fn done(&self) -> bool {
        self.p >= self.b.len()
    }
    fn bytes(&mut self) -> &'a [u8] {
        let n = self.varint() as usize;
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        s
    }
    fn skip(&mut self, wire: u64) {
        match wire {
            0 => {
                self.varint();
            }
            1 => self.p += 8,
            2 => {
                self.bytes();
            }
            5 => self.p += 4,
            _ => panic!("bad wire type {wire}"),
        }
    }
    fn packed(&mut self) -> Vec<u32> {
        let mut r = Reader {
            b: self.bytes(),
            p: 0,
        };
        let mut out = Vec::new();
        while !r.done() {
            out.push(r.varint() as u32);
        }
        out
    }
}

pub fn decode(buf: &[u8]) -> Vec<Layer> {
    let mut r = Reader { b: buf, p: 0 };
    let mut layers = Vec::new();
    while !r.done() {
        let k = r.varint();
        if k >> 3 == 3 && k & 7 == 2 {
            layers.push(layer(r.bytes()));
        } else {
            r.skip(k & 7);
        }
    }
    layers
}

fn layer(buf: &[u8]) -> Layer {
    let mut r = Reader { b: buf, p: 0 };
    let mut l = Layer {
        name: String::new(),
        extent: 4096,
        keys: vec![],
        values: vec![],
        features: vec![],
    };
    while !r.done() {
        let k = r.varint();
        match (k >> 3, k & 7) {
            (1, 2) => l.name = String::from_utf8_lossy(r.bytes()).into_owned(),
            (2, 2) => l.features.push(feature(r.bytes())),
            (3, 2) => l.keys.push(String::from_utf8_lossy(r.bytes()).into_owned()),
            (4, 2) => l.values.push(value(r.bytes())),
            (5, 0) => l.extent = r.varint() as u32,
            (_, w) => r.skip(w),
        }
    }
    l
}

fn feature(buf: &[u8]) -> Feature {
    let mut r = Reader { b: buf, p: 0 };
    let mut f = Feature {
        typ: 0,
        tags: vec![],
        geometry: vec![],
    };
    while !r.done() {
        let k = r.varint();
        match (k >> 3, k & 7) {
            (2, 2) => f.tags = r.packed(),
            (3, 0) => f.typ = r.varint() as u8,
            (4, 2) => f.geometry = r.packed(),
            (_, w) => r.skip(w),
        }
    }
    f
}

fn value(buf: &[u8]) -> Value {
    let mut r = Reader { b: buf, p: 0 };
    let mut v = Value::Str(String::new());
    while !r.done() {
        let k = r.varint();
        v = match (k >> 3, k & 7) {
            (1, 2) => Value::Str(String::from_utf8_lossy(r.bytes()).into_owned()),
            (2, 5) => {
                let x = f32::from_le_bytes(r.b[r.p..r.p + 4].try_into().unwrap());
                r.p += 4;
                Value::Num(x as f64)
            }
            (3, 1) => {
                let x = f64::from_le_bytes(r.b[r.p..r.p + 8].try_into().unwrap());
                r.p += 8;
                Value::Num(x)
            }
            (4, 0) => Value::Num(r.varint() as i64 as f64),
            (5, 0) => Value::Num(r.varint() as f64),
            (6, 0) => {
                let u = r.varint();
                Value::Num(((u >> 1) as i64 ^ -((u & 1) as i64)) as f64)
            }
            (7, 0) => Value::Bool(r.varint() != 0),
            (_, w) => {
                r.skip(w);
                continue;
            }
        };
    }
    v
}

impl Layer {
    pub fn get<'a>(&'a self, f: &Feature, key: &str) -> Option<&'a Value> {
        f.tags
            .chunks(2)
            .find(|kv| self.keys[kv[0] as usize] == key)
            .map(|kv| &self.values[kv[1] as usize])
    }
    pub fn str<'a>(&'a self, f: &Feature, key: &str) -> &'a str {
        self.get(f, key).map(|v| v.as_str()).unwrap_or("")
    }
}

impl Feature {
    /// Lines (or rings, for polygons) in tile coordinates.
    pub fn geometry(&self) -> Vec<Vec<(i32, i32)>> {
        let g = &self.geometry;
        let mut out: Vec<Vec<(i32, i32)>> = Vec::new();
        let (mut x, mut y) = (0i32, 0i32);
        let mut i = 0;
        let zz = |u: u32| ((u >> 1) as i32) ^ -((u & 1) as i32);
        while i < g.len() {
            let cmd = g[i] & 7;
            let n = (g[i] >> 3) as usize;
            i += 1;
            match cmd {
                1 | 2 => {
                    for _ in 0..n {
                        if i + 1 >= g.len() {
                            return out;
                        }
                        x += zz(g[i]);
                        y += zz(g[i + 1]);
                        i += 2;
                        if cmd == 1 {
                            out.push(vec![(x, y)]);
                        } else if let Some(l) = out.last_mut() {
                            l.push((x, y));
                        }
                    }
                }
                7 => {
                    if let Some(l) = out.last_mut() {
                        let first = l[0];
                        l.push(first);
                    }
                }
                _ => return out,
            }
        }
        out
    }
}
