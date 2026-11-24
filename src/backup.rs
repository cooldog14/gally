//! `gally-uring --backup <dest>`: consistent DB snapshot + photo copy.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use rusqlite::{Connection, OpenFlags};

fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync directory {}", dir.display()))
}

fn entry(path: &Path) -> anyhow::Result<Option<fs::Metadata>> {
    match path.symlink_metadata() {
        Ok(md) => Ok(Some(md)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// Resolve actual ancestors before appending a missing suffix. A dangling
/// symlink or an unresolved `..` is not a safe place to create directories.
fn canonical_destination(path: &Path) -> anyhow::Result<PathBuf> {
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == ErrorKind::NotFound && entry(path)?.is_none() => {
            let leaf = path.file_name().with_context(|| {
                format!("refusing to resolve missing backup path {}", path.display())
            })?;
            Ok(canonical_destination(path.parent().context("no parent directory")?)?.join(leaf))
        }
        Err(e) => Err(e).with_context(|| format!("resolve {}", path.display())),
    }
}

#[cfg(unix)]
fn same_entry(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(unix)]
fn contains_alias(base: &Path, child: &Path) -> anyhow::Result<bool> {
    let Some(base) = entry(base)? else {
        return Ok(false);
    };
    for ancestor in child.ancestors() {
        if entry(ancestor)?.is_some_and(|md| same_entry(&base, &md)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_overlap(live: &Path, backup: &Path) -> anyhow::Result<()> {
    let overlap = live.starts_with(backup) || backup.starts_with(live);
    #[cfg(unix)]
    let overlap = overlap || contains_alias(live, backup)? || contains_alias(backup, live)?;
    ensure!(
        !overlap,
        "refusing backup path {}: overlaps live data at {}",
        backup.display(),
        live.display()
    );
    Ok(())
}

fn create_parent(dir: &Path) -> anyhow::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let parent = dir.parent().context("no backup parent directory")?;
    create_parent(parent)?;
    fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))?;
    sync_dir(parent)
}

/// Keep an open handle to the verified directory so only that version can be
/// cleaned up after exchange. An empty directory is also a valid first target.
fn open_destination(dest: &Path) -> anyhow::Result<Option<File>> {
    let Some(md) = entry(dest)? else {
        return Ok(None);
    };
    ensure!(md.is_dir(), "backup destination must be a real directory");
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let dir = options.open(dest)?;
    let mut db = false;
    let mut photos = false;
    for child in fs::read_dir(dest)? {
        let child = child?;
        let kind = child.file_type()?;
        match child.file_name().to_str() {
            Some("galleries.db") if kind.is_file() => db = true,
            Some("photos") if kind.is_dir() => photos = true,
            _ => anyhow::bail!(
                "backup destination contains an unexpected entry: {}",
                child.path().display()
            ),
        }
    }
    ensure!(
        db == photos,
        "backup destination is not a complete Gally snapshot"
    );
    if db {
        // Immutable open cannot create journals or shared-memory files in the old snapshot.
        let path = dest.join("galleries.db");
        let mut uri = String::from("file:");
        for c in path.to_str().context("backup paths must be UTF-8")?.chars() {
            match c {
                '%' => uri.push_str("%25"),
                '?' => uri.push_str("%3F"),
                '#' => uri.push_str("%23"),
                _ => uri.push(c),
            }
        }
        uri.push_str("?immutable=1");
        let check = || -> anyhow::Result<()> {
            let db = Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )?;
            db.prepare("SELECT slug, key_hash, created, bytes, photos FROM galleries LIMIT 0")?;
            db.prepare("SELECT slug, name, size, created FROM photos LIMIT 0")?;
            Ok(())
        };
        check().context("backup destination is not a Gally snapshot")?;
    }
    Ok(Some(dir))
}

/// The source can vanish during a live delete. Other failures must not be
/// confused with a missing row, especially a failed write to the stage.
fn copy_live_file(src: &Path, dst: &Path, expected: Option<u64>) -> anyhow::Result<bool> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut input = match options.open(src) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("open {} for backup", src.display())),
    };
    ensure!(
        input.metadata()?.is_file(),
        "not a regular backup source file: {}",
        src.display()
    );
    let mut output = File::create_new(dst).with_context(|| format!("create {}", dst.display()))?;
    let len = std::io::copy(&mut input, &mut output)
        .with_context(|| format!("copy {}", src.display()))?;
    ensure!(
        expected.is_none_or(|size| size == len),
        "size changed while backing up {}",
        src.display()
    );
    output
        .sync_all()
        .with_context(|| format!("sync {}", dst.display()))?;
    Ok(true)
}

