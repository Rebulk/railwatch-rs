//! The bits of Ruby 4.0.0 / Active Support semantics the dashboard props
//! depend on, ported so a page's JSON is byte-for-byte what Rails renders:
//!
//! - `round`: `Float#round(ndigits)` (numeric.c `flo_round` + `round_half_up`),
//!   which is not "format to n places": 2.675.round(2) is 2.68.
//! - `sort_by_num`: `Enumerable#sort_by` when every key is an Integer or a
//!   Float. MRI then sorts with its own *unstable* introsort
//!   (enum.c `rb_uniform_intro_sort_2`), so ties come out in an order only a
//!   faithful port reproduces. Any other key type goes through `ruby_qsort`,
//!   which on glibc 2.41 is a stable mergesort: `sort_by_stable`.
//! - `time_sub`: `Time - Time` as a Float (time.c `rb_time_unmagnify_to_float`).
//! - `t_json`: `Time#as_json` / `TimeWithZone#as_json` (xmlschema(3), UTC "Z").

use std::cmp::Ordering;

/// `Float#round(ndigits)` for ndigits >= 1, as MRI computes it. Returns a
/// Float (ndigits > 0 always does in Ruby).
pub fn round(x: f64, ndigits: i32) -> f64 {
    debug_assert!(ndigits > 0);
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let binexp = frexp_exp(x);
    // float_round_overflow: the number already has fewer digits than asked.
    const FLOAT_DIG: i32 = 15 + 2;
    if ndigits >= FLOAT_DIG - (if binexp > 0 { binexp / 4 } else { binexp / 3 - 1 }) {
        return x;
    }
    // float_round_underflow
    if ndigits < -(if binexp > 0 { binexp / 3 + 1 } else { binexp / 4 }) {
        return 0.0;
    }
    // ACCURATE_POW10: ndigits < DBL_DIG (15); the dashboard never asks for more.
    let s = 10f64.powi(ndigits);
    round_half_up(x, s) / s
}

/// `Float#round` with no digits: an Integer.
pub fn round0(x: f64) -> i64 {
    x.round() as i64
}

fn round_half_up(x: f64, s: f64) -> f64 {
    let xs = x * s;
    let mut f = xs.round(); // C round(): half away from zero, like Rust's
    if s == 1.0 {
        return f;
    }
    if x > 0.0 {
        if (f + 0.5) / s <= x {
            f += 1.0;
        }
    } else if (f - 0.5) / s >= x {
        f -= 1.0;
    }
    f
}

/// The exponent frexp() returns: x = m * 2^e with 0.5 <= |m| < 1.
fn frexp_exp(x: f64) -> i32 {
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        // subnormal
        let m = bits & ((1u64 << 52) - 1);
        return -1022 - (m.leading_zeros() as i32 - 12) ;
    }
    exp - 1022
}

/// A sort key MRI's uniform sort accepts: a Fixnum or a Float.
#[derive(Debug, Clone, Copy)]
pub enum Num {
    I(i64),
    F(f64),
}

impl From<i64> for Num {
    fn from(v: i64) -> Num {
        Num::I(v)
    }
}
impl From<f64> for Num {
    fn from(v: f64) -> Num {
        Num::F(v)
    }
}

