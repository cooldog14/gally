//! SQLite gallery metadata: galleries + banned key hashes.

use rusqlite::{Connection, ErrorCode};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// sha256(key) digest stored in `galleries.key_hash` / `banned_keys.key_hash`.
pub fn key_hash(key: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    h.finalize().to_vec()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open (creating parent dirs as needed) and initialize the DB.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
         CREATE TABLE IF NOT EXISTS galleries(slug TEXT PRIMARY KEY, key_hash BLOB UNIQUE NOT NULL, created INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS banned_keys(key_hash BLOB PRIMARY KEY, created INTEGER NOT NULL);",
    )?;
    Ok(conn)
}

static ADJ: &[&str] = &[
    "amber", "arctic", "ashen", "autumn", "azure", "birch", "bold", "brave", "brisk", "bronze",
    "calm", "cedar", "chilly", "cinder", "clear", "cobalt", "comet", "copper", "coral", "crimson",
    "daring", "dawn", "ember", "frosty", "gentle", "glacier", "golden", "granite", "harbor",
    "hasty", "hazel", "honey", "ivory", "juniper", "kind", "lunar", "maple", "marble", "meadow",
    "misty", "mossy", "nimble", "noble", "ochre", "olive", "opal", "pebble", "pine", "plucky",
    "quartz", "quiet", "rapid", "redwood", "rocky", "ruby", "russet", "sandy", "silent", "silky",
    "silver", "sleepy", "solar", "spruce", "stormy", "sunny", "swift", "tawny", "tidal", "timber",
    "velvet", "verdant", "wheat", "willow", "wispy",
];
static NOUN: &[&str] = &[
    "acorn", "anchor", "arrow", "aspen", "badger", "basin", "beacon", "beaver", "brook", "burrow",
    "cabin", "canyon", "cavern", "cliff", "cove", "crane", "creek", "dune", "eagle", "elm",
    "falcon", "fern", "finch", "fjord", "fox", "garnet", "glade", "grove", "gull", "heron",
    "hill", "hollow", "inlet", "island", "jay", "kelp", "lagoon", "lark", "lichen", "lotus",
    "meadowlark", "mesa", "moth", "newt", "oasis", "otter", "owl", "oxbow", "peak", "pond",
    "poplar", "prairie", "quail", "raven", "reef", "ridge", "river", "robin", "sedge", "sparrow",
    "stone", "stork", "summit", "tern", "thrush", "trail", "trout", "tundra", "valley", "vole",
    "wren",
];

fn rand_below(n: u32) -> u32 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(1);
    static mut S: u64 = 0x9e37_79b9_7f4a_7c15;
    // Single-threaded event loop historically; keep the same xorshift.
    // (Worst case under contention is a repeated slug → IntegrityError retry.)
    let s = unsafe {
        S ^= S << 13;
        S ^= S >> 7;
        S ^= S << 17;
        S = S.wrapping_add(t);
        S
    };
    ((s >> 11) % (n as u64)) as u32
}

fn gen_slug(conn: &Connection) -> String {
    for _ in 0..50 {
        let s = format!(
            "{}-{}-{}",
            ADJ[rand_below(ADJ.len() as u32) as usize],
            NOUN[rand_below(NOUN.len() as u32) as usize],
            rand_below(999) + 1
        );
        let taken: bool = conn
            .query_row("SELECT 1 FROM galleries WHERE slug = ?1", [&s], |_| Ok(()))
            .is_ok();
        if !taken {
            return s;
        }
    }
    format!("oddball-slug-{}", rand_below(999) + 1)
}

/// Gallery helpers as methods on `rusqlite::Connection` (via trait import).
pub trait GalleryDb {
    fn is_banned(&self, digest: &[u8]) -> bool;
    fn lookup_gallery(&self, key: &str) -> Option<String>;
    fn get_or_create_gallery(&mut self, key: &str) -> Option<String>;
    fn gallery_exists(&self, slug: &str) -> bool;
    /// Move the gallery's key hash to `banned_keys` and delete the row.
    /// Returns false when `slug` is unknown.
    fn ban_gallery(&mut self, slug: &str) -> bool;
    fn unban(&mut self, digest: &[u8]) -> bool;
}

impl GalleryDb for Connection {
    fn is_banned(&self, digest: &[u8]) -> bool {
        self.query_row("SELECT 1 FROM banned_keys WHERE key_hash = ?1", [digest], |_| Ok(()))
            .is_ok()
    }

