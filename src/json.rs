//! The minimal, line-oriented JSON dialect of the i3bar protocol.
//!
//! Only flat objects are understood: a line is a sequence of `"name":value`
//! pairs, optionally wrapped in brackets and braces. Nested objects and
//! arrays are kept verbatim, numbers are whatever `strtoul` accepts, and
//! strings are decoded.

use crate::map::Map;
use nix::errno::Errno;

/// Size of the fixed buffers the protocol works with.
pub const BUFSIZ: usize = 8192;

fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

pub fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

pub fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn hex_value(c: u8) -> Option<u16> {
    match c {
        b'0'..=b'9' => Some(u16::from(c - b'0')),
        b'a'..=b'f' => Some(u16::from(c - b'a' + 10)),
        b'A'..=b'F' => Some(u16::from(c - b'A' + 10)),
        _ => None,
    }
}

/// Decode the four hex digits of a `\u` escape into UTF-8 bytes.
fn parse_codepoint(s: &[u8]) -> Option<Vec<u8>> {
    let mut codepoint: u16 = 0;
    for i in 0..4 {
        codepoint |= hex_value(at(s, i))? << (12 - i * 4);
    }
    let utf8 = if codepoint <= 0x7f {
        vec![codepoint as u8]
    } else if codepoint <= 0x7ff {
        vec![
            0xc0 | (codepoint >> 6) as u8,
            0x80 | (codepoint & 0x3f) as u8,
        ]
    } else {
        vec![
            0xe0 | (codepoint >> 12) as u8,
            0x80 | ((codepoint >> 6) & 0x3f) as u8,
            0x80 | (codepoint & 0x3f) as u8,
        ]
    };
    Some(utf8)
}

/// Length of the quoted string at the start of `s`, 0 if it is invalid.
/// With `buf`, the decoded string is stored (bounded like a C buffer).
fn parse_string(s: &[u8], buf: Option<&mut Vec<u8>>) -> usize {
    if at(s, 0) != b'"' {
        return 0;
    }
    let mut decoded = Vec::new();
    let mut end = 0;
    loop {
        end += 1;
        match at(s, end) {
            0 => return 0,
            b'"' => break,
            b'\\' => {
                end += 1;
                let c = match at(s, end) {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' => {
                        end += 1;
                        match parse_codepoint(&s[end.min(s.len())..]) {
                            Some(bytes) => decoded.extend_from_slice(&bytes),
                            None => return 0,
                        }
                        end += 3;
                        continue;
                    }
                    _ => return 0,
                };
                decoded.push(c);
            }
            c => {
                if is_cntrl(c) {
                    return 0;
                }
                decoded.push(c);
            }
        }
    }
    if let Some(buf) = buf {
        // The decoded text plus its terminating NUL must fit the buffer.
        if decoded.len() + 1 > BUFSIZ {
            return 0;
        }
        *buf = decoded;
    }
    end + 1
}

/// Length of the number at the start of `s` as `strtoul` reads it.
fn parse_number(s: &[u8]) -> usize {
    let mut i = 0;
    while is_space(at(s, i)) {
        i += 1;
    }
    if matches!(at(s, i), b'+' | b'-') {
        i += 1;
    }
    let digits = i;
    while at(s, i).is_ascii_digit() {
        i += 1;
    }
    if i == digits {
        0
    } else {
        i
    }
}

/// Length of a nested structure, delimiters included, 0 if it is invalid.
fn parse_nested(s: &[u8], open: u8, close: u8) -> usize {
    if at(s, 0) != open {
        return 0;
    }
    let mut nested = 1;
    let mut end = 0;
    while nested > 0 {
        end += 1;
        let c = at(s, end);
        if is_cntrl(c) {
            return 0;
        }
        if c == open {
            nested += 1;
        } else if c == close {
            nested -= 1;
        }
    }
    end + 1
}

fn parse_literal(s: &[u8], literal: &[u8]) -> usize {
    if s.starts_with(literal) {
        literal.len()
    } else {
        0
    }
}