/// rb_float_cmp(a, b) < 0 etc.: a Float against an Integer compares exactly.
fn float_cmp_int(a: f64, b: i64) -> Ordering {
    if a.is_nan() {
        return Ordering::Greater; // never reached by the dashboard's keys
    }
    // Exact: compare via i128 when a is integral and in range, else by floor.
    let fl = a.floor();
    if fl < -9.3e18 {
        return Ordering::Less;
    }
    if fl > 9.3e18 {
        return Ordering::Greater;
    }
    let fi = fl as i128;
    match fi.cmp(&(b as i128)) {
        Ordering::Equal => {
            if a > fl {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        o => o,
    }
}

fn num_cmp(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::I(x), Num::I(y)) => x.cmp(&y),
        (Num::F(x), Num::I(y)) => float_cmp_int(x, y),
        (Num::I(x), Num::F(y)) => float_cmp_int(y, x).reverse(),
        (Num::F(x), Num::F(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
    }
}

fn less(a: Num, b: Num) -> bool {
    num_cmp(a, b) == Ordering::Less
}

/// `items.sort_by { key }` where every key is an Integer or a Float: a port
/// of MRI 4.0.0's rb_uniform_intro_sort_2, including its tie order.
pub fn sort_by_num<T>(items: Vec<T>, mut key: impl FnMut(&T) -> Num) -> Vec<T> {
    let mut v: Vec<(Num, usize)> = items.iter().enumerate().map(|(i, t)| (key(t), i)).collect();
    // enum.c sorts when RARRAY_LEN(ary) > 2, and `ary` holds a [value,
    // item] pair per element: two or more elements are sorted.
    if v.len() >= 2 {
        intro_sort(&mut v);
    }
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    v.into_iter().map(|(_, i)| slots[i].take().unwrap()).collect()
}

fn intro_sort(v: &mut [(Num, usize)]) {
    let n = v.len();
    let d = (usize::BITS - 1 - n.leading_zeros()) as usize;
    if v.windows(2).all(|w| num_cmp(w[0].0, w[1].0) != Ordering::Greater) {
        return;
    }
    quicksort_intro(v, d << 1);
}

fn insertion_sort(v: &mut [(Num, usize)]) {
    if v.len() < 2 {
        return;
    }
    for index in 1..v.len() {
        let tmp = v[index];
        let mut j = index;
        if less(tmp.0, v[0].0) {
            while j > 0 {
                v[j] = v[j - 1];
                j -= 1;
            }
        } else {
            while less(tmp.0, v[j - 1].0) {
                v[j] = v[j - 1];
                j -= 1;
            }
        }
        v[j] = tmp;
    }
}

fn heap_down(v: &mut [(Num, usize)], mut offset: usize, len: usize) {
    let tmp = v[offset];
    loop {
        let mut c = (offset << 1) + 1;
        if c > len {
            break;
        }
        if c < len && less(v[c].0, v[c + 1].0) {
            c += 1;
        }
        if !less(tmp.0, v[c].0) {
            break;
        }
        v[offset] = v[c];
        offset = c;
    }
    v[offset] = tmp;
}

fn heapsort(v: &mut [(Num, usize)]) {
    let n = v.len();
    if n < 2 {
        return;
    }
    let mut offset = n >> 1;
    while offset > 0 {
        offset -= 1;
        heap_down(v, offset, n - 1);
    }
    let mut offset = n - 1;
    while offset > 0 {
        v.swap(0, offset);
        offset -= 1;
        heap_down(v, 0, offset);
    }
}

fn med3(a: Num, b: Num, c: Num) -> Num {
    if less(a, b) {
        if less(b, c) {
            b
        } else if less(c, a) {
            a
        } else {
            c
        }
    } else if less(c, b) {
        b
    } else if less(a, c) {
        a
    } else {
        c
    }
}

fn quicksort_intro(v: &mut [(Num, usize)], d: usize) {
    let n = v.len();
    if n <= 16 {
        insertion_sort(v);
        return;
    }
    if d == 0 {
        heapsort(v);
        return;
    }
    let x = med3(v[0].0, v[n >> 1].0, v[n - 1].0);
    let mut i: isize = 0;
    let mut j: isize = n as isize - 1;
    loop {
        while less(v[i as usize].0, x) {
            i += 1;
        }
        while less(x, v[j as usize].0) {
            j -= 1;
        }
        if i <= j {
            v.swap(i as usize, j as usize);
            i += 1;
            j -= 1;
        }
        if i > j {
            break;
        }
    }
    let j = (j + 1) as usize;
    let i = i as usize;
    if n - j > 1 {
        quicksort_intro(&mut v[j..], d - 1);
    }
    if i > 1 {
        quicksort_intro(&mut v[..i], d - 1);
    }
}

/// `sort_by` with keys that are not all Integer/Float (arrays, strings,
/// times): ruby_qsort is glibc's qsort_r, a stable mergesort.
pub fn sort_by_stable<T>(items: &mut [T], cmp: impl FnMut(&T, &T) -> Ordering) {
    items.sort_by(cmp);
}

/// `enum.max_by { }`: the first element with the greatest key.
pub fn max_by<T, K: PartialOrd>(items: &[T], mut key: impl FnMut(&T) -> K) -> Option<&T> {
    let mut best: Option<(&T, K)> = None;
    for t in items {
        let k = key(t);
        match &best {
            Some((_, bk)) if k.partial_cmp(bk) != Some(std::cmp::Ordering::Greater) => {}
            _ => best = Some((t, k)),
        }
    }
    best.map(|(t, _)| t)
}

/// `Time - Time` in seconds, as a Float, for times held as epoch micros.
pub fn time_sub(a_us: i64, b_us: i64) -> f64 {
    let ns = (a_us as i128 - b_us as i128) * 1000;
    if ns % 1_000_000_000 == 0 {
        (ns / 1_000_000_000) as f64
    } else {
        ns as f64 / 1e9
    }
}

/// `Time#as_json` for a UTC time: "2026-10-13T12:00:00.123Z" (ms truncated).
pub fn t_json(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let ms = micros.rem_euclid(1_000_000) / 1000;
    format!("{}.{:03}Z", crate::time::utc_seconds_string(secs).replacen(' ', "T", 1), ms)
}

/// `Time#as_json` for a non-UTC-flagged Time at offset 0 (a plain
/// `Time` from `to_t`): "2026-10-14T01:15:00.000+00:00".
pub fn t_json_offset(micros: i64) -> String {
    t_json(micros).replace('Z', "+00:00")
}

/// `TimeWithZone#iso8601(6)` / `Time#iso8601(6)` in UTC.
pub fn iso8601_6(micros: i64) -> String {
    crate::time::iso8601_6(micros)
}

/// `str.first(n)`: the first n characters (not bytes).
pub fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Active Support `beginning_of_hour` for epoch micros.
pub fn beginning_of_hour(micros: i64) -> i64 {
    micros.div_euclid(3_600_000_000) * 3_600_000_000
}

/// Active Support `beginning_of_day` (UTC) for epoch micros.
pub fn beginning_of_day(micros: i64) -> i64 {
    micros.div_euclid(86_400_000_000) * 86_400_000_000
}

/// `Time#to_i`.
pub fn to_i(micros: i64) -> i64 {
    micros.div_euclid(1_000_000)
}

/// `Object#present?` for an optional string.
pub fn present(s: Option<&str>) -> bool {
    s.is_some_and(|s| !s.trim_matches(|c: char| c.is_whitespace() || c == '\u{0}').is_empty())
}

/// `ActiveRecord::Base.sanitize_sql_like(s)` with the default escape "\".
pub fn sanitize_sql_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_round_matches_ruby() {
        // Values from `ruby -e` in the rw-baseline container.
        assert_eq!(round(1.005, 2), 1.01);
        assert_eq!(round(2.675, 2), 2.68);
        assert_eq!(round(12.345, 2), 12.35);
        assert_eq!(round(0.125, 2), 0.13);
        assert_eq!(round(-0.125, 2), -0.13);
        assert_eq!(round(1.23456789, 3), 1.235);
        assert_eq!(round(1e20, 2), 1e20);
    }

    #[test]
    fn uniform_sort_reproduces_mri_tie_order() {
        // `(0...n).map { |i| [i, i % 3] }.sort_by { -k }`: the first indices
        // with key 2, as the baseline's Ruby 4.0.0 printed them.
        // Two elements are sorted too (the pair array has 4 slots).
        let two = sort_by_num(vec![("a", 1i64), ("b", 3)], |&(_, k)| Num::I(-k));
        assert_eq!(two[0].0, "b");
        let cases: [(usize, &[usize]); 6] = [
            (3, &[2]),
            (7, &[2, 5]),
            (16, &[2, 5, 8, 11, 14]),
            (50, &[32, 29, 14, 35, 20, 11, 38, 17]),
            (200, &[74, 53, 146, 116, 50, 149, 83, 47]),
            (2000, &[353, 1520, 479, 839, 1517, 482, 1160, 1514]),
        ];
        for (n, expected) in cases {
            let items: Vec<(usize, i64)> = (0..n).map(|i| (i, (i % 3) as i64)).collect();
            let sorted = sort_by_num(items, |&(_, k)| Num::I(-k));
            let ties: Vec<usize> = sorted.iter().filter(|(_, k)| *k == 2).take(8).map(|(i, _)| *i).collect();
            assert_eq!(ties, expected, "n={n}");
        }
    }

    #[test]
    fn time_sub_is_rubys_float() {
        assert_eq!(time_sub(2_000_000, 1_000_000), 1.0);
        assert_eq!(time_sub(1_500_123, 0), 1.500123);
    }

    #[test]
    fn json_time() {
        assert_eq!(t_json(1_767_225_600_123_999), "2026-01-01T00:00:00.123Z");
    }
}
