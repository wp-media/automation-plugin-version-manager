//! Reading the store's on-disk layout: what a directory holds
//! ([`StoreState`]), and the walks that `inspect`, `repair` and `gc` share.
//!
//! Everything here is read-only. The walks never follow symlinks, and an
//! entry counts as store content only when every path component is a name
//! the store itself produces — so a mis-pointed store directory can never
//! make the store adopt, or `gc` delete, someone else's files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::paths;

/// What a directory holds, as far as the store is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreState {
    /// The directory does not exist.
    Missing,
    /// The directory exists with no database and nothing a store would own
    /// (files at its root are fine): nothing is cached. A store is only
    /// created here if the directory is vacant — see
    /// [`ArtifactStore::open`](crate::ArtifactStore::open).
    Empty,
    /// The directory holds a store database (one with content, or a blank
    /// one in an otherwise vacant directory).
    Present,
    /// A store — it has the store's lock file, created with the database and
    /// never removed — holding project content but no usable database: the
    /// database is missing or blank (0 bytes), or a repair was interrupted
    /// (its marker remains). [`ArtifactStore::repair`](crate::ArtifactStore::repair)
    /// rebuilds it.
    Orphaned {
        /// How many build directories repair would adopt (may be 0 when
        /// only cached releases remain).
        adoptable_builds: u64,
    },
    /// No lock file, yet a subdirectory named like a store project — even
    /// one shaped like a store build, and whatever `apvm.db` holds. The lock
    /// file is created before a store's first build or release and never
    /// removed, so such content is someone else's: treating the directory
    /// as a store could let repair adopt it and `gc` delete it, so the store
    /// refuses.
    Foreign {
        /// The first such subdirectory.
        entry: String,
    },
}

/// Classify `base_dir` without touching it.
///
/// # Errors
///
/// [`Error::Io`] when `base_dir` cannot be inspected or listed (e.g. a
/// parent is not searchable, or it is not readable) or exists but is not a
/// directory.
pub(crate) fn inspect(base_dir: &Path) -> Result<StoreState> {
    let meta = match fs::metadata(base_dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(StoreState::Missing),
        Err(err) => {
            return Err(Error::Io {
                context: format!("cannot access store directory {}", base_dir.display()),
                source: err,
            });
        }
    };
    if !meta.is_dir() {
        return Err(Error::Io {
            context: format!("store path {} is not a directory", base_dir.display()),
            source: io::Error::from(io::ErrorKind::NotADirectory),
        });
    }
    let owned = has_lock_file(base_dir);
    // An interrupted repair: whatever the database holds is incomplete. Any
    // entry counts as the marker, a symlink too (repair refuses to follow
    // it), so the half-filled index is never trusted.
    if owned && fs::symlink_metadata(base_dir.join(paths::REPAIR_MARKER_NAME)).is_ok() {
        return Ok(StoreState::Orphaned {
            adoptable_builds: count_adoptable_builds(base_dir),
        });
    }
    let database = database(base_dir)?;
    // Project-named subdirectories are the only thing a store would ever
    // walk into (and gc delete from).
    let Some(entry) = listed_project_dirs(base_dir)?.into_iter().next() else {
        return Ok(match database {
            Database::Present => StoreState::Present,
            // Nothing to orphan: opening initializes our own blank file —
            // someone else's only where creating a store is allowed.
            Database::Blank if owned || first_occupant(base_dir)?.is_none() => StoreState::Present,
            Database::Blank | Database::Absent => StoreState::Empty,
        });
    };
    // Only a directory that has been a store — its lock file is created
    // before the store's first build or release and never removed — owns
    // project-shaped content. Without it, store-shaped names prove nothing,
    // whatever `apvm.db` holds (a stray, garbage or blank file): the data is
    // someone else's, and treating it as a store would let repair adopt it
    // and gc delete it.
    if !owned {
        return Ok(StoreState::Foreign { entry });
    }
    Ok(match database {
        Database::Present => StoreState::Present,
        Database::Absent | Database::Blank => StoreState::Orphaned {
            adoptable_builds: count_adoptable_builds(base_dir),
        },
    })
}

/// The first entry of `base_dir` that makes it occupied: anything but the
/// store's own files (`.apvm.lock`, `apvm.db` and its sidecars, kept copies
/// and repair marker) and operating-system metadata files. `None` for a
/// vacant or missing directory. A store is only ever created in a vacant
/// one: in a shared directory, `gc` could later delete store-shaped data
/// someone else puts there.
///
/// # Errors
///
/// [`Error::Io`] when `base_dir` cannot be listed.
pub(crate) fn first_occupant(base_dir: &Path) -> Result<Option<String>> {
    let entries = match fs::read_dir(base_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(list_error(base_dir, err)),
    };
    let mut occupants = Vec::new();
    for entry in entries {
        let name = entry.map_err(|err| list_error(base_dir, err))?.file_name();
        let name = name.to_string_lossy();
        if !is_incidental(&name) {
            occupants.push(name.into_owned());
        }
    }
    occupants.sort();
    Ok(occupants.into_iter().next())
}

