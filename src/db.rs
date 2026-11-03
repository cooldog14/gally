//! SQLite gallery metadata: galleries.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand_core::OsRng;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// Gallery-key digest. Keys are typed by hand, so this is a fast hash: anyone
/// holding a copy of `galleries.db` can wordlist them offline (viewer
/// passwords get Argon2 instead).
/// shortcut: no pepper; upgrading means HMAC under a server secret, which
/// invalidates every existing gallery — only worth it on a fresh install.
pub fn key_hash(key: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    h.finalize().to_vec()
}

/// Argon2id hash for a gallery viewer password (PHC string bytes).
pub fn hash_password(password: &str) -> Vec<u8> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string().into_bytes())
        .unwrap_or_default()
}

/// Verify a viewer password against a stored PHC hash.
pub fn verify_password(password: &str, hash: &[u8]) -> bool {
    let hash_str = match std::str::from_utf8(hash) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let parsed = match PasswordHash::new(hash_str) {
        Ok(p) => p,
        Err(_) => return false,
    };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
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
    // Order is load-bearing: the whole schema first (no-op on an old file),
    // then the legacy rebuild (drops any column it does not know), then the
    // column migrations. A fresh DB skips every migration path.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB, public INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0, photos INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX IF NOT EXISTS idx_galleries_key ON galleries(key_hash);",
    )?;
    // Migrate pre-password databases.
    let _ = conn.execute("ALTER TABLE galleries ADD COLUMN pass_hash BLOB", []);
    // Migrate pre-scoped-auth databases: same key may own many galleries now.
    // Old DDL carried `UNIQUE` on key_hash; rebuild the table without it.
    let legacy: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='galleries'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|ddl| ddl.contains("UNIQUE"))
        .unwrap_or(false);
    if legacy {
        conn.execute_batch(
            "CREATE TABLE galleries_new(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB);
             INSERT INTO galleries_new SELECT slug, key_hash, created, pass_hash FROM galleries;
             DROP TABLE galleries;
             ALTER TABLE galleries_new RENAME TO galleries;
             CREATE INDEX idx_galleries_key ON galleries(key_hash);",
        )?;
    }
    // Pre-existing galleries stay reachable: default 1 here, 0 on new inserts.
    let _ = conn.execute("ALTER TABLE galleries ADD COLUMN public INTEGER NOT NULL DEFAULT 1", []);
    let _ = conn.execute("ALTER TABLE galleries ADD COLUMN bytes INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE galleries ADD COLUMN photos INTEGER NOT NULL DEFAULT 0", []);
    Ok(conn)
}

/// Per-gallery counters + visibility, read in one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GalleryMeta {
    pub public: bool,
    pub bytes: i64,
    pub photos: i64,
}

/// Recount `galleries.bytes`/`.photos` from the files on disk. Runs at
/// startup: repairs counters after a crash and backfills pre-counter files.
pub fn recount(conn: &mut Connection, photos_dir: &Path) -> rusqlite::Result<()> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, (i64, i64)> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(photos_dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let Some(slug) = crate::validate::photo_slug(&name) else { continue };
            let md = match ent.metadata() {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            let e = counts.entry(slug.to_string()).or_default();
            e.0 += md.len() as i64;
            e.1 += 1;
        }
    }
    let tx = conn.transaction()?;
    tx.execute("UPDATE galleries SET bytes = 0, photos = 0", [])?;
    for (slug, (bytes, photos)) in counts {
        tx.execute(
            "UPDATE galleries SET bytes = ?1, photos = ?2 WHERE slug = ?3",
            rusqlite::params![bytes, photos, slug],
        )?;
    }
    tx.commit()
}

pub trait GalleryDb {
    /// Scoped auth: does this key own this slug? Same key may own many slugs.
    fn key_owns_slug(&self, key: &str, slug: &str) -> bool;
    /// Home upload: always mint a fresh gallery. `None` on DB failure.
    fn get_or_create_gallery(&mut self, key: &str) -> Option<String>;
    fn gallery_exists(&self, slug: &str) -> bool;
    /// Visibility + usage counters; `None` when the slug is unknown.
    fn gallery_meta(&self, slug: &str) -> Option<GalleryMeta>;
    /// Publish (true) or hide (false) the whole gallery. False when unknown.
    fn set_public(&mut self, slug: &str, public: bool) -> bool;
    /// Record one stored file: `(bytes, photos)` after the update.
    fn add_upload(&mut self, slug: &str, bytes: u64) -> Option<(i64, i64)>;
    /// Forget one removed file (saturating): `(bytes, photos)` after.
    fn remove_upload(&mut self, slug: &str, bytes: u64) -> Option<(i64, i64)>;
    /// Delete the row for `slug` only; the key keeps working.
    fn remove_gallery(&mut self, slug: &str) -> bool;
    /// (Empty galleries keep their row; see delete_photo.)
    /// Raw stored password hash, if the gallery is locked.
    fn gallery_pass_hash(&self, slug: &str) -> Option<Vec<u8>>;
    /// Set (Some) or clear (None) the gallery password. False when unknown.
    fn set_gallery_pass(&mut self, slug: &str, hash: Option<&[u8]>) -> bool;
}

