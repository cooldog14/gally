//! SQLite gallery metadata: galleries + banned key hashes.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand_core::OsRng;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

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
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB);
         CREATE INDEX IF NOT EXISTS idx_galleries_key ON galleries(key_hash);
         CREATE TABLE IF NOT EXISTS banned_keys(key_hash BLOB PRIMARY KEY, created INTEGER NOT NULL);",
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
    Ok(conn)
}

// Word lists live in `crate::validate` (single copy); slug randomness is
// `crate::validate::gen_slug` (uuid v4, thread-safe).
fn gen_slug(conn: &Connection) -> String {
    crate::validate::gen_slug(conn)
}

#[derive(Debug, PartialEq, Eq)]
pub enum MintError {
    Banned,
    Db,
}
pub trait GalleryDb {
    fn is_banned(&self, digest: &[u8]) -> bool;
    /// Scoped auth: does this key own this slug? Same key may own many slugs.
    fn key_owns_slug(&self, key: &str, slug: &str) -> bool;
    fn get_or_create_gallery(&mut self, key: &str) -> Result<String, MintError>;
    fn gallery_exists(&self, slug: &str) -> bool;
    /// (Empty galleries keep their row; see delete_photo.)
    /// Delete every row sharing the slug's key hash and ban the hash
    /// (same password = same ban). False when `slug` is unknown.
    fn ban_gallery(&mut self, slug: &str) -> bool;
    fn unban(&mut self, digest: &[u8]) -> bool;
    /// Raw stored password hash, if the gallery is locked.
    fn gallery_pass_hash(&self, slug: &str) -> Option<Vec<u8>>;
    /// Set (Some) or clear (None) the gallery password. False when unknown.
    fn set_gallery_pass(&mut self, slug: &str, hash: Option<&[u8]>) -> bool;
}

impl GalleryDb for Connection {
    fn is_banned(&self, digest: &[u8]) -> bool {
        self.query_row("SELECT 1 FROM banned_keys WHERE key_hash = ?1", [digest], |_| Ok(()))
            .is_ok()
    }

    fn key_owns_slug(&self, key: &str, slug: &str) -> bool {
        let d = key_hash(key);
        if self.is_banned(&d) {
            return false;
        }
        self.query_row(
            "SELECT 1 FROM galleries WHERE slug = ?1 AND key_hash = ?2",
            rusqlite::params![slug, d],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Home upload: always mint a fresh gallery, never reuse.
    /// Same password, different visit = different gallery.
    fn get_or_create_gallery(&mut self, key: &str) -> Result<String, MintError> {
        let d = key_hash(key);
        if self.is_banned(&d) {
            return Err(MintError::Banned);
        }
        for _ in 0..50 {
            let slug = gen_slug(self);
            match self.execute(
                "INSERT INTO galleries(slug, key_hash, created) VALUES(?1,?2,?3)",
                rusqlite::params![slug, d, now_secs()],
            ) {
                Ok(_) => return Ok(slug),
                // Retry only on slug collision; other failures are real errors.
                Err(_) if self.gallery_exists(&slug) => continue,
                Err(_) => return Err(MintError::Db),
            }
        }
        Err(MintError::Db)
    }

    fn gallery_exists(&self, slug: &str) -> bool {
        self.query_row("SELECT 1 FROM galleries WHERE slug = ?1", [slug], |_| Ok(()))
            .is_ok()
    }

    /// Ban by slug: deletes ALL galleries sharing that key and bans the hash.
    /// Same password elsewhere = same ban (keys are global secrets).
    fn ban_gallery(&mut self, slug: &str) -> bool {
        let digest: Option<Vec<u8>> = self
            .query_row("SELECT key_hash FROM galleries WHERE slug = ?1", [slug], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .ok();
        let Some(digest) = digest else { return false };
        let _ = self.execute("DELETE FROM galleries WHERE key_hash = ?1", [&digest]);
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
             CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB);
             CREATE INDEX idx_galleries_key ON galleries(key_hash);
             CREATE TABLE banned_keys(key_hash BLOB PRIMARY KEY, created INTEGER NOT NULL);",
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
    fn banned_blocks_resolve() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("test-gallery-key-02").expect("create");
        assert!(db.ban_gallery(&slug));
        // Key hash moved to banned: ownership checks and creation both blocked.
        assert!(!db.key_owns_slug("test-gallery-key-02", &slug));
        assert_eq!(db.get_or_create_gallery("test-gallery-key-02"), Err(MintError::Banned));
        assert!(!db.gallery_exists(&slug));
        assert!(db.is_banned(&key_hash("test-gallery-key-02")));
        // Unban restores ability to create (new slug).
        assert!(db.unban(&key_hash("test-gallery-key-02")));
        assert!(!db.is_banned(&key_hash("test-gallery-key-02")));
        assert!(db.get_or_create_gallery("test-gallery-key-02").is_ok());
    }

    #[test]
    fn ban_gallery_moves_row() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("test-gallery-key-03").expect("create");
        assert!(db.key_owns_slug("test-gallery-key-03", &slug));
        let digest = key_hash("test-gallery-key-03");
        assert!(db.ban_gallery(&slug));
        assert!(!db.gallery_exists(&slug));
        assert!(db.is_banned(&digest));
        // Unknown slug → false.
        assert!(!db.ban_gallery("nope-not-real-slug-aaaa"));
        assert!(!db.unban(&key_hash("never-banned-key-00")));
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
}
