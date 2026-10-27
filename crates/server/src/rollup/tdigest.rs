// SPDX-License-Identifier: FSL-1.1-ALv2 AND MIT
//
// Contains a port of the tdigest gem 0.2.1 (lib/tdigest/tdigest.rb),
// Copyright (c) 2015 Sebastian Wallin, MIT license (full text in
// THIRD-PARTY-LICENSES, "tdigest").

//! Port of the tdigest gem 0.2.1 (lib/tdigest/tdigest.rb), statement for
//! statement, including its quirks, so `as_small_bytes` comes out byte for
//! byte the same as Ruby's from the same inputs and the same RNG state:
//!
//! - centroids live in an RBTree keyed by the value that *created* them; a
//!   centroid's mean drifts as weight is added but its key never moves;
//! - Ruby numbers: a digest fed Integers keeps Integer means (`_add_weight`
//!   divides with Integer floor division); one fed Floats (from_bytes) keeps
//!   Floats; mixing follows Ruby's coercion. `Num` models exactly that;
//! - `_new_centroid` on a key that already exists replaces that centroid
//!   (its weight disappears from the tree), and `_cumulate` then resets `@n`
//!   to what the tree holds;
//! - `compress!` replays the centroids in `Array#shuffle` order using Ruby's
//!   global Mersenne Twister, and runs inside `_digest` whenever the tree
//!   grows past k/delta = 2,500 centroids (sorted input does that).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::random::RubyRandom;

pub const VERBOSE_ENCODING: u32 = 1;
pub const SMALL_ENCODING: u32 = 2;

/// A Ruby Integer or Float.
#[derive(Debug, Clone, Copy)]
pub enum Num {
    I(i128),
    F(f64),
}

impl Num {
    pub fn to_f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }

    /// `#to_i`: Integers stay, Floats truncate toward zero.
    pub fn to_i(self) -> i64 {
        match self {
            Num::I(i) => i as i64,
            Num::F(f) => f as i64,
        }
    }

    fn sub(self, o: Num) -> Num {
        match (self, o) {
            (Num::I(a), Num::I(b)) => Num::I(a - b),
            (a, b) => Num::F(a.to_f() - b.to_f()),
        }
    }

    fn add(self, o: Num) -> Num {
        match (self, o) {
            (Num::I(a), Num::I(b)) => Num::I(a + b),
            (a, b) => Num::F(a.to_f() + b.to_f()),
        }
    }

    fn abs(self) -> Num {
        match self {
            Num::I(i) => Num::I(i.abs()),
            Num::F(f) => Num::F(f.abs()),
        }
    }

    /// `n * self / d` with Integer n and d, as Ruby evaluates it.
    fn scale(self, n: i64, d: i64) -> Num {
        match self {
            Num::I(i) => Num::I((n as i128 * i).div_euclid(d as i128)),
            Num::F(f) => Num::F(n as f64 * f / d as f64),
        }
    }
}

impl PartialEq for Num {
    fn eq(&self, other: &Num) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Num {}
impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Num) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Num {
    /// Ruby's `<=>`, exact across Integer and Float (no NaN ever reaches a digest).
    fn cmp(&self, other: &Num) -> Ordering {
        match (*self, *other) {
            (Num::I(a), Num::I(b)) => a.cmp(&b),
            (Num::F(a), Num::F(b)) => a.partial_cmp(&b).expect("NaN in t-digest"),
            (Num::I(a), Num::F(b)) => int_float_cmp(a, b),
            (Num::F(a), Num::I(b)) => int_float_cmp(b, a).reverse(),
        }
    }
}

fn int_float_cmp(i: i128, f: f64) -> Ordering {
    let fi = i as f64;
    match fi.partial_cmp(&f).expect("NaN in t-digest") {
        Ordering::Equal if f.fract() == 0.0 && f.abs() < 1.7e38 => i.cmp(&(f as i128)),
        o => o,
    }
}

#[derive(Debug, Clone)]
struct Centroid {
    mean: Num,
    n: i64,
    cumn: i64,
    mean_cumn: Option<f64>,
}

pub struct TDigest {
    delta: f64,
    k: f64,
    cx: f64,
    centroids: BTreeMap<Num, Centroid>,
    n: i64,
    last_cumulate: i64,
}

impl TDigest {
    /// `TDigest.new(delta)` with the gem's k = 25, cx = 1.1.
    pub fn new(delta: f64) -> TDigest {
        TDigest { delta, k: 25.0, cx: 1.1, centroids: BTreeMap::new(), n: 0, last_cumulate: 0 }
    }

