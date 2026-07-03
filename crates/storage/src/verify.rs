//! Artifact integrity verification.
//!
//! Manifests record size and SHA-256 for every stored file; this module
//! checks stored files against those records at three escalating levels.
//! Read paths use [`VerifyMode::Size`] by default (cheap: two `stat` calls
//! per file); [`VerifyMode::Checksum`] is available for full audits.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::fsx;
use crate::manifest::ArtifactEntry;

/// How thoroughly to verify stored artifacts against their manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Only check that every listed file exists.
    Presence,
    /// Check existence and that file sizes match the manifest (default —
    /// catches truncation and deletion at `stat` cost).
    #[default]
    Size,
    /// Fully re-hash every file and compare SHA-256 (reads all bytes).
    Checksum,
}

/// A single verification failure.
#[derive(Debug, Clone)]
pub struct VerifyIssue {
    /// Manifest filename of the offending artifact.
    pub filename: String,
    /// Full path that was checked.
    pub path: PathBuf,
    /// Human-readable description of the mismatch.
    pub reason: String,
}

/// Verify manifest `entries` against the files in `dir`.
///
/// Returns one [`VerifyIssue`] per failing artifact (empty = all good).
/// IO errors other than "file missing" are reported as issues rather than
/// aborting, so one unreadable file cannot mask the state of the rest.
pub(crate) fn verify_entries(
    dir: &Path,
    entries: &[ArtifactEntry],
    mode: VerifyMode,
) -> Result<Vec<VerifyIssue>> {
    let mut issues = Vec::new();

    for entry in entries {
        let path = dir.join(&entry.filename);

        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                issues.push(VerifyIssue {
                    filename: entry.filename.clone(),
                    path,
                    reason: "file missing".to_string(),
                });
                continue;
            }
            Err(e) => {
                issues.push(VerifyIssue {
                    filename: entry.filename.clone(),
                    path,
                    reason: format!("unreadable: {e}"),
                });
                continue;
            }
        };

        if mode == VerifyMode::Presence {
            continue;
        }

        if metadata.len() != entry.size_bytes {
            issues.push(VerifyIssue {
                filename: entry.filename.clone(),
                path,
                reason: format!(
                    "size mismatch: manifest says {} bytes, file is {} bytes",
                    entry.size_bytes,
                    metadata.len()
                ),
            });
            continue;
        }

        if mode == VerifyMode::Checksum {
            match fsx::sha256_file(&path) {
                Ok(actual) if actual != entry.sha256 => {
                    issues.push(VerifyIssue {
                        filename: entry.filename.clone(),
                        path,
                        reason: format!(
                            "checksum mismatch: manifest says {}, file hashes to {actual}",
                            entry.sha256
                        ),
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    issues.push(VerifyIssue {
                        filename: entry.filename.clone(),
                        path,
                        reason: format!("could not hash: {e}"),
                    });
                }
            }
        }
    }

    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(filename: &str, size: u64, sha256: &str) -> ArtifactEntry {
        ArtifactEntry {
            variant_id: None,
            filename: filename.to_string(),
            size_bytes: size,
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn test_verify_all_ok() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.zip"), b"hello").unwrap();
        let sha = fsx::sha256_file(&dir.path().join("a.zip")).unwrap();

        let entries = vec![entry("a.zip", 5, &sha)];
        for mode in [VerifyMode::Presence, VerifyMode::Size, VerifyMode::Checksum] {
            let issues = verify_entries(dir.path(), &entries, mode).unwrap();
            assert!(issues.is_empty(), "mode {mode:?} reported {issues:?}");
        }
    }

    #[test]
    fn test_verify_detects_missing_file() {
        let dir = TempDir::new().unwrap();
        let entries = vec![entry("gone.zip", 5, "abc")];
        let issues = verify_entries(dir.path(), &entries, VerifyMode::Presence).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].reason.contains("missing"));
    }

    #[test]
    fn test_verify_detects_size_mismatch_but_presence_passes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.zip"), b"hello").unwrap();
        let entries = vec![entry("a.zip", 999, "abc")];

        assert!(
            verify_entries(dir.path(), &entries, VerifyMode::Presence)
                .unwrap()
                .is_empty()
        );
        let issues = verify_entries(dir.path(), &entries, VerifyMode::Size).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].reason.contains("size mismatch"));
    }

    #[test]
    fn test_verify_detects_checksum_mismatch() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.zip"), b"hello").unwrap();
        let entries = vec![entry("a.zip", 5, "not-the-real-hash")];

        // Size matches, so only Checksum catches the corruption.
        assert!(
            verify_entries(dir.path(), &entries, VerifyMode::Size)
                .unwrap()
                .is_empty()
        );
        let issues = verify_entries(dir.path(), &entries, VerifyMode::Checksum).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].reason.contains("checksum mismatch"));
    }
}
