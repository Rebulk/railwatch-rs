//! Port of Ingest::Payload (railwatch-cloud app/models/ingest/payload.rb):
//! the gem's wire format, optionally gzipped NDJSON, decoded with the same
//! limits and the same error messages (they reach the sender as
//! `400 {"error": "..."}`).

use std::io::{self, Read};

use flate2::{Decompress, FlushDecompress, Status};

use crate::rjson::{self, Value};

/// IngestRequestBodyLimit::MAX_BYTES, compressed and decompressed.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RECORDS: usize = 20_000;
/// Longest NDJSON line accepted (Rust-only; Rails has no cap). The gem's
/// largest record is far smaller; without a cap a newline-free body is one
/// 32 MiB line that is buffered and handed to the parser whole.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const READ_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Malformed(pub String);

pub fn gzip(content_encoding: Option<&str>) -> bool {
    content_encoding.is_some_and(|v| v.contains("gzip"))
}

/// Payload.parse_bytes.
pub fn parse_bytes(raw: &[u8], gzip: bool) -> Result<Vec<Value>, Malformed> {
    if blank(raw) {
        return Err(Malformed("empty body".into()));
    }
    let mut records = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut decoded = 0usize;
    let mut on_chunk = |chunk: &[u8], records: &mut Vec<Value>| -> Result<(), Malformed> {
        decoded += chunk.len();
        if decoded > MAX_BYTES {
            return Err(Malformed("payload too large".into()));
        }
        // `pending` holds no newline (consume_complete_lines drained up to the
        // last one), so only the new chunk needs scanning: each byte once.
        let scan_from = pending.len();
        pending.extend_from_slice(chunk);
        consume_complete_lines(&mut pending, scan_from, records)?;
        if pending.len() > MAX_LINE_BYTES {
            return Err(Malformed("line too long".into()));
        }
        Ok(())
    };
    if gzip {
        let mut reader = GzipMember::new(raw);
        let mut buf = vec![0u8; READ_SIZE];
        let mut remaining = MAX_BYTES + 1;
        loop {
            let want = READ_SIZE.min(remaining);
            if want == 0 {
                break;
            }
            let n = reader.read(&mut buf[..want]).map_err(|e| Malformed(format!("bad gzip: {e}")))?;
            if n == 0 {
                break;
            }
            remaining -= n;
            on_chunk(&buf[..n], &mut records)?;
        }
    } else {
        for chunk in raw.chunks(READ_SIZE) {
            on_chunk(chunk, &mut records)?;
        }
    }
    append_record(&pending, &mut records)?;
    Ok(records)
}