/// Whether `name`, at a store's root, leaves the directory vacant: one of
/// the store's own files, or an operating-system metadata file.
fn is_incidental(name: &str) -> bool {
    const OWN: [&str; 6] = [
        paths::LOCK_FILE_NAME,
        paths::DB_FILE_NAME,
        "apvm.db-wal",
        "apvm.db-shm",
        "apvm.db-journal",
        paths::REPAIR_MARKER_NAME,
    ];
    const OS_METADATA: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];
    OWN.contains(&name)
        || name.starts_with("apvm.db.corrupt-")
        || OS_METADATA
            .iter()
            .any(|metadata| name.eq_ignore_ascii_case(metadata))
}

/// [`Error::Io`] for a directory that cannot be listed.
fn list_error(dir: &Path, source: io::Error) -> Error {
    Error::Io {
        context: format!("cannot list store directory {}", dir.display()),
        source,
    }
}

/// What the store database file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Database {
    /// No file.
    Absent,
    /// A file with no content: 0 bytes, with no WAL data either (truncated,
    /// or created by an open that never finished).
    Blank,
    /// Anything else — including a database SQLite will reject.
    Present,
}

/// Classify `base_dir`'s database file.
///
/// # Errors
///
/// [`Error::Io`] when the file cannot be inspected, or is a symbolic link:
/// the store never creates one, and following it would let a read-only
/// action migrate (write to) a database elsewhere.
fn database(base_dir: &Path) -> Result<Database> {
    let db_path = base_dir.join(paths::DB_FILE_NAME);
    let meta = match fs::symlink_metadata(&db_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Database::Absent),
        Err(err) => {
            return Err(Error::Io {
                context: format!("cannot access store database {}", db_path.display()),
                source: err,
            });
        }
    };
    if meta.file_type().is_symlink() {
        return Err(Error::Io {
            context: format!(
                "store database {} is a symbolic link; refusing to follow it",
                db_path.display()
            ),
            source: io::Error::new(io::ErrorKind::InvalidInput, "symbolic link"),
        });
    }
    if !meta.is_file() || meta.len() > 0 {
        return Ok(Database::Present);
    }
    // A new database in WAL mode may keep all its content in the WAL.
    let wal = base_dir.join(format!("{}-wal", paths::DB_FILE_NAME));
    Ok(match fs::metadata(wal) {
        Ok(meta) if meta.len() > 0 => Database::Present,
        _ => Database::Blank,
    })
}

/// Whether `base_dir` holds the store's lock file — created with the
/// database, never removed: proof the directory has been a store.
fn has_lock_file(base_dir: &Path) -> bool {
    fs::symlink_metadata(base_dir.join(paths::LOCK_FILE_NAME)).is_ok_and(|meta| meta.is_file())
}

/// How many build directories repair would adopt.
pub(crate) fn count_adoptable_builds(base_dir: &Path) -> u64 {
    build_dirs(base_dir, &mut 0)
        .iter()
        .filter(|build| !adoptable_files(&build.path).is_empty())
        .count() as u64
}

/// A build directory whose every path component is a store-produced name.
pub(crate) struct BuildDir {
    /// Project component (a valid project identifier).
    pub project: String,
    /// Version component (a valid version).
    pub version: String,
    /// Commit from the leaf name: its hex part (lowercase).
    pub commit: String,
    /// The leaf directory name, verbatim.
    pub name: String,
    /// Absolute path of the directory.
    pub path: PathBuf,
}

/// Every `{project}/commits/{version}/{commit}` directory under `base_dir`,
/// never following symlinks. Directories whose names the store could not
/// have produced are counted in `skipped`, not returned.
pub(crate) fn build_dirs(base_dir: &Path, skipped: &mut u64) -> Vec<BuildDir> {
    let mut found = Vec::new();
    for project in subdirectories(base_dir) {
        if paths::validate_project(&project).is_err() {
            *skipped += 1;
            continue;
        }
        let Some(commits) = managed_dir(&base_dir.join(&project).join(paths::COMMITS_DIR)) else {
            continue;
        };
        for version in subdirectories(&commits) {
            if paths::validate_version(&version).is_err() {
                *skipped += 1;
                continue;
            }
            for name in subdirectories(&commits.join(&version)) {
                // Exactly the names gc and clean treat as builds (lowercase
                // hex, or the `{7 hex}-{8 hex}` disambiguated form), so an
                // adopted build is never one they would refuse — nor a build
                // they keep left unadopted for gc to delete.
                if !is_build_dir_name(&name) {
                    *skipped += 1;
                    continue;
                }
                let commit = name.split('-').next().unwrap_or_default().to_string();
                let path = commits.join(&version).join(&name);
                found.push(BuildDir {
                    project: project.clone(),
                    version: version.clone(),
                    commit,
                    name,
                    path,
                });
            }
        }
    }
    found
}

