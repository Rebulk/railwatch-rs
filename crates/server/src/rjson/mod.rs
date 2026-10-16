//! Ruby-shaped JSON values, and the parts of Ruby's `json` gem (2.21.2, the
//! version railwatch-cloud runs) that the ingest path depends on byte for byte:
//!
//! - `parse`: `JSON.parse` semantics. Strings are byte strings that may hold
//!   invalid UTF-8 (Ruby tags them UTF-8 without validating), integers keep
//!   arbitrary precision, hashes keep insertion order with last-duplicate-wins,
//!   comments are accepted, nesting is capped at 100, and errors carry the
//!   gem's messages.
//! - `generate`: `JSON.generate` (compact, Grisu2 floats via `fpconv`).
//! - `generate_escaped`: what `render json:` produces (Active Support's
//!   encoder: html entities and U+2028/2029 escaped).
//! - `ruby`: Ruby's own conversions (`to_s`, `inspect`, `scrub`, `Float()`),
//!   which leak into stored values and messages.

pub mod fpconv;
mod parse;
pub mod ruby;

use indexmap::IndexMap;

pub use parse::{ParseError, parse};

pub type Hash = IndexMap<Vec<u8>, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    /// An Integer outside i64, kept as its canonical decimal text.
    BigInt(String),
    Float(f64),
    /// A Ruby String tagged UTF-8; the bytes are not guaranteed valid.
    Str(Vec<u8>),
    Array(Vec<Value>),
    Hash(Hash),
}

pub static NIL: Value = Value::Nil;

impl Value {
    /// `hash[key]`, nil when absent or when self is not a Hash.
    pub fn get(&self, key: &str) -> &Value {
        match self {
            Value::Hash(h) => h.get(key.as_bytes()).unwrap_or(&NIL),
            _ => &NIL,
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Ruby truthiness: everything but nil and false.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::BigInt(_) | Value::Float(_))
    }

    /// `a || b`
    pub fn or<'a>(&'a self, other: &'a Value) -> &'a Value {
        if self.truthy() { self } else { other }
    }

    /// Ruby's `Numeric#to_f` for a Numeric value.
    pub fn to_f(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::BigInt(s) => Some(s.parse::<f64>().unwrap_or(f64::INFINITY)),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Ruby's class name, as it appears in messages.
    pub fn class_name(&self) -> &'static str {
        match self {
            Value::Nil => "NilClass",
            Value::Bool(true) => "TrueClass",
            Value::Bool(false) => "FalseClass",
            Value::Int(_) | Value::BigInt(_) => "Integer",
            Value::Float(_) => "Float",
            Value::Str(_) => "String",
            Value::Array(_) => "Array",
            Value::Hash(_) => "Hash",
        }
    }

    /// Active Support's `present?` (the negation of `blank?`).
    pub fn present(&self) -> bool {
        match self {
            Value::Nil | Value::Bool(false) => false,
            Value::Str(s) => !ruby::blank(s),
            Value::Array(a) => !a.is_empty(),
            Value::Hash(h) => !h.is_empty(),
            _ => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenerateError(pub String);

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `JSON.generate(value)`.
pub fn generate(value: &Value) -> Result<String, GenerateError> {
    let mut out = String::with_capacity(64);
    write_value(&mut out, value, 0)?;
    Ok(out)
}

/// What `render json:` (and `#to_json`) produces in Rails: the json gem's
/// output with `<`, `>`, `&`, U+2028 and U+2029 escaped afterwards.
pub fn generate_escaped(value: &Value) -> Result<String, GenerateError> {
    Ok(escape_html(&generate(value)?))
}

pub fn escape_html(json: &str) -> String {
    if !json.bytes().any(|b| matches!(b, b'<' | b'>' | b'&' | 0xE2)) {
        return json.to_string();
    }
    let mut out = String::with_capacity(json.len() + 16);
    for ch in json.chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

const MAX_NESTING: usize = 100;

fn write_value(out: &mut String, value: &Value, depth: usize) -> Result<(), GenerateError> {
    match value {
        Value::Nil => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => {
            use std::fmt::Write;
            let _ = write!(out, "{i}");
        }
        Value::BigInt(s) => out.push_str(s),
        Value::Float(f) => {
            if f.is_nan() {
                return Err(GenerateError("NaN not allowed in JSON".into()));
            }
            if f.is_infinite() {
                let s = if *f > 0.0 { "Infinity" } else { "-Infinity" };
                return Err(GenerateError(format!("{s} not allowed in JSON")));
            }
            fpconv::dtoa(*f, out);
        }
        Value::Str(s) => write_string(out, s)?,
        Value::Array(items) => {
            let depth = depth + 1;
            if depth > MAX_NESTING {
                return Err(GenerateError(format!("nesting of {depth} is too deep")));
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item, depth)?;
            }
            out.push(']');
        }
        Value::Hash(h) => {
            let depth = depth + 1;
            if depth > MAX_NESTING {
                return Err(GenerateError(format!("nesting of {depth} is too deep")));
            }
            out.push('{');
            for (i, (k, v)) in h.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k)?;
                out.push(':');
                write_value(out, v, depth)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// The json gem's basic escaping: `"`, `\` and C0 controls only. `/` and
/// DEL pass through.
fn write_string(out: &mut String, bytes: &[u8]) -> Result<(), GenerateError> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| GenerateError("source sequence is illegal/malformed utf-8".into()))?;
    out.push('"');
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        let esc: Option<&str> = match b {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            0x08 => Some("\\b"),
            0x0C => Some("\\f"),
            0..=0x1F => None,
            _ => continue,
        };
        out.push_str(&s[start..i]);
        match esc {
            Some(e) => out.push_str(e),
            None => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xF) as usize] as char);
            }
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
    Ok(())
}

/// Builds a Hash value from string keys, in order; handy for responses.
#[macro_export]
macro_rules! rhash {
    ($($k:expr => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut h = $crate::rjson::Hash::new();
        $( h.insert($k.as_bytes().to_vec(), $crate::rjson::Value::from($v)); )*
        $crate::rjson::Value::Hash(h)
    }};
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.as_bytes().to_vec())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s.into_bytes())
    }
}
impl From<Vec<u8>> for Value {
    fn from(s: Vec<u8>) -> Value {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}
impl From<usize> for Value {
    fn from(i: usize) -> Value {
        Value::Int(i as i64)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(s: Option<T>) -> Value {
        s.map(Into::into).unwrap_or(Value::Nil)
    }
}
impl From<Vec<Value>> for Value {
    fn from(a: Vec<Value>) -> Value {
        Value::Array(a)
    }
}
