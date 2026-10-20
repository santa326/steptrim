//! Attribute-list parser for a single entity instance.
//!
//! This is deliberately generic and allocation-light; it is used for the
//! few thousand structure entities. Hot geometry types get their own
//! specialised parsers later.

use crate::scan::{is_ident, is_ws};
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value<'a> {
    /// `#123`
    Ref(u32),
    /// Raw bytes between the apostrophes, escapes untouched.
    Str(&'a [u8]),
    /// Numeric literal text, e.g. `-1.5E-3`.
    Num(&'a [u8]),
    /// Enumeration or logical, e.g. `.T.`, `.MILLI.` (bytes between the dots).
    Enum(&'a [u8]),
    /// `$`
    Null,
    /// `*`
    Derived,
    List(Vec<Value<'a>>),
    /// A typed value such as `LENGTH_MEASURE(1.0)`.
    Typed(&'a [u8], Box<Value<'a>>),
}

impl<'a> Value<'a> {
    pub fn as_id(&self) -> Option<u32> {
        match self {
            Value::Ref(r) => Some(*r),
            Value::Typed(_, v) => v.as_id(),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&'a [u8]> {
        match self {
            Value::Str(s) => Some(s),
            Value::Typed(_, v) => v.as_str(),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[Value<'a>]> {
        match self {
            Value::List(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => std::str::from_utf8(n).ok()?.parse().ok(),
            Value::Typed(_, v) => v.as_f64(),
            _ => None,
        }
    }
    /// Decoded string for display: `''` becomes `'`; other escapes are kept
    /// verbatim. Returns an empty string for non-strings.
    pub fn display_string(&self) -> String {
        match self.as_str() {
            Some(s) => unescape_simple(s),
            None => String::new(),
        }
    }
}

/// Decode a Part 21 string for display: `''` becomes `'`; the control
/// directives `\S\c` (ISO 8859-1 upper half), `\X\hh` (one 8859-1 byte),
/// `\X2\hhhh...\X0\` (UTF-16 code units) and `\X4\hhhhhhhh...\X0\` (UCS-4)
/// are decoded. Raw non-ASCII bytes, from exporters that skip the escape
/// rule, are read as UTF-8 when valid and as ISO 8859-1 otherwise.
pub fn unescape_simple(s: &[u8]) -> String {
    if !s.iter().any(|&b| b == b'\'' || b == b'\\' || b >= 0x80) {
        return String::from_utf8_lossy(s).into_owned();
    }
    fn text(bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(t) => t.to_string(),
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        }
    }
    fn hex(bytes: &[u8]) -> Option<u32> {
        u32::from_str_radix(std::str::from_utf8(bytes).ok()?, 16).ok()
    }
    let mut out = String::new();
    let mut run = 0usize; // start of the pending literal run
    let mut i = 0usize;
    while i < s.len() {
        let b = s[i];
        if b == b'\'' && s.get(i + 1) == Some(&b'\'') {
            out.push_str(&text(&s[run..i]));
            out.push('\'');
            i += 2;
            run = i;
            continue;
        }
        if b != b'\\' {
            i += 1;
            continue;
        }
        let rest = &s[i + 1..];
        let consumed: usize = if rest.len() >= 3 && rest[0] == b'S' && rest[1] == b'\\' {
            out.push_str(&text(&s[run..i]));
            out.push(char::from_u32(rest[2] as u32 + 0x80).unwrap_or('?'));
            4
        } else if rest.len() >= 4
            && rest[0] == b'X'
            && rest[1] == b'\\'
            && rest[2].is_ascii_hexdigit()
            && rest[3].is_ascii_hexdigit()
        {
            out.push_str(&text(&s[run..i]));
            out.push(char::from_u32(hex(&rest[2..4]).unwrap_or(0x3F)).unwrap_or('?'));
            5
        } else if rest.len() >= 3 && rest[0] == b'X' && (rest[1] == b'2' || rest[1] == b'4') && rest[2] == b'\\' {
            out.push_str(&text(&s[run..i]));
            let width = if rest[1] == b'2' { 4 } else { 8 };
            let mut j = 3usize;
            let mut units: Vec<u16> = Vec::new();
            loop {
                if rest[j..].starts_with(b"\\X0\\") {
                    j += 4;
                    break;
                }
                if j + width > rest.len() {
                    break;
                }
                match hex(&rest[j..j + width]) {
                    Some(cp) if width == 4 => units.push(cp as u16),
                    Some(cp) => {
                        if let Some(c) = char::from_u32(cp) {
                            let mut buf = [0u16; 2];
                            units.extend_from_slice(c.encode_utf16(&mut buf));
                        }
                    }
                    None => break,
                }
                j += width;
            }
            out.push_str(&String::from_utf16_lossy(&units));
            1 + j
        } else if rest.first() == Some(&b'\\') {
            out.push_str(&text(&s[run..i]));
            out.push('\\');
            2
        } else {
            0
        };
        if consumed == 0 {
            i += 1;
        } else {
            i += consumed;
            run = i;
        }
    }
    out.push_str(&text(&s[run..]));
    out
}

/// Serialize a value as Part 21 text. References are offset by `offset`
/// (0 keeps the original ids).
pub fn write_value(out: &mut Vec<u8>, v: &Value, offset: u64) {
    match v {
        Value::Ref(r) => {
            out.push(b'#');
            out.extend_from_slice((*r as u64 + offset).to_string().as_bytes());
        }
        Value::Str(s) => {
            out.push(b'\'');
            out.extend_from_slice(s);
            out.push(b'\'');
        }
        Value::Num(n) => out.extend_from_slice(n),
        Value::Enum(e) => {
            out.push(b'.');
            out.extend_from_slice(e);
            out.push(b'.');
        }
        Value::Null => out.push(b'$'),
        Value::Derived => out.push(b'*'),
        Value::List(l) => {
            out.push(b'(');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, x, offset);
            }
            out.push(b')');
        }
        Value::Typed(name, inner) => {
            out.extend_from_slice(name);
            out.push(b'(');
            write_value(out, inner, offset);
            out.push(b')');
        }
    }
}

fn write_attr_list(out: &mut Vec<u8>, attrs: &[Value], offset: u64) {
    out.push(b'(');
    for (i, v) in attrs.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_value(out, v, offset);
    }
    out.push(b')');
}

/// Serialize a simple entity instance: `#id=TYPE(attrs);` (no line ending).
pub fn write_simple(out: &mut Vec<u8>, id: u64, type_name: &str, attrs: &[Value], offset: u64) {
    out.push(b'#');
    out.extend_from_slice(id.to_string().as_bytes());
    out.push(b'=');
    out.extend_from_slice(type_name.as_bytes());
    write_attr_list(out, attrs, offset);
    out.push(b';');
}

/// Serialize a complex instance: `#id=(A(attrs)B(attrs));` (no line ending).
pub fn write_complex(out: &mut Vec<u8>, id: u64, parts: &[(&[u8], Vec<Value>)], offset: u64) {
    out.push(b'#');
    out.extend_from_slice(id.to_string().as_bytes());
    out.extend_from_slice(b"=(");
    for (name, attrs) in parts {
        out.extend_from_slice(name);
        write_attr_list(out, attrs, offset);
    }
    out.extend_from_slice(b");");
}

#[derive(Debug, Clone)]
pub struct AttrError {
    pub at: usize,
    pub msg: &'static str,
}

impl fmt::Display for AttrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.at)
    }
}
impl std::error::Error for AttrError {}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && is_ws(self.s[self.i]) {
            self.i += 1;
        }
    }
    fn err(&self, msg: &'static str) -> AttrError {
        AttrError { at: self.i, msg }
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// Parse comma-separated values until `)` or end of input.
    fn list_body(&mut self, closing: bool) -> Result<Vec<Value<'a>>, AttrError> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    if closing {
                        return Err(self.err("unterminated list"));
                    }
                    return Ok(out);
                }
                Some(b')') => {
                    if closing {
                        self.i += 1;
                    }
                    return Ok(out);
                }
                Some(b',') => {
                    self.i += 1;
                    continue;
                }
                Some(_) => {
                    let v = self.value()?;
                    out.push(v);
                }
            }
        }
    }

    fn value(&mut self) -> Result<Value<'a>, AttrError> {
        self.skip_ws();
        let c = self.peek().ok_or_else(|| self.err("unexpected end"))?;
        match c {
            b'#' => {
                let s = self.i + 1;
                let mut e = s;
                while e < self.s.len() && self.s[e].is_ascii_digit() {
                    e += 1;
                }
                if e == s {
                    return Err(self.err("bad reference"));
                }
                let id: u32 = std::str::from_utf8(&self.s[s..e])
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .ok_or_else(|| self.err("reference too large"))?;
                self.i = e;
                Ok(Value::Ref(id))
            }
            b'\'' => {
                let s = self.i + 1;
                let mut e = s;
                loop {
                    match self.s.get(e) {
                        None => return Err(self.err("unterminated string")),
                        Some(b'\'') => {
                            if self.s.get(e + 1) == Some(&b'\'') {
                                e += 2;
                            } else {
                                break;
                            }
                        }
                        Some(_) => e += 1,
                    }
                }
                self.i = e + 1;
                Ok(Value::Str(&self.s[s..e]))
            }
            b'(' => {
                self.i += 1;
                Ok(Value::List(self.list_body(true)?))
            }
            b'.' => {
                let s = self.i + 1;
                let mut e = s;
                while e < self.s.len() && self.s[e] != b'.' {
                    e += 1;
                }
                if e >= self.s.len() {
                    return Err(self.err("unterminated enumeration"));
                }
                self.i = e + 1;
                Ok(Value::Enum(&self.s[s..e]))
            }
            b'$' => {
                self.i += 1;
                Ok(Value::Null)
            }
            b'*' => {
                self.i += 1;
                Ok(Value::Derived)
            }
            b'-' | b'+' | b'0'..=b'9' => {
                let s = self.i;
                let mut e = s + 1;
                while e < self.s.len() && matches!(self.s[e], b'0'..=b'9' | b'.' | b'e' | b'E' | b'-' | b'+') {
                    e += 1;
                }
                self.i = e;
                Ok(Value::Num(&self.s[s..e]))
            }
            b'A'..=b'Z' | b'_' => {
                let s = self.i;
                let mut e = s;
                while e < self.s.len() && is_ident(self.s[e]) {
                    e += 1;
                }
                self.i = e;
                self.skip_ws();
                if self.peek() == Some(b'(') {
                    self.i += 1;
                    let inner = self.list_body(true)?;
                    let v = if inner.len() == 1 {
                        inner.into_iter().next().unwrap()
                    } else {
                        Value::List(inner)
                    };
                    Ok(Value::Typed(&self.s[s..e], Box::new(v)))
                } else {
                    // Bare identifier (rare); treat as an enum-like token.
                    Ok(Value::Enum(&self.s[s..e]))
                }
            }
            _ => Err(self.err("unexpected character")),
        }
    }
}

