//! HEADER section: FILE_DESCRIPTION, FILE_NAME and FILE_SCHEMA.

use crate::attr::{parse_attrs, Value};
use memchr::memmem;

#[derive(Clone, Debug, Default)]
pub struct Header {
    pub description: Vec<String>,
    pub implementation_level: String,
    pub file_name: String,
    pub time_stamp: String,
    pub author: Vec<String>,
    pub organization: Vec<String>,
    pub preprocessor_version: String,
    pub originating_system: String,
    pub authorization: String,
    pub schemas: Vec<String>,
    /// Byte range of the HEADER section body (between `HEADER;` and `ENDSEC;`).
    pub span: (usize, usize),
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::List(l)) => l.iter().map(|x| x.display_string()).collect(),
        Some(x) => vec![x.display_string()],
        None => Vec::new(),
    }
}

fn string(v: Option<&Value>) -> String {
    v.map(|x| x.display_string()).unwrap_or_default()
}

/// Parse the HEADER section. Tolerant: missing or malformed entities leave
/// their fields empty.
pub fn parse_header(data: &[u8]) -> Option<Header> {
    let h = memmem::find(data, b"HEADER;")? + 7;
    let e = h + memmem::find(&data[h..], b"ENDSEC;")?;
    let body_raw = &data[h..e];
    // Blank out /* comments */ (ST-Developer writes them between attributes).
    let cleaned = strip_comments(body_raw);
    let body: &[u8] = &cleaned;
    let mut hdr = Header { span: (h, e), ..Default::default() };

    let mut i = 0usize;
    while i < body.len() {
        // name
        while i < body.len() && crate::scan::is_ws(body[i]) {
            i += 1;
        }
        let s = i;
        while i < body.len() && crate::scan::is_ident(body[i]) {
            i += 1;
        }
        if i == s {
            // skip junk to next ';'
            match memchr::memchr(b';', &body[i..]) {
                Some(k) => {
                    i += k + 1;
                    continue;
                }
                None => break,
            }
        }
        let name = &body[s..i];
        while i < body.len() && crate::scan::is_ws(body[i]) {
            i += 1;
        }
        if body.get(i) != Some(&b'(') {
            break;
        }
        // find the matching ';' string-aware
        let end = find_semicolon(body, i)?;
        let inner_end = body[..end].iter().rposition(|&b| b == b')')?;
        let attrs = parse_attrs(&body[i + 1..inner_end]).unwrap_or_default();
        match name {
            b"FILE_DESCRIPTION" => {
                hdr.description = strings(attrs.first());
                hdr.implementation_level = string(attrs.get(1));
            }
            b"FILE_NAME" => {
                hdr.file_name = string(attrs.first());
                hdr.time_stamp = string(attrs.get(1));
                hdr.author = strings(attrs.get(2));
                hdr.organization = strings(attrs.get(3));
                hdr.preprocessor_version = string(attrs.get(4));
                hdr.originating_system = string(attrs.get(5));
                hdr.authorization = string(attrs.get(6));
            }
            b"FILE_SCHEMA" => {
                hdr.schemas = strings(attrs.first());
            }
            _ => {}
        }
        i = end + 1;
    }
    Some(hdr)
}

/// Replace every `/* ... */` outside string literals with spaces, keeping
/// byte offsets stable.
fn strip_comments(s: &[u8]) -> Vec<u8> {
    let mut out = s.to_vec();
    let mut i = 0usize;
    let mut in_str = false;
    while i < out.len() {
        match out[i] {
            b'\'' => in_str = !in_str,
            b'/' if !in_str && out.get(i + 1) == Some(&b'*') => {
                let end = memmem::find(&out[i + 2..], b"*/").map(|m| i + 2 + m + 2).unwrap_or(out.len());
                for b in &mut out[i..end] {
                    if *b != b'\n' && *b != b'\r' {
                        *b = b' ';
                    }
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn find_semicolon(s: &[u8], mut i: usize) -> Option<usize> {
    let mut in_str = false;
    while i < s.len() {
        match s[i] {
            b'\'' => in_str = !in_str,
            b';' if !in_str => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header() {
        let d = b"ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('CAx-IF Rec.Pracs.'),'2;1');\nFILE_NAME('as1.stp','2020-01-02T03:04:05',('me'),('org'),'pre 1.0','Sys 2019','');\nFILE_SCHEMA(('AP242_MANAGED_MODEL_BASED_3D_ENGINEERING_MIM_LF { 1 0 10303 442 1 1 4 }'));\nENDSEC;\nDATA;\nENDSEC;\nEND-ISO-10303-21;\n";
        let h = parse_header(d).unwrap();
        assert_eq!(h.implementation_level, "2;1");
        assert_eq!(h.file_name, "as1.stp");
        assert_eq!(h.originating_system, "Sys 2019");
        assert_eq!(h.author, vec!["me".to_string()]);
        assert!(h.schemas[0].starts_with("AP242_MANAGED"));
    }

    #[test]
    fn parses_header_with_comments() {
        let d = b"ISO-10303-21;\r\nHEADER;\r\n/* Generated by software containing ST-Developer\r\n * from STEP Tools, Inc. (www.steptools.com) \r\n */\r\n\r\nFILE_DESCRIPTION(\r\n/* description */ (''),\r\n/* implementation_level */ '2;1');\r\n\r\nFILE_NAME(\r\n/* name */ 'engine.step',\r\n/* time_stamp */ '2019-01-01T00:00:00',\r\n/* author */ (''),\r\n/* organization */ (''),\r\n/* preprocessor_version */ 'ST-DEVELOPER v18',\r\n/* originating_system */ 'Autodesk Inventor 2019',\r\n/* authorisation */ '');\r\n\r\nFILE_SCHEMA (('AUTOMOTIVE_DESIGN { 1 0 10303 214 3 1 1 }'));\r\nENDSEC;\r\nDATA;\r\nENDSEC;\r\nEND-ISO-10303-21;\r\n";
        let h = parse_header(d).unwrap();
        assert_eq!(h.implementation_level, "2;1");
        assert_eq!(h.file_name, "engine.step");
        assert_eq!(h.originating_system, "Autodesk Inventor 2019");
        assert!(h.schemas[0].starts_with("AUTOMOTIVE_DESIGN"));
    }
}
