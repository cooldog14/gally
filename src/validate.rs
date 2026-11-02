//! Filename/slug/key validation helpers (ported from the former `http.rs`).
//! Pure functions only — no HTTP parsing (axum handles that).

// (no imports: uuid provides the thread-safe randomness)

pub const MAX_NAME_LEN: usize = 100;
pub const KEY_MIN: usize = 6;
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
    // ^[a-z]+-[a-z]+-[a-z]+-[a-z]+-[0-9a-f]{4}$ (odd fallback: oddball-slug-extra-[0-9a-f]{8})
    if let Some(hex) = s.strip_prefix("oddball-slug-extra-") {
        return hex.len() == 8 && hex.bytes().all(|v| v.is_ascii_hexdigit());
    }
    let mut parts = s.split('-');
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let d = parts.next().unwrap_or("");
    let e = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return false;
    }
    let alpha = |x: &str| !x.is_empty() && x.bytes().all(|v| v.is_ascii_lowercase());
    if !alpha(a) || !alpha(b) || !alpha(c) || !alpha(d) {
        return false;
    }
    if e.len() != 4 {
        return false;
    }
    e.bytes().all(|v| v.is_ascii_hexdigit())
}

pub fn photo_slug(name: &str) -> Option<&str> {
    // ^gallery-slug_[0-9a-f]{32}_ (gallery-slug checked by is_slug)
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
    if !is_slug(slug) {
        return None;
    }
    Some(slug)
}

/// Canvas document caps. Body cap is enforced by the route (413/422 before
/// this parser runs); the rest here. See plan: raise in one place.
pub const CANVAS_MAX_BODY: usize = 256 * 1024;
pub const CANVAS_MAX_BLOCKS: usize = 200;
pub const CANVAS_TEXT_MAX: usize = 20_000;