impl GalleryDb for Connection {
    fn key_owns_slug(&self, key: &str, slug: &str) -> bool {
        let d = key_hash(key);
        self.query_row(
            "SELECT 1 FROM galleries WHERE slug = ?1 AND key_hash = ?2",
            rusqlite::params![slug, d],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Home upload: always mint a fresh gallery, never reuse.
    /// Same password, different visit = different gallery.
    fn get_or_create_gallery(&mut self, key: &str) -> Option<String> {
        let d = key_hash(key);
        for _ in 0..50 {
            let slug = crate::validate::gen_slug(self);
            match self.execute(
                "INSERT INTO galleries(slug, key_hash, created, public) VALUES(?1,?2,?3,0)",
                rusqlite::params![slug, d, now_secs()],
            ) {
                Ok(_) => return Some(slug),
                // Retry only on slug collision; other failures are real errors.
                Err(_) if self.gallery_exists(&slug) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    fn gallery_exists(&self, slug: &str) -> bool {
        self.query_row("SELECT 1 FROM galleries WHERE slug = ?1", [slug], |_| Ok(()))
            .is_ok()
    }

    fn gallery_meta(&self, slug: &str) -> Option<GalleryMeta> {
        self.query_row(
            "SELECT public, bytes, photos FROM galleries WHERE slug = ?1",
            [slug],
            |r| {
                Ok(GalleryMeta {
                    public: r.get::<_, i64>(0)? != 0,
                    bytes: r.get(1)?,
                    photos: r.get(2)?,
                })
            },
        )
        .ok()
    }

    fn set_public(&mut self, slug: &str, public: bool) -> bool {
        self.execute(
            "UPDATE galleries SET public = ?1 WHERE slug = ?2",
            rusqlite::params![public as i64, slug],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    fn add_upload(&mut self, slug: &str, bytes: u64) -> Option<(i64, i64)> {
        self.execute(
            "UPDATE galleries SET bytes = bytes + ?1, photos = photos + 1 WHERE slug = ?2",
            rusqlite::params![bytes as i64, slug],
        )
        .ok()?;
        self.gallery_meta(slug).map(|m| (m.bytes, m.photos))
    }

    fn remove_upload(&mut self, slug: &str, bytes: u64) -> Option<(i64, i64)> {
        self.execute(
            "UPDATE galleries SET bytes = MAX(bytes - ?1, 0), photos = MAX(photos - 1, 0) WHERE slug = ?2",
            rusqlite::params![bytes as i64, slug],
        )
        .ok()?;
        self.gallery_meta(slug).map(|m| (m.bytes, m.photos))
    }

    fn remove_gallery(&mut self, slug: &str) -> bool {
        self.execute("DELETE FROM galleries WHERE slug = ?1", [slug])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    fn gallery_pass_hash(&self, slug: &str) -> Option<Vec<u8>> {
        self.query_row("SELECT pass_hash FROM galleries WHERE slug = ?1", [slug], |r| {
            r.get::<_, Option<Vec<u8>>>(0)
        })
        .ok()
        .flatten()
    }

    fn set_gallery_pass(&mut self, slug: &str, hash: Option<&[u8]>) -> bool {
        self.execute("UPDATE galleries SET pass_hash = ?1 WHERE slug = ?2", rusqlite::params![hash, slug])
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
             CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB, public INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0, photos INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX idx_galleries_key ON galleries(key_hash);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn create_lookup_roundtrip() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("test-gallery-key-01").expect("create");
        assert!(db.gallery_exists(&slug));
        assert!(db.key_owns_slug("test-gallery-key-01", &slug));
        // Fresh mint each call: same key → new slug (home always creates).
        let slug2 = db.get_or_create_gallery("test-gallery-key-01").expect("create");
        assert_ne!(slug, slug2);
        assert!(db.key_owns_slug("test-gallery-key-01", &slug2));
    }

    #[test]
    fn fix_a_migration_idempotent() {
        let dir = std::env::temp_dir().join(format!("gally-db-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("g.db");
        let mut db = open(&path).expect("open");
        let slug = db.get_or_create_gallery("fix-a-key-01").expect("create");
        drop(db);
        let db2 = open(&path).expect("reopen");
        let tables: i64 = db2
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='galleries'", [], |r| r.get(0))
            .expect("count tables");
        assert_eq!(tables, 1);
        let rows: i64 = db2.query_row("SELECT COUNT(*) FROM galleries", [], |r| r.get(0)).expect("count rows");
        assert_eq!(rows, 1);
        assert!(db2.gallery_exists(&slug));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_creates_schema() {
        let dir = std::env::temp_dir().join(format!("gally-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = open(&dir.join("g.db")).expect("open");
        let slug = db.get_or_create_gallery("test-gallery-key-04").expect("create");
        assert!(db.gallery_exists(&slug));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_makes_existing_public_and_new_private() {
        let dir = std::env::temp_dir().join(format!("gally-db-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("g.db");
        let old_slug = "old-gallery-calm-pond-1a2b";
        {
            // Pre-visibility, pre-counter, pre-scoped-auth schema (UNIQUE key).
            let c = Connection::open(&path).unwrap();
            c.execute_batch(&format!(
                "CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL UNIQUE, created INTEGER NOT NULL, pass_hash BLOB);
                 CREATE INDEX idx_galleries_key ON galleries(key_hash);
                 INSERT INTO galleries(slug, key_hash, created) VALUES('{old_slug}', x'00', 1);"
            ))
            .unwrap();
        }
        let mut db = open(&path).expect("open");
        let meta = db.gallery_meta(old_slug).expect("old row migrates");
        assert!(meta.public, "existing galleries stay reachable");
        assert_eq!((meta.bytes, meta.photos), (0, 0));
        // New galleries start private.
        let fresh = db.get_or_create_gallery("mig-key-000001").expect("create");
        assert!(!db.gallery_meta(&fresh).expect("meta").public);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counters_track_uploads_and_removals() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("counters-key-01").expect("create");
        assert_eq!(db.add_upload(&slug, 40), Some((40, 1)));
        assert_eq!(db.add_upload(&slug, 60), Some((100, 2)));
        assert_eq!(db.remove_upload(&slug, 40), Some((60, 1)));
        // Saturating: counters never go negative.
        assert_eq!(db.remove_upload(&slug, 1_000), Some((0, 0)));
        assert_eq!(db.add_upload("no-such-gallery-aaaa", 1), None);
    }

    #[test]
    fn set_public_and_remove_gallery() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("public-key-0001").expect("create");
        assert!(db.set_public(&slug, true));
        assert!(db.gallery_meta(&slug).expect("meta").public);
        assert!(db.set_public(&slug, false));
        assert!(!db.gallery_meta(&slug).expect("meta").public);
        assert!(!db.set_public("no-such-gallery-aaaa", true));
        // Removing one row leaves the key usable for a new gallery.
        assert!(db.remove_gallery(&slug));
        assert!(!db.gallery_exists(&slug));
        assert!(!db.remove_gallery(&slug));
        assert!(db.get_or_create_gallery("public-key-0001").is_some());
    }

    #[test]
    fn recount_rebuilds_counters_from_disk() {
        let dir = std::env::temp_dir().join(format!("gally-db-recount-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let mut db = open(&dir.join("g.db")).expect("open");
        let slug = db.get_or_create_gallery("recount-key-001").expect("create");
        let hex = "a".repeat(32);
        let name = |tail: &str| format!("{slug}_{hex}_{tail}");
        std::fs::write(photos.join(name("a.jpg")), vec![1u8; 40]).unwrap();
        std::fs::write(photos.join(name("b.jpg")), vec![1u8; 60]).unwrap();
        // Junk is ignored: a stale temp and a file for an unknown gallery.
        std::fs::write(photos.join(".part-dead"), b"junk").unwrap();
        std::fs::write(photos.join(format!("ghost-gallery-calm-1a2b_{hex}_x.jpg")), b"zz").unwrap();
        assert_eq!(db.add_upload(&slug, 5), Some((5, 1)));
        recount(&mut db, &photos).expect("recount");
        let meta = db.gallery_meta(&slug).expect("meta");
        assert_eq!((meta.bytes, meta.photos), (100, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
