//! `gally-uring --backup <dest>`: consistent DB snapshot + photo copy.
//! VACUUM INTO takes a coherent copy while the server keeps writing.

use std::path::{Path, PathBuf};

use anyhow::Context;

fn copy_tree(from: &Path, to: &Path, files: &mut usize) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        for ent in std::fs::read_dir(&src)?.flatten() {
            let name = ent.file_name();
            // In-flight uploads are not photos yet; skip them.
            if name.to_string_lossy().starts_with(".part-") {
                continue;
            }
            let (s, d): (PathBuf, PathBuf) = (ent.path(), dst.join(&name));
            let md = ent.metadata()?;
            if md.is_dir() {
                std::fs::create_dir_all(&d)?;
                stack.push((s, d));
            } else if md.is_file() {
                std::fs::copy(&s, &d)
                    .with_context(|| format!("copy {}", s.display()))?;
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
    std::fs::create_dir_all(dest)?;
    // A backup holds password hashes and private photos: owner-only like the
    // live data dir, never world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700));
    }
    let db = dest.join("galleries.db");
    let tmp = dest.join("galleries.db.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let conn = rusqlite::Connection::open(root.join("galleries.db"))
            .context("open source database")?;
        if let Err(e) = conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()]) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("snapshot database");
        }
    }
    std::fs::rename(&tmp, &db).context("install database snapshot")?;
    // VACUUM INTO writes 0644: the snapshot holds password hashes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600));
    }
    let mut files = 0;
    let photos = dest.join("photos");
    copy_tree(&root.join("photos"), &photos, &mut files)?;
    // Deleted source photos must not linger in an overwritten backup.
    let mut pruned = 0;
    if let Ok(src_ents) = std::fs::read_dir(root.join("photos")) {
        let src: std::collections::HashSet<_> =
            src_ents.flatten().map(|e| e.file_name()).collect();
        if let Ok(dst_ents) = std::fs::read_dir(&photos) {
            for ent in dst_ents.flatten() {
                let name = ent.file_name();
                if name.to_string_lossy().starts_with('.') || src.contains(&name) {
                    continue;
                }
                if ent.path().is_file()
                    && std::fs::remove_file(ent.path()).is_ok()
                {
                    pruned += 1;
                }
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&photos, std::fs::Permissions::from_mode(0o700));
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

        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();

        assert!(dest.join("photos").join(&photo).exists());
        assert!(!dest.join("photos").join(".part-live").exists());
        assert!(!dest.join("galleries.db.tmp").exists());
        let copy = rusqlite::Connection::open(dest.join("galleries.db")).unwrap();
        let n: i64 = copy
            .query_row("SELECT COUNT(*) FROM galleries WHERE slug = ?1", [&slug], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "backup keeps the gallery row");
    }
}