fn build_snapshot(
    src: &Connection,
    source_photos: &Path,
    stage: &Path,
) -> anyhow::Result<(usize, usize)> {
    let snapshot = stage.join("galleries.db");
    src.execute(
        "VACUUM INTO ?1",
        [snapshot.to_str().context("backup paths must be UTF-8")?],
    )
    .context("snapshot database")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600))?;
    }

    let photos = stage.join("photos");
    fs::create_dir(&photos)?;
    let live_thumbs = source_photos.join(".thumbs");
    let thumbs = photos.join(".thumbs");
    let has_thumbs = if let Some(md) = entry(&live_thumbs)? {
        ensure!(md.is_dir(), "live .thumbs is not a real directory");
        fs::create_dir(&thumbs)?;
        true
    } else {
        false
    };
    let mut db = Connection::open(&snapshot)?;
    // A sidecar that runs during the first upgrade must not publish an index
    // the startup recount has not filled yet.
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .context("inspect staged database version")?;
    ensure!(
        version >= 1,
        "live database startup recount not finished; start Gally and retry"
    );
    let tx = db.transaction()?;
    // Only the snapshotted index authorizes a copy. Extra originals in a
    // snapshot could become live again when restore recounts the directory.
    tx.execute(
        "DELETE FROM photos WHERE slug NOT IN (SELECT slug FROM galleries)",
        [],
    )?;
    let mut copied = 0;
    let mut indexed = 0;
    let mut missing = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT slug, name, size FROM photos")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let slug = row.get_ref(0)?.as_str()?;
            let name = row.get_ref(1)?.as_str()?;
            let size: i64 = row.get(2)?;
            ensure!(
                !name.contains(['/', '\\']) && crate::validate::photo_slug(name) == Some(slug),
                "invalid photo name in snapshot"
            );
            indexed += 1;
            if !copy_live_file(
                &source_photos.join(name),
                &photos.join(name),
                Some(u64::try_from(size)?),
            )? {
                missing.push((slug.to_owned(), name.to_owned(), size));
                continue;
            }
            copied += 1;
            if has_thumbs {
                let name = format!("{name}.jpg");
                copy_live_file(&live_thumbs.join(&name), &thumbs.join(&name), None)?;
            }
        }
    }
    // One deleted original is normal during a live delete. None of the
    // indexed originals is a missing mount or wrong restore; do not overwrite
    // the previous snapshot with an empty one.
    if indexed > 0 && copied == 0 {
        anyhow::bail!(
            "none of the {indexed} indexed photos are present in {}; refusing to publish an empty snapshot",
            source_photos.display()
        );
    }
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
    db.close().map_err(|(_, e)| e)?;
    File::open(&snapshot)?
        .sync_all()
        .context("sync database snapshot")?;
    if has_thumbs {
        sync_dir(&thumbs)?;
    }
    sync_dir(&photos)?;
    sync_dir(stage)?;
    Ok((copied, missing.len()))
}

#[cfg(unix)]
fn same_directory(path: &Path, opened: &File) -> anyhow::Result<bool> {
    let md = path.symlink_metadata()?;
    Ok(md.is_dir() && same_entry(&md, &opened.metadata()?))
}