    fn lookup_gallery(&self, key: &str) -> Option<String> {
        let d = key_hash(key);
        if self.is_banned(&d) {
            return None;
        }
        self.query_row("SELECT slug FROM galleries WHERE key_hash = ?1", [&d], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    }

    fn get_or_create_gallery(&mut self, key: &str) -> Option<String> {
        let d = key_hash(key);
        if self.is_banned(&d) {
            return None;
        }
        if let Ok(s) = self.query_row(
            "SELECT slug FROM galleries WHERE key_hash = ?1",
            [&d],
            |r| r.get::<_, String>(0),
        ) {
            return Some(s);
        }
        for _ in 0..50 {
            let slug = gen_slug(self);
            match self.execute(
                "INSERT INTO galleries(slug, key_hash, created) VALUES(?1,?2,?3)",
                rusqlite::params![slug, d, now_secs()],
            ) {
                Ok(_) => return Some(slug),
                Err(e) => {
                    let constraint = matches!(&e,
                        rusqlite::Error::SqliteFailure(f, _)
                            if f.code == ErrorCode::ConstraintViolation
                    );
                    if constraint {
                        // Race: slug taken or key inserted concurrently — re-read.
                        if let Ok(s) = self.query_row(
                            "SELECT slug FROM galleries WHERE key_hash = ?1",
                            [&d],
                            |r| r.get::<_, String>(0),
                        ) {
                            return Some(s);
                        }
                        continue;
                    }
                    return None;
                }
            }
        }
        None
    }

    fn gallery_exists(&self, slug: &str) -> bool {
        self.query_row("SELECT 1 FROM galleries WHERE slug = ?1", [slug], |_| Ok(()))
            .is_ok()
    }

    fn ban_gallery(&mut self, slug: &str) -> bool {
        let digest: Option<Vec<u8>> = self
            .query_row("SELECT key_hash FROM galleries WHERE slug = ?1", [slug], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .ok();
        let Some(digest) = digest else { return false };
        let _ = self.execute("DELETE FROM galleries WHERE slug = ?1", [slug]);
        let _ = self.execute(
            "INSERT OR IGNORE INTO banned_keys(key_hash, created) VALUES(?1,?2)",
            rusqlite::params![digest, now_secs()],
        );
        true
    }

    fn unban(&mut self, digest: &[u8]) -> bool {
        self.execute("DELETE FROM banned_keys WHERE key_hash = ?1", [digest])
            .map(|n| n > 0)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch(
            "PRAGMA temp_store=MEMORY;
             CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB UNIQUE NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE banned_keys(key_hash BLOB PRIMARY KEY, created INTEGER NOT NULL);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn create_lookup_roundtrip() {
        let mut db = mem();
        assert_eq!(db.lookup_gallery("k1"), None);
        let slug = db.get_or_create_gallery("k1").expect("create");
        assert!(db.gallery_exists(&slug));
        assert_eq!(db.lookup_gallery("k1").as_deref(), Some(slug.as_str()));
        // Idempotent: same key → same slug.
        assert_eq!(db.get_or_create_gallery("k1").as_deref(), Some(slug.as_str()));
    }

    #[test]
    fn banned_blocks_resolve() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("k2").expect("create");
        assert!(db.ban_gallery(&slug));
        // Key hash moved to banned: lookups and creation both blocked.
        assert!(db.lookup_gallery("k2").is_none());
        assert!(db.get_or_create_gallery("k2").is_none());
        assert!(!db.gallery_exists(&slug));
        assert!(db.is_banned(&key_hash("k2")));
        // Unban restores ability to create (new slug).
        assert!(db.unban(&key_hash("k2")));
        assert!(!db.is_banned(&key_hash("k2")));
        assert!(db.get_or_create_gallery("k2").is_some());
    }

    #[test]
    fn ban_gallery_moves_row() {
        let mut db = mem();
        db.get_or_create_gallery("k3").expect("create");
        let slug = db.lookup_gallery("k3").unwrap();
        let digest = key_hash("k3");
        assert!(db.ban_gallery(&slug));
        assert!(!db.gallery_exists(&slug));
        assert!(db.is_banned(&digest));
        // Unknown slug → false.
        assert!(!db.ban_gallery("nope-not-real-1"));
        assert!(!db.unban(&key_hash("never-banned")));
    }

    #[test]
    fn open_creates_schema() {
        let dir = std::env::temp_dir().join(format!("gally-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = open(&dir.join("g.db")).expect("open");
        let slug = db.get_or_create_gallery("k4").expect("create");
        assert!(db.gallery_exists(&slug));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
