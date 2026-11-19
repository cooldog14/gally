//! `gally-uring --backup <dest>`: consistent DB snapshot + photo copy.
//! VACUUM INTO takes a coherent copy while the server keeps writing.

use std::path::{Component, Path, PathBuf};

use anyhow::Context;

fn copy_tree(from: &Path, to: &Path, files: &mut usize) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        let rd = std::fs::read_dir(&src).with_context(|| format!("read {}", src.display()))?;
        for ent in rd {
            let ent = ent.with_context(|| format!("read entry in {}", src.display()))?;
            let name = ent.file_name().to_string_lossy().into_owned();
            // In-flight uploads and thumbnail temps are not photos yet.
            if name.starts_with(".part-") || name.starts_with(".tmp-") {
                continue;
            }
            let (s, d): (PathBuf, PathBuf) = (ent.path(), dst.join(&name));
            let md = ent
                .metadata()
                .with_context(|| format!("stat {}", s.display()))?;
            if md.is_dir() {
                std::fs::create_dir_all(&d)?;
                stack.push((s, d));
            } else if md.is_file() {
                copy_file(&s, &d)?;
                *files += 1;
            }
        }
        // Persist the directory entries before moving on to the next dir.
        sync_dir(&dst)?;
    }
    Ok(())
}

/// Copy one file to a temp name, fsync it, then rename it into place.
fn copy_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dst.with_file_name(format!(".tmp-{name}"));
    let out = (|| -> anyhow::Result<()> {
        std::fs::copy(src, &tmp).with_context(|| format!("copy {}", src.display()))?;
        sync_file(&tmp)?;
        std::fs::rename(&tmp, dst).with_context(|| format!("install {}", dst.display()))?;
        Ok(())
    })();
    if out.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    out
}

/// Force one file's data to disk.
fn sync_file(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("open {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("sync {}", path.display()))
}

/// Force a directory's entries to disk. Some filesystems reject this.
fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    match std::fs::File::open(dir).and_then(|f| f.sync_all()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(e) => Err(e).with_context(|| format!("sync {}", dir.display())),
    }
}