    pub fn compression(&self) -> f64 {
        1.0 / self.delta
    }

    pub fn size(&self) -> i64 {
        self.n
    }

    pub fn centroid_count(&self) -> usize {
        self.centroids.len()
    }

    pub fn push(&mut self, x: Num, n: i64, rng: &mut RubyRandom) {
        self.digest(x, n, rng);
    }

    pub fn push_int(&mut self, x: i64, rng: &mut RubyRandom) {
        self.digest(Num::I(x as i128), 1, rng);
    }

    fn reset(&mut self) {
        self.centroids.clear();
        self.n = 0;
        self.last_cumulate = 0;
    }

    pub fn compress(&mut self, rng: &mut RubyRandom) {
        let mut points: Vec<(Num, i64)> = self.centroids.values().map(|c| (c.mean, c.n)).collect();
        self.reset();
        rng.shuffle(&mut points);
        for (mean, n) in points {
            self.digest(mean, n, rng);
        }
        self.cumulate(true, true);
    }

    fn find_nearest(&self, x: Num) -> Option<Num> {
        // rbtree: upper_bound(x) is the greatest key <= x, lower_bound(x)
        // the least key >= x. The gem names them the other way round.
        let ceil = self.centroids.range(..=x).next_back().map(|(k, _)| *k);
        let floor = self.centroids.range(x..).next().map(|(k, _)| *k);
        match (ceil, floor) {
            (None, None) => None,
            (None, f) => f,
            (c, None) => c,
            (Some(c), Some(f)) => {
                if f.sub(x).abs() < c.sub(x).abs() {
                    Some(f)
                } else {
                    Some(c)
                }
            }
        }
    }

    fn digest(&mut self, x: Num, n: i64, rng: &mut RubyRandom) {
        let min = self.centroids.keys().next().copied();
        let max = self.centroids.keys().next_back().copied();
        let nearest = self.find_nearest(x);
        self.n += n;

        let nearest_mean = nearest.map(|k| self.centroids[&k].mean);
        if let Some(key) = nearest.filter(|_| nearest_mean == Some(x)) {
            self.add_weight(key, x, n);
        } else if nearest == min {
            self.new_centroid(x, n, 0);
        } else if nearest == max {
            self.new_centroid(x, n, self.n);
        } else {
            let key = nearest.unwrap();
            let c = &self.centroids[&key];
            let p = c.mean_cumn.unwrap_or(0.0) / self.n as f64;
            let max_n = (((4 * self.n) as f64) * self.delta * p * (1.0 - p)).floor() as i64;
            if max_n - c.n >= n {
                self.add_weight(key, x, n);
            } else {
                let cumn = c.cumn;
                self.new_centroid(x, n, cumn);
            }
        }

        self.cumulate(false, false);
        if self.centroids.len() as f64 > self.k / self.delta {
            self.compress(rng);
        }
    }

    fn add_weight(&mut self, key: Num, x: Num, n: i64) {
        let needs_cumulate = {
            let c = self.centroids.get_mut(&key).unwrap();
            if x != c.mean {
                c.mean = c.mean.add(x.sub(c.mean).scale(n, c.n + n));
            }
            c.mean_cumn.is_none()
        };
        if needs_cumulate {
            self.cumulate(false, true);
        }
        let c = self.centroids.get_mut(&key).unwrap();
        c.cumn += n;
        c.mean_cumn = Some(c.mean_cumn.unwrap() + n as f64 / 2.0);
        c.n += n;
    }

    fn new_centroid(&mut self, x: Num, n: i64, cumn: i64) {
        // RBTree#[]= on an existing key replaces the value and keeps the key.
        self.centroids.insert(x, Centroid { mean: x, n, cumn, mean_cumn: None });
    }

    fn cumulate(&mut self, exact: bool, force: bool) {
        if !force {
            let factor = if self.last_cumulate == 0 { f64::INFINITY } else { self.n as f64 / self.last_cumulate as f64 };
            if self.n == self.last_cumulate || (!exact && self.cx > factor) {
                return;
            }
        }
        let mut cumn: i64 = 0;
        for c in self.centroids.values_mut() {
            c.mean_cumn = Some(cumn as f64 + c.n as f64 / 2.0);
            cumn += c.n;
            c.cumn = cumn;
        }
        self.n = cumn;
        self.last_cumulate = cumn;
    }