/// Structural validation of a canvas document owned by `slug`.
/// `None` = invalid; `Some(srcs)` = valid, listing the media filenames the
/// caller must confirm exist on disk.
pub fn canvas_media(doc: &str, slug: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(doc).ok()?;
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let blocks = obj.get("blocks")?.as_array()?;
    if blocks.len() > CANVAS_MAX_BLOCKS {
        return None;
    }
    let mut srcs = Vec::new();
    for b in blocks {
        let b = b.as_object()?;
        if b.keys().any(|k| !matches!(k.as_str(), "id" | "kind" | "x" | "y" | "w" | "src" | "text" | "created")) {
            return None;
        }
        let id = b.get("id")?.as_str()?;
        if id.is_empty() || id.len() > 64 {
            return None;
        }
        let num = |k: &str, lo: f64, hi: f64| {
            b.get(k).and_then(|v| v.as_f64()).filter(|v| (lo..=hi).contains(v))
        };
        if num("x", 0.0, 100.0).is_none()
            || num("y", 0.0, 100_000.0).is_none()
            || num("w", 5.0, 100.0).is_none()
        {
            return None;
        }
        match b.get("kind")?.as_str()? {
            // Media must belong to this gallery: no cross-gallery references.
            "image" | "audio" => {
                let src = b.get("src")?.as_str()?;
                if photo_slug(src)? != slug {
                    return None;
                }
                srcs.push(src.to_string());
            }
            "text" => {
                if b.get("text")?.as_str()?.chars().count() > CANVAS_TEXT_MAX {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(srcs)
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

fn rand_below(n: u32) -> u32 {
    // Thread-safe: fresh OS randomness per draw (uuid v4).
    (uuid::Uuid::new_v4().as_u128() % (n as u128)) as u32
}

/// Four random words plus a short hex tail, e.g. `velvet-lagoon-quiet-river-1a2b`.
/// Uniqueness is enforced by the caller via DB check-and-retry.
pub fn gen_slug(db: &rusqlite::Connection) -> String {
    for _ in 0..50 {
        let s = format!(
            "{}-{}-{}-{}-{:04x}",
            ADJ[rand_below(ADJ.len() as u32) as usize],
            NOUN[rand_below(NOUN.len() as u32) as usize],
            ADJ[rand_below(ADJ.len() as u32) as usize],
            NOUN[rand_below(NOUN.len() as u32) as usize],
            uuid::Uuid::new_v4().as_u128() as u16
        );
        let taken: bool = db
            .query_row("SELECT 1 FROM galleries WHERE slug = ?1", [&s], |_| Ok(()))
            .is_ok();
        if !taken {
            return s;
        }
    }
    format!("oddball-slug-extra-{:08x}", uuid::Uuid::new_v4().as_u128() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts() {
        assert!(is_slug("velvet-lagoon-quiet-river-1a2b"));
        assert!(is_slug("blue-heron-granite-pond-00ff"));
        assert!(is_slug("a-b-c-d-1234"));
        assert!(is_slug("oddball-slug-extra-89abcdef"));
    }

    #[test]
    fn slug_rejects() {
        assert!(!is_slug("Happy-otter-calm-pond-1a2b")); // uppercase
        assert!(!is_slug("happy-otter-007")); // old short format
        assert!(!is_slug("happy-otter-calm-1a2b")); // old 3-word format
        assert!(!is_slug("happy-otter-calm-pond-12345")); // 5-hex
        assert!(!is_slug("happy-otter-calm-pond-123")); // 3-hex
        assert!(!is_slug("happy-otter-calm-pond-XYZ1")); // non-hex
        assert!(!is_slug("happy-otter-calm-pond")); // missing tail
        assert!(!is_slug("happy-otter-calm-pond-1a2b-x")); // extra part
        assert!(!is_slug("happy-ott3r-calm-pond-1a2b")); // digit in word
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
        assert!(normalize_key("abcdef").is_some());
        assert!(normalize_key("abcde").is_none());
        assert!(normalize_key(&"k".repeat(128)).is_some());
        assert!(normalize_key(&"k".repeat(129)).is_none());
        assert_eq!(normalize_key("  a-long-secret-key  "), Some("a-long-secret-key".to_string()));
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
        let name = format!("velvet-lagoon-quiet-river-1a2b_{}_photo.jpg", h);
        assert_eq!(photo_slug(&name), Some("velvet-lagoon-quiet-river-1a2b"));
    }

    #[test]
    fn photo_slug_bad() {
        assert!(photo_slug("nope.jpg").is_none());
        let h = "z".repeat(32); // not hex
        assert!(photo_slug(&format!("velvet-lagoon-quiet-river-1a2b_{}_x.jpg", h)).is_none());
        assert!(photo_slug("Happy-otter-calm-pond-1a2b_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg").is_none());
        // Old short galleries no longer resolve.
        assert!(photo_slug("happy-otter-7_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg").is_none());
        assert!(photo_slug("happy-otter-calm-pond-123_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg").is_none());
    }

    #[test]
    fn canvas_accepts_text_and_media() {
        let slug = "velvet-lagoon-quiet-river-1a2b";
        let media = format!("{slug}_{}_photo.jpg", "a".repeat(32));
        let doc = canvas_doc(&format!(
            r#"{{"id":"a","kind":"text","x":5,"y":20,"w":50,"text":"hello","created":1700000000000}},{{"id":"b","kind":"image","x":0,"y":0,"w":100,"src":"{media}"}}"#
        ));
        assert_eq!(canvas_media(&doc, slug), Some(vec![media.clone()]));
        // Audio references validate the same way (same filename shape).
        let doc = canvas_doc(&format!(
            r#"{{"id":"c","kind":"audio","x":0,"y":0,"w":50,"src":"{media}"}}"#
        ));
        assert_eq!(canvas_media(&doc, slug), Some(vec![media]));
    }

    fn canvas_doc(blocks: &str) -> String {
        format!(r#"{{"blocks":[{blocks}]}}"#)
    }

    #[test]
    fn canvas_rejects_bad_documents() {
        let slug = "velvet-lagoon-quiet-river-1a2b";
        let foreign = format!("plain-otter-calm-pond-9f9f_{}_x.jpg", "a".repeat(32));
        let cases = [
            String::new(),
            "[]".to_string(),
            r#"{"blocks":{}}"#.to_string(),
            r#"{"blocks":[],"extra":1}"#.to_string(),
            canvas_doc(r#"{"id":"a","kind":"video","x":0,"y":0,"w":50}"#),
            canvas_doc(r#"{"id":"a","kind":"text","x":101,"y":0,"w":50,"text":"hi"}"#),
            canvas_doc(r#"{"id":"a","kind":"text","x":0,"y":100001,"w":50,"text":"hi"}"#),
            canvas_doc(r#"{"id":"a","kind":"text","x":0,"y":0,"w":4,"text":"hi"}"#),
            canvas_doc(r#"{"id":"a","kind":"text","x":0,"y":0,"w":50}"#),
            canvas_doc(r#"{"id":"a","kind":"image","x":0,"y":0,"w":50}"#),
            canvas_doc(r#"{"id":"a","kind":"text","x":0,"y":0,"w":50,"text":"hi","z":1}"#),
            canvas_doc(&format!(
                r#"{{"id":"a","kind":"image","x":0,"y":0,"w":50,"src":"{foreign}"}}"#
            )),
            canvas_doc(&format!(
                r#"{{"id":"{}","kind":"text","x":0,"y":0,"w":50,"text":"hi"}}"#,
                "i".repeat(65)
            )),
        ];
        for c in cases {
            assert!(canvas_media(&c, slug).is_none(), "should reject: {c}");
        }
        // Text cap: exactly at the cap passes, one char over fails.
        let at = canvas_doc(&format!(
            r#"{{"id":"a","kind":"text","x":0,"y":0,"w":50,"text":{:?}}}"#,
            "x".repeat(CANVAS_TEXT_MAX)
        ));
        assert!(canvas_media(&at, slug).is_some());
        let over = canvas_doc(&format!(
            r#"{{"id":"a","kind":"text","x":0,"y":0,"w":50,"text":{:?}}}"#,
            "x".repeat(CANVAS_TEXT_MAX + 1)
        ));
        assert!(canvas_media(&over, slug).is_none());
        // Block cap: 200 blocks pass, 201 fail.
        let one = r#"{"id":"a","kind":"text","x":0,"y":0,"w":50,"text":"hi"}"#;
        let at = canvas_doc(&vec![one; CANVAS_MAX_BLOCKS].join(","));
        assert!(canvas_media(&at, slug).is_some());
        let over = canvas_doc(&vec![one; CANVAS_MAX_BLOCKS + 1].join(","));
        assert!(canvas_media(&over, slug).is_none());
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
}
