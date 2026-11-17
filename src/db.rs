//! SQLite gallery metadata: galleries.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
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
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fresh-install schema. Older files are upgraded by `migrate`.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB, public INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0, photos INTEGER NOT NULL DEFAULT 0, token_version INTEGER NOT NULL DEFAULT 0);
    CREATE INDEX IF NOT EXISTS idx_galleries_key ON galleries(key_hash);
    CREATE TABLE IF NOT EXISTS photos(slug TEXT NOT NULL, name TEXT NOT NULL, size INTEGER NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(slug, name));
    CREATE INDEX IF NOT EXISTS idx_photos_list ON photos(slug, created DESC, name DESC);";

/// Open (creating parent dirs as needed) and initialize the DB.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA busy_timeout=5000; {SCHEMA}"
    ))?;
    migrate(&conn)?;
    Ok(conn)
}

fn gallery_columns(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("PRAGMA table_info(galleries)")?;
    stmt.query_map([], |r| r.get::<_, String>(1))?.collect()
}

/// Bring pre-current data dirs up to `SCHEMA`. Every step is guarded by the
/// live column set, so re-running on a current DB is a no-op.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut cols = gallery_columns(conn)?;
    if !cols.contains("pass_hash") {
        conn.execute("ALTER TABLE galleries ADD COLUMN pass_hash BLOB", [])?;
        cols.insert("pass_hash".to_string());
    }
    // Pre-scoped-auth DDL carried a UNIQUE constraint on key_hash, so one key
    // could own only one gallery. Rebuild without it.
    let legacy_unique: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='galleries'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|ddl| ddl.contains("UNIQUE"))
        .unwrap_or(false);
    if legacy_unique {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE galleries_new(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB);
             INSERT INTO galleries_new SELECT slug, key_hash, created, pass_hash FROM galleries;
             DROP TABLE galleries;
             ALTER TABLE galleries_new RENAME TO galleries;
             CREATE INDEX idx_galleries_key ON galleries(key_hash);
             COMMIT;",
        )?;
        cols = gallery_columns(conn)?;
    }
    // Pre-visibility galleries were all public: default the added column to 1.
    for (name, decl) in [
        ("public", "INTEGER NOT NULL DEFAULT 1"),
        ("bytes", "INTEGER NOT NULL DEFAULT 0"),
        ("photos", "INTEGER NOT NULL DEFAULT 0"),
        ("token_version", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !cols.contains(name) {
            conn.execute(
                &format!("ALTER TABLE galleries ADD COLUMN {name} {decl}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// Per-gallery counters + visibility, read in one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GalleryMeta {
    pub public: bool,
    pub bytes: i64,
    pub photos: i64,
}

/// Recount `galleries.bytes`/`.photos` from the files on disk. Runs at
/// startup: repairs counters after a crash. A directory we cannot read aborts
/// the rebuild before the transaction, leaving the previous listing intact.
pub fn recount(conn: &mut Connection, photos_dir: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::collections::HashMap;
    let mut counts: HashMap<String, (i64, i64)> = HashMap::new();
    // Rows carry the capture time (EXIF backfill or upload time); the disk
    // only knows mtime, so keep what the DB already has for known files.
    let mut existing: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT name, created FROM photos")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (name, created) = row?;
            existing.insert(name, created);
        }
    }
    // The listing table is a projection of the directory: rebuild it, not
    // just the counters, so a crashed install gets its rows back.
    let mut files: Vec<(String, String, i64, i64)> = Vec::new();
    let rd = std::fs::read_dir(photos_dir)
        .with_context(|| format!("read photos dir {}", photos_dir.display()))?;
    for ent in rd {
        let ent = ent.with_context(|| format!("read photos dir {}", photos_dir.display()))?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(slug) = crate::validate::photo_slug(&name) else {
            continue;
        };
        let slug = slug.to_string();
        let md = match ent.metadata() {
            // Directories (`.thumbs`) and dotfiles never become rows.
            Ok(m) if m.is_file() => m,
            Ok(_) => continue,
            // Vanished between read_dir and stat: not a listing error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("stat {}", ent.path().display()));
            }
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let created = existing.get(&name).copied().unwrap_or(mtime);
        files.push((slug.clone(), name, md.len() as i64, created));
        let e = counts.entry(slug).or_default();
        e.0 += md.len() as i64;
        e.1 += 1;
    }
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM photos", [])?;
    {
        let mut ins =
            tx.prepare("INSERT INTO photos(slug, name, size, created) VALUES(?1, ?2, ?3, ?4)")?;
        for (slug, name, size, created) in &files {
            ins.execute(rusqlite::params![slug, name, size, created])?;
        }
    }
    tx.execute("UPDATE galleries SET bytes = 0, photos = 0", [])?;
    for (slug, (bytes, photos)) in counts {
        tx.execute(
            "UPDATE galleries SET bytes = ?1, photos = ?2 WHERE slug = ?3",
            rusqlite::params![bytes, photos, slug],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// One-time EXIF capture-date backfill for libraries created before the
/// `created` column carried capture time. `user_version = 1` records that
/// it ran; unreadable/no-EXIF files keep their prior timestamps and are not
/// retried. Idempotent if interrupted before the version bump.
pub fn backfill_exif(conn: &mut Connection, photos_dir: &Path) -> rusqlite::Result<usize> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= 1 {
        return Ok(0);
    }
    let mut names = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT name FROM photos")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            names.push(row?);
        }
    }
    let tx = conn.transaction()?;
    let mut updated = 0;
    for name in names {
        if let Some(ms) = crate::exif_time::taken_millis(&photos_dir.join(&name)) {
            tx.execute(
                "UPDATE photos SET created = ?1 WHERE name = ?2",
                rusqlite::params![ms, name],
            )?;
            updated += 1;
        }
    }
    tx.commit()?;
    // Outside the transaction: a crash before this reruns the (idempotent)
    // backfill next boot.
    conn.execute_batch("PRAGMA user_version = 1")?;
    Ok(updated)
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
    /// Record one stored file in the listing table and the counters, one
    /// transaction: `(bytes, photos)` after. `Ok(None)` when the gallery
    /// vanished; errors are DB failures, not a missing gallery.
    /// `created` is the sort timestamp (capture time, or upload time).
    fn add_upload(
        &mut self,
        slug: &str,
        name: &str,
        bytes: u64,
        created: i64,
    ) -> rusqlite::Result<Option<(i64, i64)>>;
    /// Forget one removed file and its listing row (saturating counters):
    /// `(bytes, photos)` after. `Ok(None)` when the gallery is unknown.
    fn remove_upload(
        &mut self,
        slug: &str,
        name: &str,
        bytes: u64,
    ) -> rusqlite::Result<Option<(i64, i64)>>;
    /// Delete the gallery row and every `photos` row for it, one transaction.
    /// The key keeps working.
    fn remove_gallery(&mut self, slug: &str) -> bool;
    /// One page, newest first. `before` is the `(created, name)` cursor.
    /// Returns `(created, name)` pairs.
    fn photos_page(
        &self,
        slug: &str,
        before: Option<(i64, String)>,
        limit: usize,
    ) -> rusqlite::Result<Vec<(i64, String)>>;
    /// Does this stored file still have a listing row?
    fn photo_exists(&self, slug: &str, name: &str) -> bool;
    /// (Empty galleries keep their row; see delete_photo.)
    /// Raw stored password hash, if the gallery is locked.
    fn gallery_pass_hash(&self, slug: &str) -> Option<Vec<u8>>;
    /// Set (Some) or clear (None) the gallery password. False when unknown.
    /// Also bumps `token_version`, revoking old viewer cookies.
    fn set_gallery_pass(&mut self, slug: &str, hash: Option<&[u8]>) -> bool;
    /// Current viewer-cookie generation; None when the slug is unknown.
    fn gallery_token_version(&self, slug: &str) -> Option<i64>;
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
        self.query_row(
            "SELECT 1 FROM galleries WHERE slug = ?1",
            [slug],
            |_| Ok(()),
        )
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

    fn add_upload(
        &mut self,
        slug: &str,
        name: &str,
        bytes: u64,
        created: i64,
    ) -> rusqlite::Result<Option<(i64, i64)>> {
        let tx = self.transaction()?;
        let n = tx.execute(
            "UPDATE galleries SET bytes = bytes + ?1, photos = photos + 1 WHERE slug = ?2",
            rusqlite::params![bytes as i64, slug],
        )?;
        // Row vanished mid-upload: drop the transaction (rollback).
        if n == 0 {
            return Ok(None);
        }
        tx.execute(
            "INSERT OR REPLACE INTO photos(slug, name, size, created) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![slug, name, bytes as i64, created],
        )?;
        let out = tx.query_row(
            "SELECT bytes, photos FROM galleries WHERE slug = ?1",
            [slug],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.commit()?;
        Ok(Some(out))
    }

    fn remove_upload(
        &mut self,
        slug: &str,
        name: &str,
        bytes: u64,
    ) -> rusqlite::Result<Option<(i64, i64)>> {
        let tx = self.transaction()?;
        let n = tx.execute(
            "UPDATE galleries SET bytes = MAX(bytes - ?1, 0), photos = MAX(photos - 1, 0) WHERE slug = ?2",
            rusqlite::params![bytes as i64, slug],
        )?;
        if n == 0 {
            return Ok(None);
        }
        tx.execute(
            "DELETE FROM photos WHERE slug = ?1 AND name = ?2",
            rusqlite::params![slug, name],
        )?;
        let out = tx.query_row(
            "SELECT bytes, photos FROM galleries WHERE slug = ?1",
            [slug],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.commit()?;
        Ok(Some(out))
    }

    fn remove_gallery(&mut self, slug: &str) -> bool {
        let Ok(tx) = self.transaction() else {
            return false;
        };
        let _ = tx.execute("DELETE FROM photos WHERE slug = ?1", [slug]);
        let n = tx
            .execute("DELETE FROM galleries WHERE slug = ?1", [slug])
            .unwrap_or(0);
        tx.commit().map(|_| n > 0).unwrap_or(false)
    }

    fn photos_page(
        &self,
        slug: &str,
        before: Option<(i64, String)>,
        limit: usize,
    ) -> rusqlite::Result<Vec<(i64, String)>> {
        let (created, name) = match &before {
            Some((c, n)) => (Some(*c), Some(n.as_str())),
            None => (None, None),
        };
        let mut stmt = self.prepare(
            "SELECT created, name FROM photos
             WHERE slug = ?1 AND (?2 IS NULL OR created < ?2 OR (created = ?2 AND name < ?3))
             ORDER BY created DESC, name DESC LIMIT ?4",
        )?;
        stmt.query_map(rusqlite::params![slug, created, name, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect()
    }

    fn photo_exists(&self, slug: &str, name: &str) -> bool {
        self.query_row(
            "SELECT 1 FROM photos WHERE slug = ?1 AND name = ?2",
            rusqlite::params![slug, name],
            |_| Ok(()),
        )
        .is_ok()
    }

    fn gallery_pass_hash(&self, slug: &str) -> Option<Vec<u8>> {
        self.query_row(
            "SELECT pass_hash FROM galleries WHERE slug = ?1",
            [slug],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        )
        .ok()
        .flatten()
    }

    fn set_gallery_pass(&mut self, slug: &str, hash: Option<&[u8]>) -> bool {
        self.execute(
            "UPDATE galleries SET pass_hash = ?1, token_version = token_version + 1 WHERE slug = ?2",
            rusqlite::params![hash, slug],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    fn gallery_token_version(&self, slug: &str) -> Option<i64> {
        self.query_row(
            "SELECT token_version FROM galleries WHERE slug = ?1",
            [slug],
            |r| r.get(0),
        )
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch(&format!("PRAGMA temp_store=MEMORY; {SCHEMA}"))
            .expect("schema");
        conn
    }

    #[test]
    fn create_lookup_roundtrip() {
        let mut db = mem();
        let slug = db
            .get_or_create_gallery("test-gallery-key-01")
            .expect("create");
        assert!(db.gallery_exists(&slug));
        assert!(db.key_owns_slug("test-gallery-key-01", &slug));
        // Fresh mint each call: same key → new slug (home always creates).
        let slug2 = db
            .get_or_create_gallery("test-gallery-key-01")
            .expect("create");
        assert_ne!(slug, slug2);
        assert!(db.key_owns_slug("test-gallery-key-01", &slug2));
    }

    #[test]
    fn open_creates_schema() {
        let dir = std::env::temp_dir().join(format!("gally-db-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db = open(&dir.join("g.db")).expect("open");
        let slug = db
            .get_or_create_gallery("test-gallery-key-04")
            .expect("create");
        assert!(db.gallery_exists(&slug));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_migrates_legacy_schemas() {
        let dir = std::env::temp_dir().join(format!("gally-db-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = "legacy-key-0001";
        let slug = "velvet-lagoon-quiet-river-1a2b";

        // Pre-scoped-auth: UNIQUE key_hash, no pass_hash/public/counters.
        let path = dir.join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL UNIQUE, created INTEGER NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO galleries(slug, key_hash, created) VALUES(?1, ?2, 1)",
                rusqlite::params![slug, key_hash(key)],
            )
            .unwrap();
        }
        let mut db = open(&path).expect("open migrates pre-scoped-auth db");
        recount(&mut db, &dir).expect("recount after migration");
        assert!(db.key_owns_slug(key, slug));
        assert!(db.gallery_meta(slug).expect("meta").public);
        assert!(
            db.get_or_create_gallery(key).is_some(),
            "same key must mint a second gallery after the UNIQUE rebuild"
        );

        // Pre-token_version: a locked gallery from the previous release.
        let path = dir.join("legacy2.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE galleries(slug TEXT PRIMARY KEY, key_hash BLOB NOT NULL, created INTEGER NOT NULL, pass_hash BLOB, public INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0, photos INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO galleries(slug, key_hash, created, public) VALUES(?1, X'00', 1, 1)",
                [slug],
            )
            .unwrap();
        }
        let mut db = open(&path).expect("open migrates pre-token_version db");
        assert_eq!(db.gallery_token_version(slug), Some(0));
        assert!(db.set_gallery_pass(slug, Some(&hash_password("Viewer-Pass-9"))));
        assert_eq!(
            db.gallery_token_version(slug),
            Some(1),
            "changing the password bumps the new column"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counters_track_uploads_and_removals() {
        let mut db = mem();
        let slug = db.get_or_create_gallery("counters-key-01").expect("create");
        assert_eq!(db.add_upload(&slug, "a.jpg", 40, 1_000), Ok(Some((40, 1))));
        assert_eq!(db.add_upload(&slug, "b.jpg", 60, 2_000), Ok(Some((100, 2))));
        assert_eq!(db.remove_upload(&slug, "a.jpg", 40), Ok(Some((60, 1))));
        // Saturating: counters never go negative.
        assert_eq!(db.remove_upload(&slug, "b.jpg", 1_000), Ok(Some((0, 0))));
        assert_eq!(
            db.add_upload("no-such-gallery-aaaa", "x.jpg", 1, 3_000),
            Ok(None)
        );
        // The listing rows tracked the same add/remove cycle.
        let rows: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE slug = ?1",
                [&slug],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
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
    fn recount_aborts_before_wiping_on_photos_dir_error() {
        let dir = std::env::temp_dir().join(format!("gally-db-recount-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = open(&dir.join("g.db")).expect("open");
        let slug = db
            .get_or_create_gallery("recount-error-key")
            .expect("create");
        assert_eq!(db.add_upload(&slug, "kept.jpg", 5, 500), Ok(Some((5, 1))));
        // read_dir on a regular file fails like a permission or mount error.
        let not_a_dir = dir.join("photos");
        std::fs::write(&not_a_dir, b"not a directory").unwrap();
        let err = recount(&mut db, &not_a_dir).unwrap_err();
        assert!(format!("{err:#}").contains("read photos dir"), "{err:#}");
        // Nothing was deleted: the listing and counters are unchanged.
        assert!(db.photo_exists(&slug, "kept.jpg"));
        assert_eq!(
            db.gallery_meta(&slug).expect("meta"),
            GalleryMeta {
                public: false,
                bytes: 5,
                photos: 1,
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // Junk is ignored: a stale temp, a thumb subdir, and a ghost gallery.
        std::fs::write(photos.join(".part-dead"), b"junk").unwrap();
        std::fs::create_dir_all(photos.join(".thumbs")).unwrap();
        std::fs::write(photos.join(".thumbs").join("junk.jpg"), b"junk").unwrap();
        std::fs::write(
            photos.join(format!("ghost-gallery-calm-1a2b_{hex}_x.jpg")),
            b"zz",
        )
        .unwrap();
        assert_eq!(db.add_upload(&slug, "stale.jpg", 5, 500), Ok(Some((5, 1))));
        // Capture times already in the DB survive the rebuild.
        assert_eq!(
            db.add_upload(&slug, &name("a.jpg"), 40, 12_345),
            Ok(Some((45, 2)))
        );
        recount(&mut db, &photos).expect("recount");
        let meta = db.gallery_meta(&slug).expect("meta");
        assert_eq!((meta.bytes, meta.photos), (100, 2));
        // The listing table was rebuilt (stale row gone, two real files in).
        let rows: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE slug = ?1",
                [&slug],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
        let created: i64 = db
            .query_row(
                "SELECT created FROM photos WHERE slug = ?1 AND name = ?2",
                rusqlite::params![slug, name("a.jpg")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, 12_345, "recount keeps the stored capture time");
        // .thumbs and dotfiles never become rows.
        let junk: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE name LIKE '.%' OR name = 'junk.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(junk, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_runs_once() {
        let dir = std::env::temp_dir().join(format!("gally-db-backfill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let mut db = open(&dir.join("g.db")).expect("open");
        let slug = db.get_or_create_gallery("backfill-key-01").expect("create");
        let name = format!("{slug}_{}_old.jpg", "b".repeat(32));
        // No such file: no EXIF, so nothing is rewritten.
        assert_eq!(db.add_upload(&slug, &name, 3, 1), Ok(Some((3, 1))));
        assert_eq!(backfill_exif(&mut db, &photos).expect("backfill"), 0);
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "the run is recorded");
        // A later call must not touch rows again.
        db.execute("UPDATE photos SET created = 999 WHERE slug = ?1", [&slug])
            .unwrap();
        assert_eq!(backfill_exif(&mut db, &photos).expect("backfill"), 0);
        let created: i64 = db
            .query_row(
                "SELECT created FROM photos WHERE slug = ?1 AND name = ?2",
                rusqlite::params![slug, name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, 999, "user_version guards the backfill");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
