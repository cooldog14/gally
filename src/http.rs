use httparse::{Request, EMPTY_HEADER};
use std::collections::HashMap;

pub const MAX_NAME_LEN: usize = 100;
pub const KEY_MIN: usize = 4;
pub const KEY_MAX: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Put,
    Post,
    Delete,
    Other,
}

pub fn content_type_for(ext: &str) -> &'static str {
    match ext {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

pub fn is_slug(s: &str) -> bool {
    // ^[a-z]+-[a-z]+-[1-9][0-9]{0,2}$
    let mut parts = s.split('-');
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return false;
    }
    let alpha = |x: &str| !x.is_empty() && x.bytes().all(|v| v.is_ascii_lowercase());
    if !alpha(a) || !alpha(b) {
        return false;
    }
    if c.len() > 3 || c.is_empty() || c.as_bytes()[0] == b'0' {
        return false;
    }
    c.bytes().all(|v| v.is_ascii_digit())
}

pub fn photo_slug(name: &str) -> Option<&str> {
    // ^([a-z]+-[a-z]+-[0-9]{1,3})_[0-9a-f]{32}_
    let us = name.find('_')?;
    let slug = &name[..us];
    let rest = &name[us + 1..];
    if rest.len() < 33 {
        return None;
    }
    let (hex, tail) = rest.split_at(32);
    if !tail.starts_with('_') {
        return None;
    }
    if !hex.bytes().all(|v| v.is_ascii_hexdigit()) {
        return None;
    }
    // slug check (allow 0-prefixed? python FILE_SLUG_RE allows 0xx; keep permissive)
    let mut parts = slug.split('-');
    let a = parts.next()?;
    let b = parts.next()?;
    let c = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if a.is_empty() || b.is_empty() || c.is_empty() {
        return None;
    }
    if !a.bytes().all(|v| v.is_ascii_lowercase()) || !b.bytes().all(|v| v.is_ascii_lowercase()) {
        return None;
    }
    if c.len() > 3 || !c.bytes().all(|v| v.is_ascii_digit()) {
        return None;
    }
    Some(slug)
}

pub fn sanitize_filename(raw: &str) -> (String, String) {
    let base0 = raw.rsplit('/').next().unwrap_or(raw).rsplit('\\').next().unwrap_or(raw);
    let mut base: String = base0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if base.is_empty() || base == "." || base == ".." {
        base = "file".into();
    }
    if base.len() > MAX_NAME_LEN {
        let (stem, ext) = match base.rfind('.') {
            Some(i) if i > 0 => {
                let e = base[i..].to_lowercase();
                (base[..i].to_string(), e)
            }
            _ => (base.clone(), String::new()),
        };
        let mut suffix = ext;
        if suffix.len() >= MAX_NAME_LEN {
            suffix.truncate(MAX_NAME_LEN);
        }
        let keep = MAX_NAME_LEN - suffix.len();
        let mut s = stem;
        s.truncate(keep);
        base = s + &suffix;
    }
    let ext = match base.rfind('.') {
        Some(i) if i > 0 => base[i..].to_lowercase(),
        _ => String::new(),
    };
    (base, ext)
}

pub fn normalize_key(raw: &str) -> Option<String> {
    let k = raw.trim();
    if (KEY_MIN..=KEY_MAX).contains(&k.len()) {
        Some(k.to_string())
    } else {
        None
    }
}

pub fn json_body(status: u16, obj: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        status,
        reason,
        obj.len(),
        obj
    )
    .into_bytes()
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

pub fn parse_cookies(h: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for part in h.split(';') {
        let p = part.trim();
        if let Some(i) = p.find('=') {
            m.insert(p[..i].trim().to_string(), p[i + 1..].trim().to_string());
        }
    }
    m
}

pub fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

pub fn parse_request(head: &[u8]) -> Option<(Method, String, HashMap<String, String>, usize)> {
    let mut hdrs = [EMPTY_HEADER; 32];
    let mut req = Request::new(&mut hdrs);
    let Ok(httparse::Status::Complete(_)) = req.parse(head) else { return None };
    let method = match req.method.unwrap_or("") {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "PUT" => Method::Put,
        "POST" => Method::Post,
        "DELETE" => Method::Delete,
        _ => Method::Other,
    };
    let path = req.path.unwrap_or("/").split('?').next().unwrap_or("/").to_string();
    let mut map = HashMap::new();
    for h in req.headers.iter() {
        map.insert(h.name.to_lowercase(), String::from_utf8_lossy(h.value).to_string());
    }
    Some((method, path, map, 0))
}