/// Length of a value (string, number, object, array, true, false, null).
/// With `buf`, the value is stored: decoded for strings, verbatim otherwise.
fn parse_value(s: &[u8], buf: Option<&mut Vec<u8>>) -> usize {
    let want = buf.is_some();
    let mut decoded = Vec::new();
    let len = parse_string(s, if want { Some(&mut decoded) } else { None });
    if len > 0 {
        if let Some(buf) = buf {
            *buf = decoded;
        }
        return len;
    }
    let mut len = parse_number(s);
    if len == 0 {
        len = parse_nested(s, b'{', b'}');
    }
    if len == 0 {
        len = parse_nested(s, b'[', b']');
    }
    if len > 0 {
        if want && len >= BUFSIZ {
            return 0;
        }
        if let Some(buf) = buf {
            *buf = s[..len].to_vec();
        }
        return len;
    }
    for literal in [&b"true"[..], b"false", b"null"] {
        let len = parse_literal(s, literal);
        if len > 0 {
            if let Some(buf) = buf {
                *buf = literal.to_vec();
            }
            return len;
        }
    }
    0
}

/// Length of a separator optionally surrounded by whitespace, 0 otherwise.
fn parse_sep(s: &[u8], sep: u8) -> usize {
    let mut i = 0;
    while is_space(at(s, i)) {
        i += 1;
    }
    if at(s, i) != sep {
        return 0;
    }
    i += 1;
    while is_space(at(s, i)) {
        i += 1;
    }
    i
}

/// Parse an inline `"name" : value` pair.
fn parse_pair(s: &[u8]) -> Option<(usize, Vec<u8>, Vec<u8>)> {
    let mut name = Vec::new();
    let mut value = Vec::new();
    let mut len = parse_string(s, Some(&mut name));
    if len == 0 {
        return None;
    }
    let sep = parse_sep(&s[len..], b':');
    if sep == 0 {
        return None;
    }
    len += sep;
    let vlen = parse_value(&s[len..], Some(&mut value));
    if vlen == 0 {
        return None;
    }
    Some((len + vlen, name, value))
}

/// Parse one line of flattened JSON into `map`.
pub fn parse_line(line: &[u8], map: &mut Map) -> Result<(), Errno> {
    let mut p = 0;
    loop {
        while matches!(at(line, p), b'[' | b']' | b',' | b'{' | b'}') || is_space(at(line, p)) {
            p += 1;
        }
        if at(line, p) == 0 {
            return Ok(());
        }
        let (len, name, value) = parse_pair(&line[p..]).ok_or(Errno::EINVAL)?;
        p += len;
        let delim = at(line, p);
        if delim != b',' && delim != b'}' && delim != 0 && !is_space(delim) {
            return Err(Errno::EINVAL);
        }
        if delim != 0 {
            p += 1;
        }
        map.set(&name, &value);
    }
}

/// Whether `s` is exactly one JSON string.
pub fn is_string(s: &[u8]) -> bool {
    !s.is_empty() && parse_string(s, None) == s.len()
}

/// Whether `s` is exactly one JSON value.
pub fn is_valid(s: &[u8]) -> bool {
    !s.is_empty() && parse_value(s, None) == s.len()
}

/// Quote and escape `s` as a JSON string; fails like an overflowing
/// fixed-size buffer when the result is too long.
pub fn escape(s: &[u8]) -> Result<Vec<u8>, Errno> {
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'"');
    for &c in s {
        match c {
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            c if is_cntrl(c) => out.extend_from_slice(format!("\\u{:04x}", c).as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
    if out.len() >= BUFSIZ {
        return Err(Errno::ENOSPC);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        assert!(is_valid(b" 7"));
        assert!(is_valid(b"+5"));
        assert!(is_valid(b"{x}"));
        assert!(!is_valid(b"[1"));
        assert!(!is_valid(b"1.5"));
        assert!(is_string(b"\"quoted\""));
        assert!(!is_string(b"\"a\"b\""));
    }

    #[test]
    fn lines() {
        let mut map = Map::new();
        parse_line(br#"{"full_text":"a\nb A","min_width":-7}"#, &mut map).unwrap();
        assert_eq!(map.get(b"full_text"), Some(&b"a\nb A"[..]));
        assert_eq!(map.get(b"min_width"), Some(&b"-7"[..]));
        let mut map = Map::new();
        assert!(parse_line(br#"{"full_text":"J","min_width":2.5}"#, &mut map).is_err());
        assert_eq!(map.get(b"full_text"), Some(&b"J"[..]));
    }

    #[test]
    fn escaping() {
        assert_eq!(escape(b"a\x01\"").unwrap(), b"\"a\\u0001\\\"\"".to_vec());
    }
}