/// `raw.blank?` on a binary string: empty or only ASCII whitespace.
fn blank(raw: &[u8]) -> bool {
    raw.iter().all(|b| matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r'))
}

/// Appends every complete line in `buffer`, searching for newlines from
/// `scan_from` (everything before it is known to hold none).
fn consume_complete_lines(buffer: &mut Vec<u8>, scan_from: usize, records: &mut Vec<Value>) -> Result<(), Malformed> {
    let mut offset = 0;
    let mut search = scan_from;
    while let Some(nl) = buffer[search..].iter().position(|&b| b == b'\n') {
        let end = search + nl;
        if end - offset > MAX_LINE_BYTES {
            return Err(Malformed("line too long".into()));
        }
        append_record(&buffer[offset..end], records)?;
        offset = end + 1;
        search = offset;
    }
    if offset > 0 {
        buffer.drain(..offset);
    }
    Ok(())
}

/// String#strip: leading and trailing NUL and ASCII whitespace.
fn strip(line: &[u8]) -> &[u8] {
    let ws = |b: &u8| matches!(b, 0 | b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let start = line.iter().position(|b| !ws(b)).unwrap_or(line.len());
    let end = line.iter().rposition(|b| !ws(b)).map(|i| i + 1).unwrap_or(start);
    &line[start..end.max(start)]
}

fn append_record(line: &[u8], records: &mut Vec<Value>) -> Result<(), Malformed> {
    let line = strip(line);
    if line.is_empty() {
        return Ok(());
    }
    if records.len() >= MAX_RECORDS {
        return Err(Malformed("too many records".into()));
    }
    // JSON::NestingError is a ParserError subclass: also "bad json: ...".
    let value = rjson::parse(line).map_err(|e| Malformed(format!("bad json: {}", e.0)))?;
    records.push(value);
    Ok(())
}

/// Reads the first member of a gzip stream as Ruby's Zlib::GzipReader does,
/// with its error messages. Data after the member is left unread
/// (`trailing()` says whether there was any).
pub struct GzipMember<'a> {
    src: &'a [u8],
    pos: usize,
    inflate: Decompress,
    header_done: bool,
    finished: bool,
    crc: flate2::Crc,
}

fn zerr(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

impl<'a> GzipMember<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        GzipMember { src, pos: 0, inflate: Decompress::new(false), header_done: false, finished: false, crc: flate2::Crc::new() }
    }

    /// Bytes remain after the member (BoundedGzip refuses them).
    pub fn trailing(&self) -> bool {
        self.finished && self.pos < self.src.len()
    }

    /// gzfile_read_header.
    fn read_header(&mut self) -> io::Result<()> {
        let s = self.src;
        if s.len() < 10 || s[0] != 0x1f || s[1] != 0x8b {
            return Err(zerr("not in gzip format"));
        }
        if s[2] != 8 {
            return Err(zerr(format!("unsupported compression method {}", s[2])));
        }
        let flags = s[3];
        if flags & 0x02 != 0 {
            return Err(zerr("multi-part gzip file is not supported"));
        }
        if flags & 0x20 != 0 {
            return Err(zerr("encrypted gzip file is not supported"));
        }
        if flags & 0xc0 != 0 {
            return Err(zerr(format!("unknown flags 0x{flags:02x}")));
        }
        let mut pos = 10;
        let eof = || zerr("unexpected end of file");
        if flags & 0x04 != 0 {
            if s.len() < pos + 2 {
                return Err(eof());
            }
            let len = s[pos] as usize | (s[pos + 1] as usize) << 8;
            pos += 2;
            if s.len() < pos + len {
                return Err(eof());
            }
            pos += len;
        }
        for flag in [0x08u8, 0x10] {
            if flags & flag != 0 {
                match s[pos..].iter().position(|&b| b == 0) {
                    Some(n) => pos += n + 1,
                    None => return Err(eof()),
                }
            }
        }
        if flags & 0x01 != 0 {
            pos += 2; // header CRC (Ruby skips it too)
        }
        self.pos = pos.min(s.len());
        self.header_done = true;
        Ok(())
    }

    fn check_footer(&mut self) -> io::Result<()> {
        let rest = &self.src[self.pos.min(self.src.len())..];
        if rest.len() < 8 {
            return Err(zerr("footer is not found"));
        }
        let crc = u32::from_le_bytes(rest[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(rest[4..8].try_into().unwrap());
        if crc != self.crc.sum() {
            return Err(zerr("invalid compressed data -- crc error"));
        }
        if len != self.crc.amount() {
            return Err(zerr("invalid compressed data -- length error"));
        }
        self.pos += 8;
        Ok(())
    }
}

impl Read for GzipMember<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.header_done {
            self.read_header()?;
        }
        if self.finished || out.is_empty() {
            return Ok(0);
        }
        loop {
            let input = &self.src[self.pos..];
            let before_in = self.inflate.total_in();
            let before_out = self.inflate.total_out();
            let status = self
                .inflate
                .decompress(input, out, FlushDecompress::None)
                .map_err(|e| zerr(e.message().unwrap_or("invalid compressed data").to_string()))?;
            let consumed = (self.inflate.total_in() - before_in) as usize;
            let produced = (self.inflate.total_out() - before_out) as usize;
            self.pos += consumed;
            self.crc.update(&out[..produced]);
            if status == Status::StreamEnd {
                self.check_footer()?;
                self.finished = true;
                return Ok(produced);
            }
            if produced > 0 {
                return Ok(produced);
            }
            if consumed == 0 && self.pos >= self.src.len() {
                return Err(zerr("unexpected end of file"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn errors_match_ruby_zlib() {
        let good = gz(&b"{\"t\":\"x\"}\n".repeat(3));
        let err = |b: &[u8]| parse_bytes(b, true).unwrap_err().0;
        assert_eq!(err(&good[..good.len() - 4]), "bad gzip: footer is not found");
        assert_eq!(err(&good[..15]), "bad gzip: unexpected end of file");
        assert_eq!(err(b"hello"), "bad gzip: not in gzip format");
        assert_eq!(err(b"\x1f\x8b"), "bad gzip: not in gzip format");
        let mut crc = good.clone();
        let n = crc.len();
        crc[n - 6] ^= 1;
        assert_eq!(err(&crc), "bad gzip: invalid compressed data -- crc error");
        let mut len = good.clone();
        len[n - 2] ^= 1;
        assert_eq!(err(&len), "bad gzip: invalid compressed data -- length error");
        let mut method = good.clone();
        method[2] = 7;
        assert_eq!(err(&method), "bad gzip: unsupported compression method 7");
        let mut flags = good.clone();
        flags[3] = 0xff;
        assert_eq!(err(&flags), "bad gzip: multi-part gzip file is not supported");
        // Data after the first member is ignored, as GzipReader does.
        let mut two = good.clone();
        two.extend_from_slice(&good);
        assert_eq!(parse_bytes(&two, true).unwrap().len(), 3);
    }

    #[test]
    fn lines() {
        assert_eq!(parse_bytes(b" \n\t", false).unwrap_err().0, "empty body");
        assert_eq!(parse_bytes(b"\0{\"a\":1}\0\n\r\n", false).unwrap().len(), 1);
        assert_eq!(parse_bytes(b"\x0c{\"a\":1}\x0b\n", false).unwrap().len(), 1);
        assert_eq!(
            parse_bytes(b"{\"a\":1}\r{\"b\":2}\n", false).unwrap_err().0,
            "bad json: unexpected token at end of stream '{\"b\":2}' at line 1 column 9"
        );
        assert_eq!(parse_bytes(b"\"x\"\n1\nnull\n[1]\n", false).unwrap().len(), 4);
        let many = b"{}\n".repeat(MAX_RECORDS + 1);
        assert_eq!(parse_bytes(&many, false).unwrap_err().0, "too many records");
    }

    /// A line over MAX_LINE_BYTES is refused whether or not it ends in a
    /// newline, gzipped or not; a line exactly at the cap is accepted.
    #[test]
    fn line_length_cap() {
        let at_cap = |n: usize| {
            let mut l = b"\"".to_vec();
            l.resize(n - 1, b'a');
            l.push(b'"');
            l
        };
        let ok = at_cap(MAX_LINE_BYTES);
        assert_eq!(parse_bytes(&ok, false).unwrap().len(), 1);
        let mut ok_nl = ok.clone();
        ok_nl.extend_from_slice(b"\n{}\n");
        assert_eq!(parse_bytes(&ok_nl, false).unwrap().len(), 2);
        let over = at_cap(MAX_LINE_BYTES + 1);
        assert_eq!(parse_bytes(&over, false).unwrap_err().0, "line too long");
        let mut over_nl = over.clone();
        over_nl.push(b'\n');
        assert_eq!(parse_bytes(&over_nl, false).unwrap_err().0, "line too long");
        assert_eq!(parse_bytes(&gz(&over_nl), true).unwrap_err().0, "line too long");
    }
}
