//! Filename/slug/key validation helpers (ported from the former `http.rs`).
//! Pure functions only — no HTTP parsing (axum handles that).

use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_NAME_LEN: usize = 100;
pub const KEY_MIN: usize = 4;
pub const KEY_MAX: usize = 128;

pub static ADJ: &[&str] = &[
    "amber", "arctic", "ashen", "autumn", "azure", "birch", "bold", "brave", "brisk", "bronze",
    "calm", "cedar", "chilly", "cinder", "clear", "cobalt", "comet", "copper", "coral", "crimson",
    "daring", "dawn", "ember", "frosty", "gentle", "glacier", "golden", "granite", "harbor",
    "hasty", "hazel", "honey", "ivory", "juniper", "kind", "lunar", "maple", "marble", "meadow",
    "misty", "mossy", "nimble", "noble", "ochre", "olive", "opal", "pebble", "pine", "plucky",
    "quartz", "quiet", "rapid", "redwood", "rocky", "ruby", "russet", "sandy", "silent", "silky",
    "silver", "sleepy", "solar", "spruce", "stormy", "sunny", "swift", "tawny", "tidal", "timber",
    "velvet", "verdant", "wheat", "willow", "wispy",
];
pub static NOUN: &[&str] = &[
    "acorn", "anchor", "arrow", "aspen", "badger", "basin", "beacon", "beaver", "brook", "burrow",
    "cabin", "canyon", "cavern", "cliff", "cove", "crane", "creek", "dune", "eagle", "elm",
    "falcon", "fern", "finch", "fjord", "fox", "garnet", "glade", "grove", "gull", "heron",
    "hill", "hollow", "inlet", "island", "jay", "kelp", "lagoon", "lark", "lichen", "lotus",
    "meadowlark", "mesa", "moth", "newt", "oasis", "otter", "owl", "oxbow", "peak", "pond",
    "poplar", "prairie", "quail", "raven", "reef", "ridge", "river", "robin", "sedge", "sparrow",
    "stone", "stork", "summit", "tern", "thrush", "trail", "trout", "tundra", "valley", "vole",
    "wren",
];

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
    let (hexpart, tail) = rest.split_at(32);
    if !tail.starts_with('_') {
        return None;
    }
    if !hexpart.bytes().all(|v| v.is_ascii_hexdigit()) {
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

/// JSON string escape (for hand-built JSON payloads).
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

fn rand_below(n: u32) -> u32 {
    // xorshift from nanos — no extra deps
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(1);
    static mut S: u64 = 0x9e37_79b9_7f4a_7c15;
    unsafe {
        S ^= S << 13;
        S ^= S >> 7;
        S ^= S << 17;
        S = S.wrapping_add(t);
        ((S >> 11) % (n as u64)) as u32
    }
}

pub fn gen_slug(db: &rusqlite::Connection) -> String {
    for _ in 0..50 {
        let s = format!(
            "{}-{}-{}",
            ADJ[rand_below(ADJ.len() as u32) as usize],
            NOUN[rand_below(NOUN.len() as u32) as usize],
            rand_below(999) + 1
        );
        let taken: bool = db
            .query_row("SELECT 1 FROM galleries WHERE slug = ?1", [&s], |_| Ok(()))
            .is_ok();
        if !taken {
            return s;
        }
    }
    format!("oddball-slug-{}", rand_below(999) + 1)
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
    fn gen_slug_shape_and_unique() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB, created INTEGER)",
            [],
        )
        .unwrap();
        let s = gen_slug(&db);
        assert!(is_slug(&s), "generated slug should validate: {s}");
    }

    #[test]
    fn escape_json() {
        assert_eq!(escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape("x\ny"), "x\\ny");
        assert_eq!(escape("\u{1}"), "\\u0001");
    }
}
