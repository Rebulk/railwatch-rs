//! Ruby core conversions the reference leans on implicitly: `to_s`/`inspect`
//! (a String column given a Hash stores its inspect), `Float#to_s`, `Float()`,
//! `String#scrub`, Active Support's `blank?`, and `Time.at(float).usec`.

use super::Value;

/// `value.to_s`
pub fn to_s(v: &Value) -> Vec<u8> {
    match v {
        Value::Nil => Vec::new(),
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string().into_bytes(),
        Value::Int(i) => i.to_string().into_bytes(),
        Value::BigInt(s) => s.clone().into_bytes(),
        Value::Float(f) => float_to_s(*f).into_bytes(),
        Value::Array(_) | Value::Hash(_) => inspect(v),
    }
}

/// `value.inspect`
pub fn inspect(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_inspect(&mut out, v);
    out
}

fn write_inspect(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Nil => out.extend_from_slice(b"nil"),
        Value::Str(s) => inspect_str(out, s),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                write_inspect(out, item);
            }
            out.push(b']');
        }
        Value::Hash(h) => {
            out.push(b'{');
            for (i, (k, val)) in h.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                inspect_str(out, k);
                out.extend_from_slice(b" => ");
                write_inspect(out, val);
            }
            out.push(b'}');
        }
        other => out.extend_from_slice(&to_s(other)),
    }
}

/// String#inspect for a UTF-8 tagged string. Non-printable is approximated
/// as Cc, surrogates, U+2028/2029 and noncharacters; Ruby also escapes
/// unassigned code points, for which Rust has no table (docs/DIFFERENCES.md).
fn inspect_str(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    let mut i = 0;
    while i < s.len() {
        match next_char(&s[i..]) {
            None => {
                out.extend_from_slice(format!("\\x{:02X}", s[i]).as_bytes());
                i += 1;
            }
            Some((c, len)) => {
                let cp = c as u32;
                match c {
                    '"' => out.extend_from_slice(b"\\\""),
                    '\\' => out.extend_from_slice(b"\\\\"),
                    '\n' => out.extend_from_slice(b"\\n"),
                    '\r' => out.extend_from_slice(b"\\r"),
                    '\t' => out.extend_from_slice(b"\\t"),
                    '\u{0C}' => out.extend_from_slice(b"\\f"),
                    '\u{0B}' => out.extend_from_slice(b"\\v"),
                    '\u{08}' => out.extend_from_slice(b"\\b"),
                    '\u{07}' => out.extend_from_slice(b"\\a"),
                    '\u{1B}' => out.extend_from_slice(b"\\e"),
                    '#' if matches!(s.get(i + 1), Some(b'{' | b'$' | b'@')) => out.extend_from_slice(b"\\#"),
                    _ if nonprintable(cp) => {
                        if cp > 0xFFFF {
                            out.extend_from_slice(format!("\\u{{{cp:X}}}").as_bytes());
                        } else {
                            out.extend_from_slice(format!("\\u{cp:04X}").as_bytes());
                        }
                    }
                    _ => out.extend_from_slice(&s[i..i + len]),
                }
                i += len;
            }
        }
    }
    out.push(b'"');
}

fn nonprintable(cp: u32) -> bool {
    cp < 0x20
        || (0x7F..=0x9F).contains(&cp)
        || cp == 0x2028
        || cp == 0x2029
        || (0xFDD0..=0xFDEF).contains(&cp)
        || (cp & 0xFFFE) == 0xFFFE
}

/// Decodes one UTF-8 char (also accepting encoded surrogates, which Ruby's
/// JSON parser can produce and which Ruby then treats as invalid bytes).
fn next_char(s: &[u8]) -> Option<(char, usize)> {
    let len = match s[0] {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return None,
    };
    let chunk = s.get(..len)?;
    std::str::from_utf8(chunk).ok().and_then(|t| t.chars().next()).map(|c| (c, len))
}

