// SPDX-License-Identifier: AGPL-3.0-only AND BSD-2-Clause
//
// Follows Ruby's json gem 2.21.2 parser (ext/json/ext/parser/parser.c) and
// its error messages, Copyright (C) 1993-2013 Yukihiro Matsumoto and the
// json gem authors, 2-clause BSD license (THIRD-PARTY-LICENSES, "Ruby").

//! `JSON.parse` as Ruby's json 2.21.2 does it (ext/json/ext/parser/parser.c),
//! including its error messages, because those reach the sender verbatim as
//! `{"error":"bad json: ..."}`. Default options: max_nesting 100, comments
//! allowed, NaN/Infinity refused, control characters in strings refused, no
//! UTF-8 validation of raw bytes.

use super::{Hash, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

const MAX_NESTING: usize = 100;
const FRAGMENT_LEN: usize = 32;

struct Parser<'a> {
    src: &'a [u8],
    cur: usize,
    nesting: usize,
}

enum Frame {
    Array(Vec<Value>),
    Object(Hash, Option<Vec<u8>>),
}

pub fn parse(src: &[u8]) -> Result<Value, ParseError> {
    let mut p = Parser { src, cur: 0, nesting: 0 };
    let value = p.parse_any()?;
    p.eat_whitespace()?;
    if p.cur < p.src.len() {
        return Err(p.error("unexpected token at end of stream %s"));
    }
    Ok(value)
}

impl<'a> Parser<'a> {
    fn peek(&self) -> u8 {
        if self.cur < self.src.len() { self.src[self.cur] } else { 0 }
    }

    fn eos(&self) -> bool {
        self.cur >= self.src.len()
    }

    /// build_parse_error_message + " at line L column C".
    fn error(&self, format: &str) -> ParseError {
        self.error_at(format, self.cur)
    }

    fn error_at(&self, format: &str, at: usize) -> ParseError {
        let fragment: Vec<u8> = if at < self.src.len() {
            let rest = &self.src[at..];
            let mut len = 0;
            while len < FRAGMENT_LEN && len < rest.len() {
                let ch = rest[len];
                if ch == 0 || ch == b'\n' || ch == b' ' || ch == b'\t' || ch == b'\r' {
                    break;
                }
                len += 1;
            }
            if len > 0 {
                // buffer = "'" + rest[..len]; then trim a split multibyte char.
                let mut buffer = vec![b'\''];
                buffer.extend_from_slice(&rest[..len]);
                while buffer[len] >= 0x80 && buffer[len] < 0xC0 {
                    len -= 1;
                }
                if buffer[len] >= 0xC0 {
                    len -= 1;
                }
                buffer.truncate(len + 1);
                buffer.push(b'\'');
                buffer
            } else {
                // The raw cursor, printed with %s: up to the next NUL byte.
                rest.iter().take_while(|&&b| b != 0).copied().collect()
            }
        } else {
            b"EOF".to_vec()
        };
        let mut message = Vec::new();
        let mut parts = format.splitn(2, "%s");
        message.extend_from_slice(parts.next().unwrap().as_bytes());
        if let Some(tail) = parts.next() {
            message.extend_from_slice(&fragment);
            message.extend_from_slice(tail.as_bytes());
        }
        let (line, column) = self.position(at);
        let mut text = String::from_utf8_lossy(&message).into_owned();
        text.push_str(&format!(" at line {line} column {column}"));
        ParseError(text)
    }

    fn position(&self, at: usize) -> (usize, usize) {
        // cursor_position: counts the cursor byte itself (the NUL terminator
        // at the end of the buffer), back to the previous newline.
        let mut column = 0;
        let mut line = 1;
        let mut i = at as isize;
        while i >= 0 {
            let b = if (i as usize) < self.src.len() { self.src[i as usize] } else { 0 };
            i -= 1;
            if b == b'\n' {
                line += 1;
                break;
            }
            column += 1;
        }
        while i >= 0 {
            if self.src[i as usize] == b'\n' {
                line += 1;
            }
            i -= 1;
        }
        (line, column)
    }

