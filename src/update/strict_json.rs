//! JSON under the strict profile a signed manifest is read with, so the apps
//! (and the signing script) cannot disagree about what a body says. A small
//! validator checks the grammar itself, because a lenient library parser
//! keeps the last of two equal keys and accepts forms RFC 8259 does not.
//!
//! The profile: strict UTF-8 (invalid bytes, a byte order mark, UTF-16 are
//! errors); RFC 8259 grammar only (whitespace is space, tab, CR, LF; no
//! comments, trailing commas, single quotes); nesting at most 32 deep (the
//! top-level value is depth 1, arrays and objects both count, checked while
//! scanning); no duplicate object keys, compared after unescaping; no lone
//! surrogate escape; **numbers are whole non-negative integers only**
//! (`0` or `1-9` then digits: a sign, fraction, exponent or leading zero
//! fails the whole document). An integer past `i64` still passes here and
//! reads as out of range for the field that wants it.
//!
//! The value is built by the same scan, so what was validated is what is
//! read, and a number past what `f64` can hold cannot fail the document (it
//! reads as a float with no integer value). Pure. Sibling edges: none.
//! Outside: `serde_json::Value` (the shape only).

use serde_json::{Map, Number, Value};

const MAX_DEPTH: u32 = 32;

/// `bytes` as one JSON value under the profile, or `None`.
pub(super) fn parse(bytes: &[u8]) -> Option<Value> {
    std::str::from_utf8(bytes).ok()?;
    let mut scan = Scan { b: bytes, at: 0 };
    scan.skip_space();
    let value = scan.value(1)?;
    scan.skip_space();
    (scan.at == bytes.len()).then_some(value)
}

struct Scan<'a> {
    b: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.peek() == Some(byte)).then(|| self.at += 1)
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        self.b[self.at..]
            .starts_with(word)
            .then(|| self.at += word.len())
    }

    fn value(&mut self, depth: u32) -> Option<Value> {
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Value::String),
            b't' => self.literal(b"true").map(|()| Value::Bool(true)),
            b'f' => self.literal(b"false").map(|()| Value::Bool(false)),
            b'n' => self.literal(b"null").map(|()| Value::Null),
            b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    /// A whole non-negative number. Within `i64` it is an integer; past it,
    /// a float (the largest `f64` when even that is exceeded), which no
    /// integer field accepts.
    fn number(&mut self) -> Option<Value> {
        let start = self.at;
        if self.peek()? == b'0' {
            self.at += 1;
        } else {
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.at += 1;
            }
        }
        // Nothing may follow that continues a number: a digit after a leading
        // zero, a fraction, an exponent.
        (!matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'e' | b'E'))).then_some(())?;
        let digits = std::str::from_utf8(&self.b[start..self.at]).ok()?;
        Some(match digits.parse::<i64>() {
            Ok(n) => Value::Number(n.into()),
            Err(_) => {
                let big = digits
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .unwrap_or(f64::MAX);
                Value::Number(Number::from_f64(big)?)
            }
        })
    }

    fn array(&mut self, depth: u32) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.at += 1;
        self.skip_space();
        let mut items = Vec::new();
        if self.eat(b']').is_some() {
            return Some(Value::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value(depth + 1)?);
            self.skip_space();
            match self.peek()? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    return Some(Value::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn object(&mut self, depth: u32) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.at += 1;
        self.skip_space();
        let mut map = Map::new();
        if self.eat(b'}').is_some() {
            return Some(Value::Object(map));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            if map.contains_key(&key) {
                return None;
            }
            self.skip_space();
            self.eat(b':')?;
            self.skip_space();
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_space();
            match self.peek()? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Some(Value::Object(map));
                }
                _ => return None,
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.b.get(self.at..self.at + 4)?;
        self.at += 4;
        digits
            .iter()
            .try_fold(0u32, |acc, d| Some(acc * 16 + char::from(*d).to_digit(16)?))
    }

    /// A string, returned unescaped (for the duplicate-key comparison).
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let byte = self.peek()?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                0..=0x1f => return None,
                b'\\' => {
                    let escape = self.peek()?;
                    self.at += 1;
                    let plain = match escape {
                        b'"' => b'"',
                        b'\\' => b'\\',
                        b'/' => b'/',
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'u' => {
                            let first = self.hex4()?;
                            let code = match first {
                                0xd800..=0xdbff => {
                                    self.eat(b'\\')?;
                                    self.eat(b'u')?;
                                    let second = self.hex4()?;
                                    (0xdc00..=0xdfff).contains(&second).then_some(())?;
                                    0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                                }
                                0xdc00..=0xdfff => return None,
                                other => other,
                            };
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(
                                char::from_u32(code)?.encode_utf8(&mut buffer).as_bytes(),
                            );
                            continue;
                        }
                        _ => return None,
                    };
                    out.push(plain);
                }
                other => out.push(other),
            }
        }
    }
}