pub fn percent_decode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                o.push((h * 16 + l) as char);
                i += 3;
                continue;
            }
        }
        o.push(b[i] as char);
        i += 1;
    }
    o
}
pub fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[derive(Default)]
pub struct SerdeJsonLike(HashMap<String, String>);
impl SerdeJsonLike {
    pub fn get(&self, k: &str) -> Option<&String> {
        self.0.get(k)
    }
}
/// Minimal flat-JSON string parser: {"k":"v",...} — enough for {key,password}.
pub fn parse_json_obj(b: &[u8]) -> SerdeJsonLike {
    let mut m = HashMap::new();
    let s = String::from_utf8_lossy(b);
    let s = s.trim();
    if !s.starts_with('{') {
        return SerdeJsonLike(m);
    }
    // very small state machine for "key":"value" pairs
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut k = String::new();
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' && i + 1 < chars.len() {
                i += 1;
                k.push(chars[i]);
            } else {
                k.push(chars[i]);
            }
            i += 1;
        }
        i += 1; // closing quote
        while i < chars.len() && (chars[i] == ' ' || chars[i] == ':' || chars[i] == ',') {
            if chars[i] == ':' {
                i += 1;
                break;
            }
            i += 1;
        }
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        let mut v = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    match chars[i] {
                        'n' => v.push('\n'),
                        't' => v.push('\t'),
                        c => v.push(c),
                    }
                } else {
                    v.push(chars[i]);
                }
                i += 1;
            }
            i += 1;
        } else {
            while i < chars.len() && chars[i] != ',' && chars[i] != '}' {
                v.push(chars[i]);
                i += 1;
            }
            v = v.trim().to_string();
        }
        if !k.is_empty() {
            m.insert(k, v);
        }
    }
    SerdeJsonLike(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts() {
        assert!(is_slug("happy-otter-7"));
        assert!(is_slug("blue-heron-123"));
        assert!(is_slug("a-b-1"));
    }

    #[test]
    fn slug_rejects() {
        assert!(!is_slug("Happy-otter-7")); // uppercase
        assert!(!is_slug("happy-otter-007")); // leading zero
        assert!(!is_slug("happy-otter-1234")); // 4-digit
        assert!(!is_slug("happy-otter-0")); // zero
        assert!(!is_slug("happy-otter")); // missing part
        assert!(!is_slug("happy-otter-1-x")); // extra part
        assert!(!is_slug("happy-ott3r-1")); // digit in word
        assert!(!is_slug("")); // empty
    }

    #[test]
    fn sanitize_truncates_to_100() {
        let long = "a".repeat(150) + ".JPG";
        let (base, ext) = sanitize_filename(&long);
        assert!(base.len() <= MAX_NAME_LEN);
        assert_eq!(ext, ".jpg");
        assert!(base.ends_with(".jpg"));
    }

    #[test]
    fn sanitize_ext_lowercased() {
        let (base, ext) = sanitize_filename("Photo.PNG");
        assert_eq!(ext, ".png");
        assert_eq!(base, "Photo.PNG");
        let (_, ext2) = sanitize_filename("noext");
        assert_eq!(ext2, "");
    }

    #[test]
    fn normalize_key_bounds() {
        assert!(normalize_key("abcd").is_some());
        assert!(normalize_key("abc").is_none());
        assert!(normalize_key(&"k".repeat(128)).is_some());
        assert!(normalize_key(&"k".repeat(129)).is_none());
        assert_eq!(normalize_key("  secret  "), Some("secret".to_string()));
    }

    #[test]
    fn decode_percent() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("%ZZ"), "%ZZ"); // invalid kept
        assert_eq!(percent_decode("trail%"), "trail%");
    }

    #[test]
    fn photo_slug_ok() {
        let h = "a".repeat(32);
        let name = format!("happy-otter-7_{}_photo.jpg", h);
        assert_eq!(photo_slug(&name), Some("happy-otter-7"));
    }

    #[test]
    fn photo_slug_bad() {
        assert!(photo_slug("nope.jpg").is_none());
        let h = "z".repeat(32); // not hex
        assert!(photo_slug(&format!("happy-otter-7_{}_x.jpg", h)).is_none());
        assert!(photo_slug("Happy-otter-7_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg").is_none());
    }

    #[test]
    fn request_get_path_headers() {
        let raw = b"GET /photos/abc?x=1 HTTP/1.1\r\nHost: ex.com\r\nX-Gallery-Key: sekrit\r\n\r\n";
        let (m, path, hdrs, _) = parse_request(raw).expect("parse");
        assert!(m == Method::Get);
        assert_eq!(path, "/photos/abc");
        assert_eq!(hdrs.get("host").unwrap(), "ex.com");
        assert_eq!(hdrs.get("x-gallery-key").unwrap(), "sekrit");
    }

    #[test]
    fn request_bad_none() {
        assert!(parse_request(b"GARBAGE").is_none());
        assert!(parse_request(b"GET /only-path").is_none());
    }
}