/// Files in a build directory that repair would index: regular files (not
/// symlinks) with valid names — never hidden files (in-flight temp files,
/// `.DS_Store`, `._*` resource forks) or `Thumbs.db` / `desktop.ini`, which
/// the operating system drops beside builds: adopted, one would be served
/// as the build's artifact.
pub(crate) fn adoptable_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let is_file = entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false);
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let os_metadata = ["Thumbs.db", "desktop.ini"]
            .iter()
            .any(|metadata| name.eq_ignore_ascii_case(metadata));
        if is_file
            && !name.starts_with('.')
            && !os_metadata
            && paths::validate_filename(&name).is_ok()
        {
            files.push((name, entry.path()));
        }
    }
    files.sort();
    files
}

/// Subdirectories of `base_dir` named like store projects (none when it
/// cannot be listed: for walks that skip what they cannot read).
pub(crate) fn project_dirs(base_dir: &Path) -> Vec<String> {
    listed_project_dirs(base_dir).unwrap_or_default()
}

/// Subdirectories of `base_dir` named like store projects; a directory that
/// cannot be listed is an error, never "no projects" — classifying it as
/// empty would let a store be created over (and gc delete) what it hides.
///
/// # Errors
///
/// [`Error::Io`] when `base_dir` cannot be listed.
fn listed_project_dirs(base_dir: &Path) -> Result<Vec<String>> {
    Ok(try_subdirectories(base_dir)
        .map_err(|err| list_error(base_dir, err))?
        .into_iter()
        .filter(|name| paths::validate_project(name).is_ok())
        .collect())
}

/// `path` if it is a real directory. `None` for a symlink (even one to a
/// directory), a file, or nothing — walking into a symlinked managed
/// directory could reach, and let gc delete, data outside the store.
pub(crate) fn managed_dir(path: &Path) -> Option<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Some(path.to_path_buf()),
        _ => None,
    }
}

/// Whether `rel` is a directory path the store produces, spelled exactly as
/// it does: `{project}/commits/{version}/{build}` or
/// `{project}/releases/{release}`. Pure: a record whose path fails this was
/// tampered with.
pub(crate) fn is_store_rel(rel: &str) -> bool {
    match rel.split('/').collect::<Vec<_>>().as_slice() {
        [project, kind, version, build] if *kind == paths::COMMITS_DIR => {
            paths::validate_project(project).is_ok()
                && paths::validate_version(version).is_ok()
                && is_build_dir_name(build)
        }
        [project, kind, release] if *kind == paths::RELEASES_DIR => {
            paths::validate_project(project).is_ok() && is_release_dir_name(release)
        }
        _ => false,
    }
}

/// `Ok(Some(base_dir/rel))` when `rel` is a store-produced directory path
/// ([`is_store_rel`]) and none of its existing components is a symlink or a
/// non-directory; `Ok(None)` otherwise.
///
/// Destructive operations, and writes into a recorded directory, resolve
/// record paths through this, so neither a tampered record nor a symlinked
/// directory can lead them outside the store (`base_dir` itself may be a
/// symlink: the user chose it). Components that do not exist yet are fine:
/// there is nothing to follow there.
///
/// # Errors
///
/// The I/O error when a component cannot be inspected (e.g. permission
/// denied), so callers do not mistake it for a layout problem.
pub(crate) fn owned_dir(base_dir: &Path, rel: &str) -> io::Result<Option<PathBuf>> {
    if !is_store_rel(rel) {
        return Ok(None);
    }
    let mut walked = base_dir.to_path_buf();
    let mut exists = true;
    for part in rel.split('/') {
        walked.push(part);
        if !exists {
            continue;
        }
        match fs::symlink_metadata(&walked) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(err) if err.kind() == io::ErrorKind::NotFound => exists = false,
            Err(err) => return Err(err),
        }
    }
    Ok(Some(walked))
}

/// Whether `name` is a build directory name the store produces: 7–64
/// lowercase hex chars, or 7 of them plus `-` and an 8-char hex
/// disambiguator (see `pick_build_dir`).
pub(crate) fn is_build_dir_name(name: &str) -> bool {
    let is_hex = |part: &str| part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match name.split_once('-') {
        None => (7..=64).contains(&name.len()) && is_hex(name),
        Some((prefix, hash)) => {
            prefix.len() == 7 && hash.len() == 8 && is_hex(prefix) && is_hex(hash)
        }
    }
}