    /// `percentile(p)`: the mean of the first centroid whose mean_cumn
    /// reaches n * p (or the last one); None on an empty digest.
    pub fn percentile(&mut self, p: f64) -> Option<Num> {
        assert!((0.0..=1.0).contains(&p), "p should be in [0,1], got {p}");
        if self.n == 0 {
            return None;
        }
        self.cumulate(true, false);
        let h = self.n as f64 * p;
        let mut last: Option<&Centroid> = None;
        for c in self.centroids.values() {
            let mc = c.mean_cumn.unwrap_or(0.0);
            if mc == h {
                return Some(c.mean);
            } else if mc > h {
                // [last_c, v]: lower is last_c; h > lower.mean_cumn, so upper.
                return Some(c.mean);
            }
            last = Some(c);
        }
        last.map(|c| c.mean)
    }

    pub fn as_small_bytes(&self) -> Vec<u8> {
        let size = self.centroids.len();
        let mut out = Vec::with_capacity(16 + size * 5);
        out.extend_from_slice(&SMALL_ENCODING.to_ne_bytes());
        out.extend_from_slice(&self.compression().to_ne_bytes());
        out.extend_from_slice(&(size as u32).to_ne_bytes());
        let mut x = Num::I(0);
        for c in self.centroids.values() {
            let delta = c.mean.sub(x);
            x = c.mean;
            out.extend_from_slice(&(delta.to_f() as f32).to_ne_bytes());
        }
        for c in self.centroids.values() {
            let mut n = c.n;
            let mut k = 0;
            while !(0..=0x7f).contains(&n) {
                out.push((0x80 | (0x7f & n)) as u8);
                n >>= 7;
                k += 1;
                assert!(k <= 6, "Unreasonable large number");
            }
            out.push(n as u8);
        }
        out
    }

    /// `TDigest.from_bytes`: either encoding; centroids are re-pushed in
    /// stored order, which is what the gem does.
    pub fn from_bytes(bytes: &[u8], rng: &mut RubyRandom) -> Result<TDigest, String> {
        let (means, counts, compression) = decode(bytes)?;
        let mut t = TDigest::new(1.0 / compression);
        for (m, c) in means.into_iter().zip(counts) {
            t.push(Num::F(m), c, rng);
        }
        Ok(t)
    }
}

/// The stored centroids (means, weights) and compression, in either of the
/// gem's encodings. Means come back as Ruby would unpack them: doubles of
/// the stored floats, delta-decoded in double precision.
pub fn decode(bytes: &[u8]) -> Result<(Vec<f64>, Vec<i64>, f64), String> {
    if bytes.len() < 16 {
        return Err("t-digest too short".into());
    }
    let u32_at = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap());
    let format = u32_at(0);
    let compression = f64::from_ne_bytes(bytes[4..12].try_into().unwrap());
    let size = u32_at(12) as usize;
    match format {
        VERBOSE_ENCODING => {
            if bytes.len() < 16 + size * 12 {
                return Err("t-digest truncated".into());
            }
            let means = (0..size).map(|i| f64::from_ne_bytes(bytes[16 + 8 * i..24 + 8 * i].try_into().unwrap())).collect();
            let counts = (0..size).map(|i| u32_at(16 + 8 * size + 4 * i) as i64).collect();
            Ok((means, counts, compression))
        }
        SMALL_ENCODING => {
            if bytes.len() < 16 + size * 4 {
                return Err("t-digest truncated".into());
            }
            let mut means = Vec::with_capacity(size);
            let mut x = 0.0f64;
            for i in 0..size {
                let d = f32::from_ne_bytes(bytes[16 + 4 * i..20 + 4 * i].try_into().unwrap()) as f64;
                x += d;
                means.push(x);
            }
            let weights = &bytes[16 + 4 * size..];
            let mut counts = Vec::with_capacity(size);
            let mut at = 0;
            for _ in 0..size {
                let mut byte = *weights.get(at).ok_or("t-digest weights truncated")? as i64;
                at += 1;
                let mut w = byte & 0x7f;
                let mut shift = 7;
                while byte & 0x80 != 0 {
                    byte = weights.get(at).copied().unwrap_or(0) as i64;
                    at += 1;
                    w += (byte & 0x7f) << shift;
                    shift += 7;
                }
                counts.push(w);
            }
            Ok((means, counts, compression))
        }
        other => Err(format!("unknown t-digest encoding {other}")),
    }
}