#[cfg(target_os = "linux")]
fn check_no_mounts(dir: &Path, mountinfo: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    // Linux mountinfo quotes these bytes in mount-point paths.
    let mut escaped = Vec::with_capacity(dir.as_os_str().len());
    for &b in dir.as_os_str().as_bytes() {
        match b {
            b' ' => escaped.extend_from_slice(b"\\040"),
            b'\t' => escaped.extend_from_slice(b"\\011"),
            b'\n' => escaped.extend_from_slice(b"\\012"),
            b'\\' => escaped.extend_from_slice(b"\\134"),
            _ => escaped.push(b),
        }
    }
    for line in mountinfo
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let point = line
            .split(|&b| b == b' ')
            .nth(4)
            .context("invalid mountinfo entry")?;
        if let Some(suffix) = point.strip_prefix(escaped.as_slice()) {
            ensure!(
                !(suffix.is_empty() || suffix.starts_with(b"/")),
                "refusing to replace or remove a backup with mounts at or under {}",
                dir.display()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_backup_dir(stage: &Path, dest: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cstr = |path: &Path| {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backup path contains a NUL byte",
            )
        })
    };
    let stage = cstr(stage)?;
    let dest = cstr(dest)?;
    // SAFETY: both pointers are valid C strings for the duration of the syscall.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            dest.as_ptr(),
            flags,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn exchange(stage: &Path, dest: &Path, old: &File) -> anyhow::Result<()> {
    ensure!(
        same_directory(dest, old)?,
        "backup destination changed during staging"
    );
    check_no_mounts(
        dest,
        &fs::read("/proc/self/mountinfo").context("inspect backup mounts")?,
    )?;
    rename_backup_dir(stage, dest, libc::RENAME_EXCHANGE)
        .context("atomic backup exchange (renameat2 RENAME_EXCHANGE) failed; previous backup kept")
}

#[cfg(not(target_os = "linux"))]
fn exchange(_: &Path, _: &Path, _: &File) -> anyhow::Result<()> {
    anyhow::bail!(
        "atomic replacement of an existing backup requires Linux RENAME_EXCHANGE; previous backup kept"
    )
}

fn cleanup(stage: &Path) {
    let remove = || -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        check_no_mounts(
            stage,
            &fs::read("/proc/self/mountinfo").context("inspect backup mounts")?,
        )?;
        fs::remove_dir_all(stage)?;
        Ok(())
    };
    if let Err(e) = remove() {
        eprintln!("backup: could not clean {}: {e}", stage.display());
    }
}