/// Make the snapshot's `photos` table agree with the backup folder.
/// Missing files are copied from `source` when present; rows whose file is
/// gone are removed. A backup must never list a photo it does not hold.
fn reconcile_photo_rows(
    db_path: &Path,
    source: Option<&Path>,
    photos_dir: &Path,
) -> anyhow::Result<(usize, usize)> {
    let mut db = rusqlite::Connection::open(db_path)
        .with_context(|| format!("open {}", db_path.display()))?;
    let rows: Vec<(String, String, i64)> = {
        let mut stmt = db.prepare("SELECT slug, name, size FROM photos")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut copied = 0;
    let mut missing: Vec<(String, String, i64)> = Vec::new();
    for (slug, name, size) in rows {
        let dst = photos_dir.join(&name);
        let dst_len = std::fs::metadata(&dst)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        if let Some(dst_len) = dst_len {
            // A truncated copy is repaired from the live source when possible.
            if let Some(src_dir) = source {
                let src = src_dir.join(&name);
                if let Ok(md) = std::fs::metadata(&src)
                    && md.is_file()
                    && md.len() != dst_len
                {
                    copy_file(&src, &dst)?;
                    copied += 1;
                }
            }
            continue;
        }
        if let Some(src_dir) = source {
            let src = src_dir.join(&name);
            if std::fs::metadata(&src).is_ok_and(|m| m.is_file()) {
                copy_file(&src, &dst)?;
                copied += 1;
                continue;
            }
        }
        missing.push((slug, name, size));
    }
    if copied > 0 {
        sync_dir(photos_dir)?;
    }
    if !missing.is_empty() {
        let tx = db.transaction()?;
        for (slug, name, size) in &missing {
            tx.execute(
                "DELETE FROM photos WHERE slug = ?1 AND name = ?2",
                rusqlite::params![slug, name],
            )?;
            tx.execute(
                "UPDATE galleries SET bytes = MAX(bytes - ?1, 0), photos = MAX(photos - 1, 0) WHERE slug = ?2",
                rusqlite::params![size, slug],
            )?;
        }
        tx.commit()?;
    }
    // The snapshot must be a single self-contained file before it is renamed.
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(db);
    sync_file(db_path)?;
    Ok((copied, missing.len()))
}

/// Refuses a destination inside the live data dir.
fn absolutize(p: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(p) {
        return canon;
    }
    // A destination that does not exist yet cannot be canonicalized; resolve
    // `..` lexically so it cannot dodge the live-dir guard.
    let abs = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(p);
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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
    // still the last good one.
    let mut files = 0;
    let source_photos = root.join("photos");
    let photos = dest.join("photos");
    copy_tree(&source_photos, &photos, &mut files)?;
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
    // A photo uploaded after the first copy but before this snapshot is in
    // the DB but not yet in the backup folder. Copy it now, and drop rows
    // whose file is gone, so the installed DB never lists a missing photo.
    let (copied, dropped) = match reconcile_photo_rows(&tmp, Some(&source_photos), &photos) {
        Ok(counts) => counts,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    files += copied;
    let mut dropped_rows = dropped;
    if let Err(e) = std::fs::rename(&tmp, &db) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("install database snapshot");
    }
    sync_dir(dest).context("sync backup dir")?;
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
    for ent in std::fs::read_dir(&source_photos).context("read source photos")? {
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
    // Pruning can remove a photo deleted from the source after the snapshot.
    // Reconcile again so the installed DB does not list a pruned file.
    let (copied, dropped) = reconcile_photo_rows(&db, Some(&source_photos), &photos)?;
    files += copied;
    dropped_rows += dropped;
    sync_dir(&photos).context("sync backup photos")?;
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
        "backup: {files} photos + galleries.db -> {} ({pruned} pruned, {dropped_rows} missing rows removed)",
        dest.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_reconciles_snapshot_rows_with_photos() {
        use crate::db::{GalleryDb, open};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let source_photos = root.join("photos");
        let backup_photos = tmp.path().join("dest").join("photos");
        std::fs::create_dir_all(&source_photos).unwrap();
        std::fs::create_dir_all(&backup_photos).unwrap();

        let db_path = root.join("galleries.db");
        let mut db = open(&db_path).unwrap();
        let slug = db.get_or_create_gallery("reconcile-key-0001").unwrap();
        let hex = "e".repeat(32);
        let copied = format!("{slug}_{hex}_copied.jpg");
        let dropped = format!("{slug}_{hex}_dropped.jpg");
        let kept = format!("{slug}_{hex}_kept.jpg");
        let repaired = format!("{slug}_{hex}_repaired.jpg");

        // A photo uploaded after the first copy: source exists, backup does not.
        std::fs::write(source_photos.join(&copied), vec![1u8; 10]).unwrap();
        db.add_upload(&slug, &copied, 10, 1).unwrap().unwrap();
        // A row whose file is gone everywhere.
        db.add_upload(&slug, &dropped, 20, 2).unwrap().unwrap();
        // Already in the backup; its source has since been deleted.
        std::fs::write(backup_photos.join(&kept), vec![3u8; 30]).unwrap();
        db.add_upload(&slug, &kept, 30, 3).unwrap().unwrap();
        // A truncated backup copy: re-copy it from the source.
        std::fs::write(source_photos.join(&repaired), vec![4u8; 15]).unwrap();
        std::fs::write(backup_photos.join(&repaired), vec![0u8; 1]).unwrap();
        db.add_upload(&slug, &repaired, 15, 4).unwrap().unwrap();

        let snap = tmp.path().join("snapshot.db");
        db.execute("VACUUM INTO ?1", [snap.to_string_lossy().as_ref()])
            .unwrap();
        drop(db);

        let (copied_count, dropped_count) =
            reconcile_photo_rows(&snap, Some(&source_photos), &backup_photos).unwrap();
        assert_eq!((copied_count, dropped_count), (2, 1));
        assert_eq!(
            std::fs::read(backup_photos.join(&copied)).unwrap(),
            vec![1u8; 10]
        );
        assert_eq!(
            std::fs::read(backup_photos.join(&repaired)).unwrap(),
            vec![4u8; 15],
            "a truncated copy is repaired from the source"
        );
        assert!(backup_photos.join(&kept).exists());

        let snap_db = open(&snap).unwrap();
        assert!(snap_db.photo_exists(&slug, &copied));
        assert!(!snap_db.photo_exists(&slug, &dropped));
        assert!(snap_db.photo_exists(&slug, &kept));
        assert!(snap_db.photo_exists(&slug, &repaired));
        let meta = snap_db.gallery_meta(&slug).unwrap().unwrap();
        assert_eq!((meta.bytes, meta.photos), (55, 3));
    }

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

    #[test]
    fn backup_refuses_dotdot_path_into_live_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir_all(root.join("photos")).unwrap();
        // The destination does not exist, so canonicalize fails; resolving
        // `..` lexically still lands inside the live data dir.
        let dest = tmp
            .path()
            .join("backups")
            .join("..")
            .join("data")
            .join("backups");
        let err = run(&root, &dest).unwrap_err();
        assert!(format!("{err:#}").contains("refusing"), "{err:#}");
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
            db.add_upload(&slug, &name, bytes.len() as u64, 1_000),
            Ok(Some((bytes.len() as i64, 1)))
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
        assert!(rdb.gallery_exists(&slug).expect("exists"));
        let meta = rdb
            .gallery_meta(&slug)
            .expect("query")
            .expect("gallery meta");
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
            Ok(Some((0, 0)))
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
            .expect("query")
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
        db.add_upload(&slug_a, &name_a, 1, 1_000).unwrap().unwrap();
        drop(db);
        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();

        // Gallery B appears only after that good backup.
        let mut db = crate::db::open(&db_path).unwrap();
        let slug_b = db.get_or_create_gallery("fail-key-b-0002").unwrap();
        let name_b = format!("{slug_b}_{hex}_b.jpg");
        std::fs::write(photos.join(&name_b), b"b").unwrap();
        db.add_upload(&slug_b, &name_b, 1, 1_000).unwrap().unwrap();
        drop(db);
        // copy(file -> existing dir) fails, exercising the photo-copy stage.
        let blocker = dest.join("photos").join(&name_b);
        std::fs::create_dir_all(&blocker).unwrap();
        let err = run(&root, &dest).unwrap_err();
        assert!(format!("{err:#}").contains("install"), "{err:#}");

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
