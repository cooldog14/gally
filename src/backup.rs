//! `gally-uring --backup <dest>`: consistent DB snapshot + photo copy.
//! VACUUM INTO takes a coherent copy while the server keeps writing.

use std::path::{Path, PathBuf};

use anyhow::Context;

fn copy_tree(from: &Path, to: &Path, files: &mut usize) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        for ent in std::fs::read_dir(&src)?.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            // In-flight uploads and thumbnail temps are not photos yet.
            if name.starts_with(".part-") || name.starts_with(".tmp-") {
                continue;
            }
            let (s, d): (PathBuf, PathBuf) = (ent.path(), dst.join(&name));
            let md = ent.metadata()?;
            if md.is_dir() {
                std::fs::create_dir_all(&d)?;
                stack.push((s, d));
            } else if md.is_file() {
                std::fs::copy(&s, &d).with_context(|| format!("copy {}", s.display()))?;
                *files += 1;
            }
        }
    }
    Ok(())
}

/// Refuses a destination inside the live data dir.
fn absolutize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    })
}
/// Copy `root`'s database and photos into `dest` (created if missing).
/// Overwrites an earlier backup in place; stale photos are pruned.
pub fn run(root: &Path, dest: &Path) -> anyhow::Result<()> {
    let root_c = absolutize(root);
    let dest_c = absolutize(dest);
    if dest_c == root_c
        || dest_c.starts_with(&root_c)
        || dest_c.join("galleries.db") == root_c.join("galleries.db")
    {
        anyhow::bail!("refusing to back up into the live data dir");
    }
    // The sidecar starts with the server: never snapshot a missing/empty DB
    // (or create one in the live data dir) while the app is still initializing.
    let src_db = root.join("galleries.db");
    if !src_db.is_file() {
        anyhow::bail!(
            "{} does not exist; server not initialized",
            src_db.display()
        );
    }
    let src = rusqlite::Connection::open(&src_db).context("open source database")?;
    let initialized: i64 = src
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='galleries'",
            [],
            |r| r.get(0),
        )
        .context("inspect source database")?;
    if initialized == 0 {
        anyhow::bail!(
            "{} has no schema; refusing to snapshot an uninitialized database",
            src_db.display()
        );
    }
    std::fs::create_dir_all(dest)?;
    // A backup holds password hashes and private photos: owner-only like the
    // live data dir, never world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700))
            .context("chmod backup dir")?;
    }
    // Photos first, additively: if the copy fails the previous DB snapshot is
    // still the last good one, never a DB pointing at missing bytes.
    let mut files = 0;
    let photos = dest.join("photos");
    copy_tree(&root.join("photos"), &photos, &mut files)?;
    // Only after the photos are in place: replace the DB snapshot.
    let db = dest.join("galleries.db");
    let tmp = dest.join("galleries.db.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        if let Err(e) = src.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()]) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("snapshot database");
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &db) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("install database snapshot");
    }
    // VACUUM INTO writes 0644: the snapshot holds password hashes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600))
            .context("chmod database snapshot")?;
    }
    // Deleted source photos must not linger in an overwritten backup. A dir
    // read error is fatal: we cannot prove the backup matches the source.
    let mut src = std::collections::HashSet::new();
    for ent in std::fs::read_dir(root.join("photos")).context("read source photos")? {
        src.insert(ent.context("read source photo entry")?.file_name());
    }
    let mut pruned = 0;
    let mut prune_failed = 0;
    for ent in std::fs::read_dir(&photos).context("read backup photos")? {
        let ent = ent.context("read backup photo entry")?;
        let name = ent.file_name();
        if name.to_string_lossy().starts_with('.') || src.contains(&name) {
            continue;
        }
        if !ent.path().is_file() {
            continue;
        }
        if std::fs::remove_file(ent.path()).is_ok() {
            pruned += 1;
        } else {
            prune_failed += 1;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&photos, std::fs::Permissions::from_mode(0o700))
            .context("chmod backup photos")?;
    }
    if prune_failed > 0 {
        anyhow::bail!(
            "backup: {prune_failed} stale photo(s) could not be pruned in {}",
            photos.display()
        );
    }
    println!(
        "backup: {files} photos + galleries.db -> {} ({pruned} pruned)",
        dest.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_copies_db_and_photos_without_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let photos = root.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let mut db = crate::db::open(&root.join("galleries.db")).unwrap();
        use crate::db::GalleryDb;
        let slug = db.get_or_create_gallery("backup-key-0001").unwrap();
        drop(db);
        let hex = "b".repeat(32);
        let photo = format!("{slug}_{hex}_one.jpg");
        std::fs::write(photos.join(&photo), b"bytes").unwrap();
        std::fs::write(photos.join(".part-live"), b"junk").unwrap();
        std::fs::write(photos.join(".tmp-live"), b"junk").unwrap();
        let thumbs = photos.join(".thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join(".tmp-live.jpg"), b"junk").unwrap();
        std::fs::write(thumbs.join(format!("{photo}.jpg")), b"thumb").unwrap();

        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();

        assert!(dest.join("photos").join(&photo).exists());
        assert!(!dest.join("photos").join(".part-live").exists());
        assert!(!dest.join("photos").join(".tmp-live").exists());
        assert!(!dest.join("photos/.thumbs/.tmp-live.jpg").exists());
        assert!(
            dest.join("photos/.thumbs")
                .join(format!("{photo}.jpg"))
                .exists(),
            "real thumbs are copied"
        );
        assert!(!dest.join("galleries.db.tmp").exists());
        let copy = rusqlite::Connection::open(dest.join("galleries.db")).unwrap();
        let n: i64 = copy
            .query_row(
                "SELECT COUNT(*) FROM galleries WHERE slug = ?1",
                [&slug],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "backup keeps the gallery row");
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for ent in std::fs::read_dir(from).unwrap().flatten() {
            let s = ent.path();
            let d = to.join(ent.file_name());
            if s.is_dir() {
                copy_dir(&s, &d);
            } else {
                std::fs::copy(&s, &d).unwrap();
            }
        }
    }

    #[test]
    fn restore_round_trip_and_prune() {
        use crate::db::GalleryDb;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let photos = root.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let db_path = root.join("galleries.db");
        let mut db = crate::db::open(&db_path).unwrap();
        let slug = db.get_or_create_gallery("restore-key-0001").unwrap();
        let hex = "c".repeat(32);
        let name = format!("{slug}_{hex}_one.jpg");
        let bytes = b"photo-bytes";
        std::fs::write(photos.join(&name), bytes).unwrap();
        assert_eq!(
            db.add_upload(&slug, &name, bytes.len() as u64),
            Some((bytes.len() as i64, 1))
        );
        std::fs::create_dir_all(photos.join(".thumbs")).unwrap();
        std::fs::write(photos.join(".thumbs").join(format!("{name}.jpg")), b"thumb").unwrap();
        drop(db);

        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();

        // Restore into a fresh root exactly like an operator would.
        let restored = tmp.path().join("restored");
        std::fs::create_dir_all(&restored).unwrap();
        std::fs::copy(dest.join("galleries.db"), restored.join("galleries.db")).unwrap();
        copy_dir(&dest.join("photos"), &restored.join("photos"));
        let mut rdb = crate::db::open(&restored.join("galleries.db")).unwrap();
        crate::db::recount(&mut rdb, &restored.join("photos")).unwrap();
        assert!(rdb.gallery_exists(&slug));
        let meta = rdb.gallery_meta(&slug).expect("gallery meta");
        assert_eq!((meta.bytes, meta.photos), (bytes.len() as i64, 1));
        let page = rdb.photos_page(&slug, None, 10).unwrap();
        assert!(page.iter().any(|(_, n)| n == &name), "{page:?}");
        assert!(rdb.photo_exists(&slug, &name));
        assert!(
            restored
                .join("photos/.thumbs")
                .join(format!("{name}.jpg"))
                .exists()
        );

        // Deleting the source photo (file + row) then backing up prunes it.
        std::fs::remove_file(photos.join(&name)).unwrap();
        let mut sdb = crate::db::open(&db_path).unwrap();
        assert_eq!(
            sdb.remove_upload(&slug, &name, bytes.len() as u64),
            Some((0, 0))
        );
        drop(sdb);
        run(&root, &dest).unwrap();
        assert!(!dest.join("photos").join(&name).exists(), "pruned");

        let restored2 = tmp.path().join("restored2");
        std::fs::create_dir_all(&restored2).unwrap();
        std::fs::copy(dest.join("galleries.db"), restored2.join("galleries.db")).unwrap();
        copy_dir(&dest.join("photos"), &restored2.join("photos"));
        let mut rdb2 = crate::db::open(&restored2.join("galleries.db")).unwrap();
        crate::db::recount(&mut rdb2, &restored2.join("photos")).unwrap();
        let meta = rdb2
            .gallery_meta(&slug)
            .expect("gallery row survives empty");
        assert_eq!((meta.bytes, meta.photos), (0, 0));
        assert!(rdb2.photos_page(&slug, None, 10).unwrap().is_empty());
        assert!(!rdb2.photo_exists(&slug, &name));
    }

    #[test]
    fn refuses_uninitialized_database() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("photos")).unwrap();
        let err = run(&root, &tmp.path().join("dest")).unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");

        // An empty SQLite file (created before the schema) is refused too.
        std::fs::write(root.join("galleries.db"), b"").unwrap();
        let err = run(&root, &tmp.path().join("dest2")).unwrap_err();
        assert!(format!("{err:#}").contains("no schema"), "{err:#}");
    }

    #[test]
    fn failed_photo_copy_keeps_previous_db() {
        use crate::db::GalleryDb;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let photos = root.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let db_path = root.join("galleries.db");
        let mut db = crate::db::open(&db_path).unwrap();
        let slug_a = db.get_or_create_gallery("fail-key-a-0001").unwrap();
        let hex = "d".repeat(32);
        let name_a = format!("{slug_a}_{hex}_a.jpg");
        std::fs::write(photos.join(&name_a), b"a").unwrap();
        db.add_upload(&slug_a, &name_a, 1).unwrap();
        drop(db);
        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();

        // Gallery B appears only after that good backup.
        let mut db = crate::db::open(&db_path).unwrap();
        let slug_b = db.get_or_create_gallery("fail-key-b-0002").unwrap();
        let name_b = format!("{slug_b}_{hex}_b.jpg");
        std::fs::write(photos.join(&name_b), b"b").unwrap();
        db.add_upload(&slug_b, &name_b, 1).unwrap();
        drop(db);
        // copy(file -> existing dir) fails, exercising the photo-copy stage.
        let blocker = dest.join("photos").join(&name_b);
        std::fs::create_dir_all(&blocker).unwrap();
        let err = run(&root, &dest).unwrap_err();
        assert!(format!("{err:#}").contains("copy"), "{err:#}");

        // The DB from the earlier good backup must still be the installed one.
        let prev = rusqlite::Connection::open(dest.join("galleries.db")).unwrap();
        let has = |slug: &str| -> i64 {
            prev.query_row(
                "SELECT COUNT(*) FROM galleries WHERE slug = ?1",
                [slug],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(has(&slug_a), 1, "old snapshot intact");
        assert_eq!(has(&slug_b), 0, "DB is installed only after photo copy");
        drop(prev);
        std::fs::remove_dir_all(&blocker).unwrap();
    }
}