/// Float#to_s: shortest round-trip digits; fixed notation when there is a
/// fraction to show or the integer has at most 15 digits, and for
/// -4 < decpt <= 0; otherwise `d.ddde+XX` (as measured against Ruby 4.0).
pub fn float_to_s(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` gives the shortest round-trip digits, as Ruby's dtoa mode 0.
    // Rust's `{:e}` gives the shortest round-trip digit count; re-render at
    // that precision so an exact tie rounds half-to-even, as Ruby's dtoa does
    // (962909363100483.25 -> ...483.2).
    let shortest = format!("{:e}", f.abs());
    let ndigits = shortest.split_once('e').unwrap().0.chars().filter(|c| c.is_ascii_digit()).count();
    let sci = format!("{:.*e}", ndigits - 1, f.abs());
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    let mut out = String::new();
    if f < 0.0 {
        out.push('-');
    }
    let nd = digits.len() as i32;
    if decpt > 0 && decpt < nd {
        out.push_str(&digits[..decpt as usize]);
        out.push('.');
        out.push_str(&digits[decpt as usize..]);
    } else if decpt > 0 && decpt <= 15 {
        out.push_str(&digits);
        for _ in 0..(decpt - nd) {
            out.push('0');
        }
        out.push_str(".0");
    } else if decpt <= 0 && decpt > -4 {
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        if nd > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('e');
        let e = decpt - 1;
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    }
    out
}

/// Kernel#Float(value). Errors are (class, message).
pub fn float(v: &Value) -> Result<f64, (&'static str, String)> {
    match v {
        Value::Int(_) | Value::BigInt(_) | Value::Float(_) => Ok(v.to_f().unwrap()),
        Value::Str(s) => parse_float_str(s).ok_or_else(|| {
            if s.contains(&0) {
                ("ArgumentError", "string for Float contains null byte".to_string())
            } else {
                (
                    "ArgumentError",
                    format!("invalid value for Float(): {}", String::from_utf8_lossy(&inspect(v))),
                )
            }
        }),
        Value::Nil => Err(("TypeError", "can't convert nil into Float".into())),
        Value::Bool(b) => Err(("TypeError", format!("can't convert {b} into Float"))),
        Value::Array(_) => Err(("TypeError", "can't convert Array into Float".into())),
        Value::Hash(_) => Err(("TypeError", "can't convert Hash into Float".into())),
    }
}

/// The strict `Float("...")` grammar: optional surrounding whitespace, sign,
/// digits with single underscores between them, optional fraction and
/// exponent; or a 0x hexadecimal integer.
fn parse_float_str(s: &[u8]) -> Option<f64> {
    let is_ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let start = s.iter().position(|b| !is_ws(b))?;
    let end = s.iter().rposition(|b| !is_ws(b))? + 1;
    let t = &s[start..end];
    let (neg, body) = match t.first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if body.len() > 2 && (body.starts_with(b"0x") || body.starts_with(b"0X")) {
        let hex = digits_with_underscores(&body[2..], |b| b.is_ascii_hexdigit())?;
        let v = u128::from_str_radix(&hex, 16).ok()? as f64;
        return Some(if neg { -v } else { v });
    }
    let mut i = 0;
    let mut text = String::new();
    let int_len = run(body, &mut i, |b| b.is_ascii_digit() || b == b'_');
    let int_part = &body[..int_len];
    if !int_part.is_empty() {
        text.push_str(&digits_with_underscores(int_part, |b| b.is_ascii_digit())?);
    }
    if body.get(i) == Some(&b'.') {
        i += 1;
        let f_start = i;
        let n = run(body, &mut i, |b| b.is_ascii_digit() || b == b'_');
        if n > 0 {
            text.push('.');
            text.push_str(&digits_with_underscores(&body[f_start..i], |b| b.is_ascii_digit())?);
        }
        if int_part.is_empty() && n == 0 {
            return None;
        }
    } else if int_part.is_empty() {
        return None;
    }
    if matches!(body.get(i), Some(b'e' | b'E')) {
        i += 1;
        text.push('e');
        if let Some(&c @ (b'-' | b'+')) = body.get(i) {
            text.push(c as char);
            i += 1;
        }
        let e_start = i;
        let n = run(body, &mut i, |b| b.is_ascii_digit() || b == b'_');
        if n == 0 {
            return None;
        }
        text.push_str(&digits_with_underscores(&body[e_start..i], |b| b.is_ascii_digit())?);
    }
    if i != body.len() {
        return None;
    }
    if text.starts_with('.') {
        text.insert(0, '0');
    }
    let v: f64 = text.parse().ok()?;
    Some(if neg { -v } else { v })
}

fn run(s: &[u8], i: &mut usize, ok: impl Fn(u8) -> bool) -> usize {
    let start = *i;
    while *i < s.len() && ok(s[*i]) {
        *i += 1;
    }
    *i - start
}

fn digits_with_underscores(s: &[u8], digit: impl Fn(u8) -> bool) -> Option<String> {
    if s.is_empty() || s[0] == b'_' || s[s.len() - 1] == b'_' {
        return None;
    }
    let mut out = String::new();
    let mut prev_us = false;
    for &b in s {
        if b == b'_' {
            if prev_us {
                return None;
            }
            prev_us = true;
        } else if digit(b) {
            prev_us = false;
            out.push(b as char);
        } else {
            return None;
        }
    }
    Some(out)
}

/// String#scrub: each maximal invalid subsequence becomes U+FFFD (Rust's
/// lossy decoding follows the same maximal-subpart rule as Ruby).
pub fn scrub(s: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(s).into_owned().into_bytes()
}

pub fn valid_utf8(s: &[u8]) -> bool {
    std::str::from_utf8(s).is_ok()
}

/// Active Support's `blank?` for a String: empty or only [[:space:]]
/// (Unicode White_Space). An invalid string is never blank-matched; Ruby
/// raises there, which no caller reaches with one.
pub fn blank(s: &[u8]) -> bool {
    match std::str::from_utf8(s) {
        Ok(t) => t.chars().all(|c| {
            matches!(c as u32, 0x09..=0x0D | 0x20 | 0x85 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
        }),
        Err(_) => false,
    }
}

/// `string.first(n)`: the first n characters (invalid bytes count as one each).
pub fn first_chars(s: &[u8], n: usize) -> Vec<u8> {
    let mut i = 0;
    let mut count = 0;
    while i < s.len() && count < n {
        i += next_char(&s[i..]).map(|(_, l)| l).unwrap_or(1);
        count += 1;
    }
    s[..i].to_vec()
}

/// `Time.at(float).usec` and the whole seconds, from the float's exact value
/// (Ruby keeps the exact rational, so 1767225600.123 has usec 122999).
pub fn time_at_float(f: f64) -> (i64, u32) {
    let secs = f.floor();
    let frac = f - secs; // exact: both share the float's binade
    let bits = frac.to_bits();
    if frac == 0.0 {
        return (secs as i64, 0);
    }
    let exp = ((bits >> 52) & 0x7FF) as i32;
    let mant = if exp == 0 { (bits & ((1 << 52) - 1)) << 1 } else { (bits & ((1 << 52) - 1)) | (1 << 52) };
    let shift = 1075 - exp; // frac = mant / 2^shift
    let usec = if shift >= 107 {
        0
    } else {
        ((mant as u128 * 1_000_000u128) >> shift) as u32
    };
    (secs as i64, usec)
}
