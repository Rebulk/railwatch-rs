//! Port of Railwatch::Telemetry::BoundedGzip.verify! (gem 0.9.x): inflate an
//! untrusted gzip blob without ever producing more than `max_bytes`, check the
//! declared size, optionally check it is UTF-8, and refuse trailing data.
//! Messages match Ruby's Zlib (which is C zlib, as flate2's `zlib` backend is).

use std::io::Read;

#[derive(Debug, Clone, PartialEq)]
pub enum GzipError {
    Malformed(String),
    TooLarge(String),
    SizeMismatch(String),
}

impl GzipError {
    /// The Ruby class, as it appears in a rejection reason.
    pub fn class(&self) -> &'static str {
        match self {
            GzipError::Malformed(_) => "Railwatch::Telemetry::BoundedGzip::Malformed",
            GzipError::TooLarge(_) => "Railwatch::Telemetry::BoundedGzip::TooLarge",
            GzipError::SizeMismatch(_) => "Railwatch::Telemetry::BoundedGzip::SizeMismatch",
        }
    }
}

impl std::fmt::Display for GzipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GzipError::Malformed(m) | GzipError::TooLarge(m) | GzipError::SizeMismatch(m) => f.write_str(m),
        }
    }
}

pub fn verify(compressed: &[u8], max_bytes: u64, expected: i64, utf8: bool) -> Result<u64, GzipError> {
    if !(0..=max_bytes as i64).contains(&expected) {
        return Err(GzipError::SizeMismatch(format!("declared size {expected} exceeds the {max_bytes} byte ceiling")));
    }
    let mut out = Vec::new();
    let total = inflate_bounded(compressed, max_bytes, &mut out)?;
    if utf8 && std::str::from_utf8(&out).is_err() {
        return Err(GzipError::Malformed("decompressed content is not valid UTF-8".into()));
    }
    if total == expected as u64 {
        Ok(total)
    } else {
        Err(GzipError::SizeMismatch(format!("declared size {expected} does not match decompressed size {total}")))
    }
}

/// One gzip member, bounded, trailing data refused (as each_chunk does).
fn inflate_bounded(compressed: &[u8], max_bytes: u64, out: &mut Vec<u8>) -> Result<u64, GzipError> {
    let mut gz = crate::ingest::payload::GzipMember::new(compressed);
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = gz.read(&mut buf).map_err(|e| GzipError::Malformed(format!("invalid gzip stream: {e}")))?;
        if n == 0 {
            break;
        }
        if total + n as u64 > max_bytes {
            return Err(GzipError::TooLarge(format!("decompressed content exceeds {max_bytes} bytes")));
        }
        total += n as u64;
        out.extend_from_slice(&buf[..n]);
    }
    if gz.trailing() {
        return Err(GzipError::Malformed("gzip stream has trailing data".into()));
    }
    Ok(total)
}