/// Copy the database and indexed photos into a private sibling, then publish.
pub fn run(root: &Path, dest: &Path) -> anyhow::Result<()> {
    let root = root.canonicalize().context("resolve live data directory")?;
    let live_photos = canonical_destination(&root.join("photos"))?;
    let supplied: PathBuf = std::path::absolute(dest)?.components().collect();
    let dest = canonical_destination(&supplied)?;
    let backup_photos = canonical_destination(&dest.join("photos"))?;
    for path in [&dest, &backup_photos] {
        check_overlap(&root, path)?;
        check_overlap(&live_photos, path)?;
    }
    ensure!(
        entry(&supplied)?.is_none_or(|md| md.is_dir()),
        "backup destination must be a real directory"
    );

    // The sidecar can start before the app has initialized its database.
    let src_db = root.join("galleries.db");
    ensure!(
        src_db.is_file(),
        "{} does not exist; server not initialized",
        src_db.display()
    );
    let src_db = src_db.canonicalize().context("resolve live database")?;
    check_overlap(&src_db, &dest)?;
    check_overlap(&src_db, &backup_photos)?;
    let old = open_destination(&dest)?;
    let src = Connection::open_with_flags(&src_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open source database")?;
    let initialized: bool = src
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='galleries')",
            [],
            |r| r.get(0),
        )
        .context("inspect source database")?;
    ensure!(
        initialized,
        "{} has no schema; refusing to snapshot an uninitialized database",
        src_db.display()
    );
    ensure!(live_photos.is_dir(), "live photos directory is missing");
    #[cfg(unix)]
    let live_photos_open = {
        let dir = File::open(&live_photos).context("open live photos directory")?;
        ensure!(
            dir.metadata()?.is_dir(),
            "live photos path is not a directory"
        );
        dir
    };

    let parent = dest.parent().context("backup destination has no parent")?;
    create_parent(parent)?;
    let stage = parent.join(format!(".gally-stage-{}", uuid::Uuid::new_v4()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&stage)
        .context("create private backup stage")?;

    let publish = || -> anyhow::Result<_> {
        let counts = build_snapshot(&src, &live_photos, &stage)?;
        // A replaced photo tree would read from a different source than inspected.
        #[cfg(unix)]
        ensure!(
            same_directory(&live_photos, &live_photos_open)?,
            "live photos directory changed during staging"
        );
        sync_dir(parent)?;
        if let Some(old) = &old {
            exchange(&stage, &dest, old)?;
        } else {
            ensure!(
                entry(&dest)?.is_none(),
                "backup destination appeared during staging"
            );
            #[cfg(target_os = "linux")]
            rename_backup_dir(&stage, &dest, libc::RENAME_NOREPLACE)
                .context("publish first backup")?;
            #[cfg(not(target_os = "linux"))]
            fs::rename(&stage, &dest).context("publish first backup")?;
        }
        Ok(counts)
    };
    let (copied, missing) = match publish() {
        Ok(counts) => counts,
        Err(e) => {
            cleanup(&stage);
            return Err(e);
        }
    };
    // After the exchange the stage name holds the old version, not our copy.
    sync_dir(parent).with_context(|| {
        format!(
            "backup renamed but not durably published; any previous version is retained at {}",
            stage.display()
        )
    })?;
    #[cfg(target_os = "linux")]
    if let Some(old) = old {
        if same_directory(&stage, &old).unwrap_or(false) {
            cleanup(&stage);
        } else {
            eprintln!(
                "backup: leaving unrecognized previous directory at {}",
                stage.display()
            );
        }
    }
    println!(
        "backup: {copied} photos + galleries.db -> {} ({missing} missing rows removed)",
        dest.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_drops_indexed_rows_whose_source_vanished() {
        use crate::db::{GalleryDb, open};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("photos/.thumbs")).unwrap();
        let mut db = open(&root.join("galleries.db")).unwrap();
        let slug = db.get_or_create_gallery("reconcile-key-0001").unwrap();
        let hex = "e".repeat(32);
        let kept = format!("{slug}_{hex}_kept.jpg");
        let dropped = format!("{slug}_{hex}_dropped.jpg");
        fs::write(root.join("photos").join(&kept), b"original").unwrap();
        fs::write(
            root.join("photos/.thumbs").join(format!("{dropped}.jpg")),
            b"stale",
        )
        .unwrap();
        db.add_upload(&slug, &kept, 8, 1).unwrap().unwrap();
        db.add_upload(&slug, &dropped, 20, 2).unwrap().unwrap();
        crate::db::backfill_exif(&mut db, &root.join("photos")).unwrap();

        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();
        let snap = open(&dest.join("galleries.db")).unwrap();
        assert!(!snap.photo_exists(&slug, &dropped));
        assert!(snap.photo_exists(&slug, &kept));
        assert_eq!(
            fs::read(dest.join("photos").join(&kept)).unwrap(),
            b"original"
        );
        assert!(
            !dest
                .join("photos/.thumbs")
                .join(format!("{dropped}.jpg"))
                .exists()
        );
        let meta = snap.gallery_meta(&slug).unwrap().unwrap();
        assert_eq!((meta.bytes, meta.photos), (8, 1));
        let live = db.gallery_meta(&slug).unwrap().unwrap();
        assert_eq!(
            (live.bytes, live.photos),
            (28, 2),
            "only the snapshot is reconciled"
        );
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
        let hex = "b".repeat(32);
        let photo = format!("{slug}_{hex}_one.jpg");
        std::fs::write(photos.join(&photo), b"bytes").unwrap();
        db.add_upload(&slug, &photo, 5, 1).unwrap().unwrap();
        crate::db::backfill_exif(&mut db, &photos).unwrap();
        drop(db);
        std::fs::write(photos.join(".part-live"), b"junk").unwrap();
        std::fs::write(photos.join(".tmp-live"), b"junk").unwrap();
        let thumbs = photos.join(".thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join(".tmp-live.jpg"), b"junk").unwrap();
        std::fs::write(thumbs.join(format!("{photo}.jpg")), b"thumb").unwrap();

        let dest = tmp.path().join("new-parent").join("dest");
        run(&root, &dest).unwrap();

        assert_eq!(
            fs::read(dest.join("photos").join(&photo)).unwrap(),
            b"bytes"
        );
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(dest.metadata().unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(
                dest.join("galleries.db")
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
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

    #[cfg(unix)]
    #[test]
    fn backup_path_identity_detects_aliased_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        let alias = tmp.path().join("alias");
        fs::write(&live, b"data").unwrap();
        fs::hard_link(&live, &alias).unwrap();
        assert!(check_overlap(&live, &alias).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn backup_mount_guard_matches_escaped_path_boundaries() {
        let mounts = b"51 28 8:1 /data /backups\\040here/current/photos rw - ext4 /dev/sda1 rw\n";
        for path in ["/backups here/current", "/backups here/current/photos"] {
            assert!(check_no_mounts(Path::new(path), mounts).is_err());
        }
        assert!(check_no_mounts(Path::new("/backups here/cur"), mounts).is_ok());
    }

    #[test]
    fn backup_refuses_live_path_overlap_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let root = parent.join("data");
        fs::create_dir_all(root.join("photos")).unwrap();
        for dest in [
            &parent,
            &root,
            &root.join("missing"),
            &parent.join("../parent/data/backups"),
        ] {
            let err = run(&root, dest).unwrap_err();
            assert!(format!("{err:#}").contains("overlaps live data"), "{err:#}");
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn backup_resolves_symlinked_photo_trees_and_missing_leaf() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let external = tmp.path().join("external");
        let photos = external.join("originals");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(&photos).unwrap();
        symlink(&photos, root.join("photos")).unwrap();
        let alias = tmp.path().join("photo-alias");
        symlink(&photos, &alias).unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        symlink(&root, dest.join("photos")).unwrap();
        for target in [
            &external,
            &alias.join("new"),
            &alias.join("../originals/new"),
            &dest,
        ] {
            let err = run(&root, target).unwrap_err();
            assert!(format!("{err:#}").contains("overlaps live data"), "{err:#}");
        }
        assert_eq!(fs::read_dir(&photos).unwrap().count(), 0);
        assert!(dest.join("photos").is_symlink());
    }

    #[test]
    fn backup_does_not_replace_unrelated_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("photos")).unwrap();
        crate::db::open(&root.join("galleries.db")).unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(dest.join("photos")).unwrap();
        fs::write(dest.join("galleries.db"), b"not a database").unwrap();
        assert!(format!("{:#}", run(&root, &dest).unwrap_err()).contains("not a Gally snapshot"));
        fs::write(dest.join("keep-me"), b"unrelated").unwrap();
        assert!(format!("{:#}", run(&root, &dest).unwrap_err()).contains("unexpected entry"));
        assert_eq!(fs::read(dest.join("keep-me")).unwrap(), b"unrelated");
        assert_eq!(
            fs::read(dest.join("galleries.db")).unwrap(),
            b"not a database"
        );
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn restore_round_trip_after_snapshot_replacement() {
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
        crate::db::backfill_exif(&mut db, &photos).unwrap();
        std::fs::create_dir_all(photos.join(".thumbs")).unwrap();
        std::fs::write(photos.join(".thumbs").join(format!("{name}.jpg")), b"thumb").unwrap();
        drop(db);

        let dest = tmp.path().join("dest %#?");
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

        // Once absent from the index, leftover originals/previews must not
        // be carried into the replacement and resurrected by recount.
        let mut sdb = crate::db::open(&db_path).unwrap();
        assert_eq!(
            sdb.remove_upload(&slug, &name, bytes.len() as u64),
            Ok(Some((0, 0)))
        );
        drop(sdb);
        run(&root, &dest).unwrap();
        assert!(!dest.join("photos").join(&name).exists());
        assert!(
            !dest
                .join("photos/.thumbs")
                .join(format!("{name}.jpg"))
                .exists()
        );
        assert!(
            photos.join(&name).exists(),
            "backup never prunes source files"
        );
        assert!(
            !fs::read_dir(tmp.path())
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(".gally-stage-"))
        );

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

        // A real schema without the startup-recount marker is not publishable.
        let unseeded = tmp.path().join("unseeded");
        std::fs::create_dir_all(unseeded.join("photos")).unwrap();
        crate::db::open(&unseeded.join("galleries.db")).unwrap();
        let dest3 = tmp.path().join("dest3");
        std::fs::create_dir(&dest3).unwrap();
        let err = run(&unseeded, &dest3).unwrap_err();
        assert!(
            format!("{err:#}").contains("startup recount not finished"),
            "{err:#}"
        );
        assert_eq!(fs::read_dir(&dest3).unwrap().count(), 0);
        assert!(
            !fs::read_dir(tmp.path())
                .unwrap()
                .flatten()
                .any(|e| { e.file_name().to_string_lossy().starts_with(".gally-stage-") }),
            "failed stage must be cleaned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_directory_rejects_a_replaced_photos_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let photos = tmp.path().join("photos");
        fs::create_dir(&photos).unwrap();
        let opened = File::open(&photos).unwrap();
        assert!(same_directory(&photos, &opened).unwrap());
        fs::rename(&photos, tmp.path().join("photos-old")).unwrap();
        fs::create_dir(&photos).unwrap();
        assert!(!same_directory(&photos, &opened).unwrap());
    }

    #[test]
    fn backup_refuses_to_publish_when_all_photos_are_missing() {
        use crate::db::GalleryDb;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let photos = root.join("photos");
        fs::create_dir_all(&photos).unwrap();
        let mut db = crate::db::open(&root.join("galleries.db")).unwrap();
        let slug = db.get_or_create_gallery("missing-backup-key").unwrap();
        let name = format!("{slug}_{}_missing.jpg", "a".repeat(32));
        fs::write(photos.join(&name), b"photo").unwrap();
        db.add_upload(&slug, &name, 5, 1_000).unwrap();
        crate::db::backfill_exif(&mut db, &photos).unwrap();
        drop(db);

        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();
        assert_eq!(fs::read(dest.join("photos").join(&name)).unwrap(), b"photo");

        // The live index still names the photo, but the file is gone. A second
        // run must fail and leave the first snapshot untouched.
        fs::remove_file(photos.join(&name)).unwrap();
        let err = run(&root, &dest).unwrap_err();
        assert!(
            format!("{err:#}").contains("none of the 1 indexed photos"),
            "{err:#}"
        );
        let snapshot = crate::db::open(&dest.join("galleries.db")).unwrap();
        assert!(snapshot.photo_exists(&slug, &name));
        assert_eq!(fs::read(dest.join("photos").join(&name)).unwrap(), b"photo");
        assert!(
            !fs::read_dir(tmp.path())
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(".gally-stage-")),
            "failed stage must be cleaned"
        );
    }

    #[test]
    fn failed_stage_keeps_previous_snapshot() {
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
        crate::db::backfill_exif(&mut db, &photos).unwrap();
        drop(db);
        let dest = tmp.path().join("dest");
        run(&root, &dest).unwrap();
        let previous_db = fs::read(dest.join("galleries.db")).unwrap();

        // Gallery B appears only after that good backup.
        let mut db = crate::db::open(&db_path).unwrap();
        let slug_b = db.get_or_create_gallery("fail-key-b-0002").unwrap();
        let name_b = format!("{slug_b}_{hex}_b.jpg");
        // A directory in place of the source original fails the staged copy.
        fs::create_dir(photos.join(&name_b)).unwrap();
        fs::write(photos.join(&name_a), b"z").unwrap();
        db.add_upload(&slug_b, &name_b, 1, 1_000).unwrap().unwrap();
        drop(db);
        let err = run(&root, &dest).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a regular backup source file"),
            "{err:#}"
        );
        assert_eq!(fs::read(dest.join("galleries.db")).unwrap(), previous_db);
        assert_eq!(fs::read(dest.join("photos").join(&name_a)).unwrap(), b"a");
        assert!(!dest.join("photos").join(&name_b).exists());
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            2,
            "failed stage removed"
        );
    }
}
