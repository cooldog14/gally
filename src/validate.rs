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
    "acorn",
    "anchor",
    "arrow",
    "aspen",
    "badger",
    "basin",
    "beacon",
    "beaver",
    "brook",
    "burrow",
    "cabin",
    "canyon",
    "cavern",
    "cliff",
    "cove",
    "crane",
    "creek",
    "dune",
    "eagle",
    "elm",
    "falcon",
    "fern",
    "finch",
    "fjord",
    "fox",
    "garnet",
    "glade",
    "grove",
    "gull",
    "heron",
    "hill",
    "hollow",
    "inlet",
    "island",
    "jay",
    "kelp",
    "lagoon",
    "lark",
    "lichen",
    "lotus",
    "meadowlark",
    "mesa",
    "moth",
    "newt",
    "oasis",
    "otter",
    "owl",
    "oxbow",
    "peak",
    "pond",
    "poplar",
    "prairie",
    "quail",
    "raven",
    "reef",
    "ridge",
    "river",
    "robin",
    "sedge",
    "sparrow",
    "stone",
    "stork",
    "summit",
    "tern",
    "thrush",
    "trail",
    "trout",
    "tundra",
    "valley",
    "vole",
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
    let (hexpart, tail) = rest.split_at_checked(32)?;
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

pub fn sanitize_filename(raw: &str) -> (String, String) {
    let base0 = raw
        .rsplit('/')
        .next()
        .unwrap_or(raw)
        .rsplit('\\')
        .next()
        .unwrap_or(raw);
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

/// Percent-decode to raw bytes: `%41` → `A`, invalid escapes kept verbatim.
/// Bytes are NOT widened to chars: a UTF-8 password survives the trip.
pub fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut o = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            o.push(h * 16 + l);
            i += 3;
            continue;
        }
        o.push(b[i]);
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
    format!(
        "oddball-slug-extra-{:08x}",
        uuid::Uuid::new_v4().as_u128() as u32
    )
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
        assert!(normalize_key("Abc!x1").is_some());
        assert!(normalize_key("Abc!x").is_none());
        // Longer keys (old style, e.g. random hex) stay valid.
        assert!(normalize_key(&"k".repeat(12)).is_some());
        assert!(normalize_key(&"k".repeat(128)).is_some());
        assert!(normalize_key(&"k".repeat(129)).is_none());
        assert_eq!(
            normalize_key("  a-long-secret-key  "),
            Some("a-long-secret-key".to_string())
        );
    }

    #[test]
    fn decode_percent() {
        assert_eq!(percent_decode_bytes("a%20b"), b"a b");
        assert_eq!(percent_decode_bytes("%41%42"), b"AB");
        assert_eq!(percent_decode_bytes("100%25"), b"100%");
        assert_eq!(percent_decode_bytes("%ZZ"), b"%ZZ"); // invalid kept
        assert_eq!(percent_decode_bytes("trail%"), b"trail%");
        // Multi-byte UTF-8 must survive: bytes, not widened chars.
        assert_eq!(percent_decode_bytes("%C3%A4"), [0xC3, 0xA4]);
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
        assert!(
            photo_slug(&format!(
                "velvet-lagoon-quiet-river-1a2b_{}é_x.jpg",
                "a".repeat(31)
            ))
            .is_none()
        );
        let h = "z".repeat(32); // not hex
        assert!(photo_slug(&format!("velvet-lagoon-quiet-river-1a2b_{}_x.jpg", h)).is_none());
        assert!(
            photo_slug("Happy-otter-calm-pond-1a2b_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg")
                .is_none()
        );
        // Old short galleries no longer resolve.
        assert!(photo_slug("happy-otter-7_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg").is_none());
        assert!(
            photo_slug("happy-otter-calm-pond-123_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_x.jpg")
                .is_none()
        );
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