    fn eat_whitespace(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                b' ' | b'\n' | b'\t' | b'\r' if !self.eos() => self.cur += 1,
                b'/' if !self.eos() => self.eat_comment()?,
                _ => return Ok(()),
            }
        }
    }

    fn eat_comment(&mut self) -> Result<(), ParseError> {
        let start = self.cur;
        self.cur += 1;
        match self.peek() {
            b'/' if !self.eos() => {
                match self.src[self.cur..].iter().position(|&b| b == b'\n') {
                    Some(n) => self.cur += n + 1,
                    None => self.cur = self.src.len(),
                }
                Ok(())
            }
            b'*' if !self.eos() => {
                self.cur += 1;
                loop {
                    match self.src[self.cur..].iter().position(|&b| b == b'*') {
                        None => {
                            return Err(self.error_at("unterminated comment, expected closing '*/'", start));
                        }
                        Some(n) => {
                            self.cur += n + 1;
                            if self.peek() == b'/' && !self.eos() {
                                self.cur += 1;
                                return Ok(());
                            }
                        }
                    }
                }
            }
            _ => Err(self.error_at("unexpected token %s", start)),
        }
    }

    fn match_keyword(&mut self, keyword: &[u8]) -> Result<(), ParseError> {
        if self.src[self.cur..].starts_with(keyword) {
            self.cur += keyword.len();
            Ok(())
        } else {
            Err(self.error("unexpected token %s"))
        }
    }

    fn parse_any(&mut self) -> Result<Value, ParseError> {
        let mut stack: Vec<Frame> = Vec::new();
        'value: loop {
            // JSON_PHASE_VALUE
            self.eat_whitespace()?;
            let value_start = self.cur;
            let mut value = match self.peek() {
                b'n' => {
                    self.match_keyword(b"null")?;
                    Value::Nil
                }
                b't' => {
                    self.match_keyword(b"true")?;
                    Value::Bool(true)
                }
                b'f' => {
                    self.match_keyword(b"false")?;
                    Value::Bool(false)
                }
                b'N' | b'I' => return Err(self.error("unexpected token %s")),
                b'-' => {
                    self.cur += 1;
                    self.parse_number(true, value_start)?
                }
                b'0'..=b'9' => self.parse_number(false, value_start)?,
                b'"' => match self.parse_string()? {
                    Some(s) => Value::Str(s),
                    None => return Err(self.error("unexpected end of input, expected closing \"")),
                },
                b'[' => {
                    self.cur += 1;
                    self.eat_whitespace()?;
                    if self.peek() == b']' && !self.eos() {
                        self.cur += 1;
                        Value::Array(Vec::new())
                    } else {
                        self.nesting += 1;
                        if self.nesting > MAX_NESTING {
                            return Err(ParseError(format!("nesting of {} is too deep", self.nesting)));
                        }
                        stack.push(Frame::Array(Vec::new()));
                        continue 'value;
                    }
                }
                b'{' => {
                    self.cur += 1;
                    self.eat_whitespace()?;
                    if self.peek() == b'}' && !self.eos() {
                        self.cur += 1;
                        Value::Hash(Hash::new())
                    } else {
                        self.nesting += 1;
                        if self.nesting > MAX_NESTING {
                            return Err(ParseError(format!("nesting of {} is too deep", self.nesting)));
                        }
                        let key = self.object_key(true)?;
                        stack.push(Frame::Object(Hash::new(), Some(key)));
                        continue 'value;
                    }
                }
                0 if self.eos() => return Err(self.error("unexpected end of input")),
                0 => return Err(self.error("unexpected NULL byte: %s")),
                _ => return Err(self.error("unexpected character: %s")),
            };

            // A value completed: hand it to the enclosing frame, closing
            // frames as long as their closing bracket follows.
            loop {
                match stack.last_mut() {
                    None => return Ok(value),
                    Some(Frame::Array(items)) => {
                        items.push(value);
                        self.eat_whitespace()?;
                        match self.peek() {
                            b',' if !self.eos() => {
                                self.cur += 1;
                                continue 'value;
                            }
                            b']' if !self.eos() => {
                                self.cur += 1;
                                self.nesting -= 1;
                                let Some(Frame::Array(items)) = stack.pop() else { unreachable!() };
                                value = Value::Array(items);
                            }
                            _ => return Err(self.error("expected ',' or ']' after array value")),
                        }
                    }
                    Some(Frame::Object(hash, key)) => {
                        hash.insert(key.take().unwrap(), value);
                        self.eat_whitespace()?;
                        match self.peek() {
                            b',' if !self.eos() => {
                                self.cur += 1;
                                let k = self.object_key(false)?;
                                if let Some(Frame::Object(_, key)) = stack.last_mut() {
                                    *key = Some(k);
                                }
                                continue 'value;
                            }
                            b'}' if !self.eos() => {
                                self.cur += 1;
                                self.nesting -= 1;
                                let Some(Frame::Object(hash, _)) = stack.pop() else { unreachable!() };
                                value = Value::Hash(hash);
                            }
                            _ => return Err(self.error("expected ',' or '}' after object value, got: %s")),
                        }
                    }
                }
            }
        }
    }

    /// JSON_PHASE_OBJECT_KEY then JSON_PHASE_OBJECT_COLON.
    fn object_key(&mut self, first: bool) -> Result<Vec<u8>, ParseError> {
        self.eat_whitespace()?;
        if self.peek() != b'"' || self.eos() {
            return Err(self.error(if first { "expected object key, got %s" } else { "expected object key, got: %s" }));
        }
        let key = match self.parse_string()? {
            Some(k) => k,
            None => return Err(self.error("unexpected end of input, expected closing \"")),
        };
        self.eat_whitespace()?;
        if self.peek() != b':' || self.eos() {
            return Err(self.error(if first { "expected ':' after object key" } else { "expected ':' after object key, got: %s" }));
        }
        self.cur += 1;
        Ok(key)
    }

    fn digits(&mut self) -> usize {
        let start = self.cur;
        while self.cur < self.src.len() && self.src[self.cur].is_ascii_digit() {
            self.cur += 1;
        }
        self.cur - start
    }

    fn parse_number(&mut self, negative: bool, start: usize) -> Result<Value, ParseError> {
        let invalid = |p: &Self| Err(p.error_at("invalid number: %s", start));
        let first_digit = self.peek();
        let mantissa_digits = self.digits();
        if (first_digit == b'0' && mantissa_digits > 1) || (negative && mantissa_digits == 0) {
            return invalid(self);
        }
        let mut integer = true;
        if self.peek() == b'.' && !self.eos() {
            integer = false;
            self.cur += 1;
            if self.digits() == 0 {
                return invalid(self);
            }
        }
        if matches!(self.peek(), b'e' | b'E') && !self.eos() {
            integer = false;
            self.cur += 1;
            if matches!(self.peek(), b'-' | b'+') && !self.eos() {
                self.cur += 1;
            }
            if self.digits() == 0 {
                return invalid(self);
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.cur]).unwrap();
        if integer {
            return Ok(match text.parse::<i64>() {
                Ok(i) => Value::Int(i),
                Err(_) => Value::BigInt(text.to_string()),
            });
        }
        // Ruby's decoders (fast-float, then strtod) are correctly rounded, as
        // is Rust's; overflow gives +-Infinity and underflow +-0.0 in both.
        Ok(Value::Float(text.parse::<f64>().unwrap()))
    }

    /// Returns None for an unterminated string (cursor left at the end).
    fn parse_string(&mut self) -> Result<Option<Vec<u8>>, ParseError> {
        self.cur += 1;
        let mut out = Vec::new();
        loop {
            if self.eos() {
                return Ok(None);
            }
            let b = self.src[self.cur];
            match b {
                b'"' => {
                    self.cur += 1;
                    return Ok(Some(out));
                }
                b'\\' => {
                    let backslash = self.cur;
                    let Some(&esc) = self.src.get(self.cur + 1) else {
                        self.cur = self.src.len();
                        return Ok(None);
                    };
                    self.cur += 2;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'/' => out.push(b'/'),
                        b'\\' => out.push(b'\\'),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'u' => {
                            let mut ch = self.hex4(backslash)?;
                            if ch & 0xFC00 == 0xD800 {
                                if self.src[self.cur..].len() >= 6 && self.src[self.cur..].starts_with(b"\\u") {
                                    let low_at = self.cur;
                                    self.cur += 2;
                                    let sur = self.hex4(low_at)?;
                                    if sur & 0xFC00 != 0xDC00 {
                                        return Err(self.error_at("invalid surrogate pair at %s", backslash));
                                    }
                                    ch = (((ch & 0x3F) << 10) | ((((ch >> 6) & 0xF) + 1) << 16)) | (sur & 0x3FF);
                                } else {
                                    return Err(self.error_at("incomplete surrogate pair at %s", backslash));
                                }
                            }
                            push_utf8(&mut out, ch);
                        }
                        c if c < 0x20 => {
                            return Err(self.error_at("invalid ASCII control character in string: %s", backslash));
                        }
                        _ => return Err(self.error_at("invalid escape character in string: %s", backslash)),
                    }
                }
                c if c < 0x20 => {
                    return Err(self.error("invalid ASCII control character in string: %s"));
                }
                _ => {
                    out.push(b);
                    self.cur += 1;
                }
            }
        }
    }

    fn hex4(&mut self, backslash: usize) -> Result<u32, ParseError> {
        if self.src.len() < self.cur + 4 {
            return Err(self.error_at("incomplete unicode character escape sequence at %s", backslash));
        }
        let mut v = 0u32;
        for i in 0..4 {
            let d = (self.src[self.cur + i] as char).to_digit(16);
            match d {
                Some(d) => v = (v << 4) | d,
                None => {
                    return Err(self.error_at("incomplete unicode character escape sequence at %s", backslash));
                }
            }
        }
        self.cur += 4;
        Ok(v)
    }
}

/// convert_UTF32_to_UTF8: encodes lone surrogates too, as Ruby does.
fn push_utf8(out: &mut Vec<u8>, ch: u32) {
    if ch <= 0x7F {
        out.push(ch as u8);
    } else if ch <= 0x7FF {
        out.push(((ch >> 6) | 0xC0) as u8);
        out.push(((ch & 0x3F) | 0x80) as u8);
    } else if ch <= 0xFFFF {
        out.push(((ch >> 12) | 0xE0) as u8);
        out.push((((ch >> 6) & 0x3F) | 0x80) as u8);
        out.push(((ch & 0x3F) | 0x80) as u8);
    } else if ch <= 0x1FFFFF {
        out.push(((ch >> 18) | 0xF0) as u8);
        out.push((((ch >> 12) & 0x3F) | 0x80) as u8);
        out.push((((ch >> 6) & 0x3F) | 0x80) as u8);
        out.push(((ch & 0x3F) | 0x80) as u8);
    } else {
        out.push(b'?');
    }
}