/// Whether `name` could be a release directory the store produces: the
/// shape of [`paths::sanitize_tag_dir`]'s output.
pub(crate) fn is_release_dir_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 109
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Names of real subdirectories of `path`, sorted. Symlinks are skipped (a
/// directory entry's type describes the link itself), as are names that are
/// not valid UTF-8. A missing or unreadable `path` yields nothing.
pub(crate) fn subdirectories(path: &Path) -> Vec<String> {
    try_subdirectories(path).unwrap_or_default()
}

/// [`subdirectories`], failing when `path` exists but cannot be listed. A
/// missing `path` has none.
fn try_subdirectories(path: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        match entry.file_name().into_string() {
            Ok(name) => names.push(name),
            Err(raw) => tracing::warn!(name = ?raw, "skipping non-UTF-8 directory name"),
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dir_names_match_what_the_store_produces() {
        for ok in [
            "a1b2c3d",
            "a1b2c3d4e5f6",
            "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
            "a1b2c3d-0f1e2d3c",
        ] {
            assert!(is_build_dir_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a1b2c3",
            "A1B2C3D",
            "q1",
            "2024",
            "a1b2c3d-",
            "a1b2c3d-0f1e2d3",
            "a1b2c3d4-0f1e2d3c",
            "a1b2c3d-0f1e2d3c-00",
        ] {
            assert!(!is_build_dir_name(bad), "{bad:?}");
        }
        assert!(!is_build_dir_name(&"a".repeat(65)));
    }

    #[test]
    fn owned_dirs_are_store_shaped_and_free_of_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("store");
        let build = "wp-rocket/commits/3.17.4/a1b2c3d";
        let release = "backwpup/releases/v5.3.0";
        std::fs::create_dir_all(base.join(build)).unwrap();

        assert_eq!(owned_dir(&base, build).unwrap(), Some(base.join(build)));
        // Not created yet: nothing to follow, still ours.
        assert_eq!(owned_dir(&base, release).unwrap(), Some(base.join(release)));
        for bad in [
            "",
            "wp-rocket",
            "My-Notes",
            "wp-rocket/commits/3.17.4",
            "wp-rocket/commits/3.17.4/a1b2c3d/extra",
            "wp-rocket/commits/../a1b2c3d",
            "wp-rocket/other/3.17.4/a1b2c3d",
            "wp-rocket/commits/3.17.4/notahash",
            "Bad Project/releases/v1",
            "backwpup/releases/.hidden",
            "/abs/commits/1.0/a1b2c3d",
            "wp-rocket\\commits\\3.17.4\\a1b2c3d",
            "wp-rocket/commits/3.17.4/a1b2c3d/",
            "./wp-rocket/commits/3.17.4/a1b2c3d",
        ] {
            assert_eq!(owned_dir(&base, bad).unwrap(), None, "{bad:?}");
            assert!(!is_store_rel(bad), "{bad:?}");
        }
        assert!(is_store_rel(build) && is_store_rel(release));
    }

    #[cfg(unix)]
    #[test]
    fn owned_dirs_never_go_through_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("store");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(outside.join("3.17.4/a1b2c3d")).unwrap();
        std::fs::create_dir_all(base.join("wp-rocket")).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("wp-rocket/commits")).unwrap();
        assert_eq!(
            owned_dir(&base, "wp-rocket/commits/3.17.4/a1b2c3d").unwrap(),
            None
        );

        // A file where a directory belongs is refused too.
        std::fs::create_dir_all(base.join("backwpup")).unwrap();
        std::fs::write(base.join("backwpup/releases"), b"x").unwrap();
        assert_eq!(owned_dir(&base, "backwpup/releases/v1").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_uninspectable_component_is_an_error_not_a_layout_problem() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("store");
        let commits = base.join("wp-rocket/commits");
        std::fs::create_dir_all(commits.join("3.17.4/a1b2c3d")).unwrap();
        std::fs::set_permissions(&commits, std::fs::Permissions::from_mode(0o000)).unwrap();
        let outcome = owned_dir(&base, "wp-rocket/commits/3.17.4/a1b2c3d");
        std::fs::set_permissions(&commits, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root reads through any permission bits.
        if is_root() {
            return;
        }
        assert_eq!(
            outcome.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    /// Whether the tests run as root, which permission bits do not stop.
    #[cfg(unix)]
    fn is_root() -> bool {
        let probe = tempfile::tempdir().unwrap();
        let locked = probe.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&locked).is_ok();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        readable
    }

    #[test]
    fn release_dir_names_match_the_sanitized_tag_shape() {
        for tag in ["v5.7.6", "1.0", "release/2024 beta", "..", "apvm.db", "a b"] {
            assert!(is_release_dir_name(&paths::sanitize_tag_dir(tag)), "{tag}");
        }
        for bad in ["", ".hidden", "trailing.", "has space", "ümlaut", "a/b"] {
            assert!(!is_release_dir_name(bad), "{bad:?}");
        }
    }
}