/// Parse the text inside an entity's outer parentheses into its attribute values.
pub fn parse_attrs(s: &[u8]) -> Result<Vec<Value<'_>>, AttrError> {
    let mut p = Parser { s, i: 0 };
    p.list_body(false)
}

/// Parse the body of a complex (external-mapped) instance: a sequence of
/// `TYPE(attrs)` partial records. Returns (type name, attributes) per record.
pub fn parse_complex(s: &[u8]) -> Result<Vec<(&[u8], Vec<Value<'_>>)>, AttrError> {
    let mut p = Parser { s, i: 0 };
    let mut out = Vec::new();
    loop {
        p.skip_ws();
        match p.peek() {
            None | Some(b')') => return Ok(out),
            Some(b'A'..=b'Z') | Some(b'_') => {
                let st = p.i;
                let mut e = st;
                while e < s.len() && is_ident(s[e]) {
                    e += 1;
                }
                p.i = e;
                p.skip_ws();
                if p.peek() != Some(b'(') {
                    return Err(p.err("expected ( after partial type name"));
                }
                p.i += 1;
                let attrs = p.list_body(true)?;
                out.push((&s[st..e], attrs));
            }
            Some(_) => return Err(p.err("unexpected character in complex record")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed_attributes() {
        let v = parse_attrs(b"'name','it''s',(#1,#2),$,*,.T.,-1.5E-3,LENGTH_MEASURE(2.0),#77").unwrap();
        assert_eq!(v.len(), 9);
        assert_eq!(v[0].display_string(), "name");
        assert_eq!(v[1].display_string(), "it's");
        let l = v[2].as_list().unwrap();
        assert_eq!(l[1].as_id(), Some(2));
        assert_eq!(v[3], Value::Null);
        assert_eq!(v[4], Value::Derived);
        assert_eq!(v[5], Value::Enum(b"T"));
        assert_eq!(v[6].as_f64(), Some(-0.0015));
        assert_eq!(v[7].as_f64(), Some(2.0));
        assert_eq!(v[8].as_id(), Some(77));
    }

    #[test]
    fn decodes_control_directives() {
        assert_eq!(unescape_simple(b"plain"), "plain");
        assert_eq!(unescape_simple(b"it''s"), "it's");
        assert_eq!(unescape_simple(b"\\X2\\04210438\\X0\\-14"), "\u{421}\u{438}-14");
        assert_eq!(unescape_simple(b"a\\S\\ab"), "a\u{e1}b");
        assert_eq!(unescape_simple(b"\\X\\E9t\\X2\\00E9\\X0\\"), "\u{e9}t\u{e9}");
        assert_eq!(unescape_simple(b"\\X4\\0001F600\\X0\\"), "\u{1F600}");
        assert_eq!(unescape_simple("raw \u{e9}".as_bytes()), "raw \u{e9}");
        assert_eq!(unescape_simple(b"back\\\\slash"), "back\\slash");
        assert_eq!(unescape_simple(b"odd\\path"), "odd\\path");
    }

    #[test]
    fn parses_complex_record() {
        let body = b"REPRESENTATION_RELATIONSHIP('','',#5,#6)REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#7)SHAPE_REPRESENTATION_RELATIONSHIP()";
        let r = parse_complex(body).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0, b"REPRESENTATION_RELATIONSHIP");
        assert_eq!(r[0].1[2].as_id(), Some(5));
        assert_eq!(r[1].1[0].as_id(), Some(7));
        assert!(r[2].1.is_empty());
    }
}
