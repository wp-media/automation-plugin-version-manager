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

use crate::error::{Error, IoContext, Result};
use crate::paths;

/// What a directory holds, as far as the store is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreState {
    /// The directory does not exist.
    Missing,
    /// The directory exists with no database and nothing a store would own
    /// (files at its root are fine): safe to initialize.
    Empty,
    /// The directory holds a store database.
    Present,
    /// No database, but store content — re-indexable build directories, or
    /// the leftovers every used store has (its lock file, database
    /// sidecars): a store whose database was lost.
    /// [`ArtifactStore::repair`](crate::ArtifactStore::repair) rebuilds it.
    Orphaned {
        /// How many build directories repair would adopt (may be 0 when
        /// only cached releases remain).
        adoptable_builds: u64,
    },
    /// No database and no sign of a store, yet a subdirectory named like a
    /// store project. Creating a store here could later let `gc` delete
    /// that data, so the store refuses.
    Foreign {
        /// The first such subdirectory.
        entry: String,
    },
}

/// Classify `base_dir` without touching it.
///
/// # Errors
///
/// [`Error::Io`] when `base_dir` cannot be inspected (e.g. a parent is not
/// searchable) or exists but is not a directory.
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
    let db_path = base_dir.join(paths::DB_FILE_NAME);
    if db_path
        .try_exists()
        .io_ctx(|| format!("cannot access store database {}", db_path.display()))?
    {
        return Ok(StoreState::Present);
    }
    // Without a database, only project-named subdirectories matter: they are
    // the only thing a store would ever walk into (and gc delete from).
    let Some(entry) = project_dirs(base_dir).into_iter().next() else {
        return Ok(StoreState::Empty);
    };
    let adoptable_builds = build_dirs(base_dir, &mut 0)
        .iter()
        .filter(|build| !adoptable_files(&build.path).is_empty())
        .count() as u64;
    if adoptable_builds > 0 || has_store_leftovers(base_dir) {
        return Ok(StoreState::Orphaned { adoptable_builds });
    }
    Ok(StoreState::Foreign { entry })
}

/// Whether `base_dir` holds files only a store leaves behind: its lock file
/// (created by the first write, never removed) or `apvm.db-*` / quarantined
/// `apvm.db.corrupt-*` files.
fn has_store_leftovers(base_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(base_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name == paths::LOCK_FILE_NAME
            || name.starts_with(&format!("{}-", paths::DB_FILE_NAME))
            || name.starts_with(&format!("{}.corrupt-", paths::DB_FILE_NAME))
    })
}

/// A build directory whose every path component is a store-produced name.
pub(crate) struct BuildDir {
    /// Project component (a valid project identifier).
    pub project: String,
    /// Version component (a valid version).
    pub version: String,
    /// Commit parsed from the leaf name (lowercase hex).
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
                let Ok(commit) = paths::validate_commit(&name) else {
                    *skipped += 1;
                    continue;
                };
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
/// symlinks) with valid names, excluding in-flight temp files.
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
        if is_file
            && !name.starts_with(paths::TMP_PREFIX)
            && paths::validate_filename(&name).is_ok()
        {
            files.push((name, entry.path()));
        }
    }
    files.sort();
    files
}

/// Subdirectories of `base_dir` named like store projects.
pub(crate) fn project_dirs(base_dir: &Path) -> Vec<String> {
    subdirectories(base_dir)
        .into_iter()
        .filter(|name| paths::validate_project(name).is_ok())
        .collect()
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
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
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
    names
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
    fn release_dir_names_match_the_sanitized_tag_shape() {
        for tag in ["v5.7.6", "1.0", "release/2024 beta", "..", "apvm.db", "a b"] {
            assert!(is_release_dir_name(&paths::sanitize_tag_dir(tag)), "{tag}");
        }
        for bad in ["", ".hidden", "trailing.", "has space", "ümlaut", "a/b"] {
            assert!(!is_release_dir_name(bad), "{bad:?}");
        }
    }
}
