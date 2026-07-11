//! # apvm-storage
//!
//! SQLite-backed cache for APVM build artifacts and GitHub Release assets.
//!
//! All metadata — builds, artifacts (with sizes and SHA-256 hashes), source
//! links, cached releases — lives in a single embedded SQLite database.
//! Artifact **files** live in plain directories next to it; the database is
//! the only index (no manifests, no symlinks):
//!
//! ```text
//! {base_dir}/apvm.db                                  ← all metadata
//! {base_dir}/{project}/commits/{version}/{commit}/    ← build artifact files
//! {base_dir}/{project}/releases/{tag}/                ← release asset files
//! ```
//!
//! Directory paths are recorded relative to `base_dir`, so the store can be
//! moved wholesale and reopened.
//!
//! # Guarantees
//!
//! - **Row ⇒ files.** Files are copied (atomically: temp file + fsync +
//!   rename) *before* metadata commits; deletes remove metadata *before*
//!   files. A crash therefore leaves either invisible files or orphan files
//!   — never a record pointing at nothing. [`ArtifactStore::gc`] reclaims
//!   orphans from both directions.
//! - **Hits are verified.** Every find/lookup checks presence + size of the
//!   files before reporting a hit; damaged entries degrade to a miss and
//!   are healed by the next store. [`ArtifactStore::verify`] goes deeper on
//!   demand (up to full SHA-256 re-hashing).
//! - **Corruption is detected and recoverable.** The database runs in WAL
//!   mode (crash-safe by design) and is integrity-checked on every open; a
//!   damaged database fails with [`Error::DatabaseCorrupted`] and
//!   [`ArtifactStore::repair`] quarantines it, rebuilds a fresh index, and
//!   re-adopts the artifact files found on disk.
//! - **Concurrency.** The store is `Send + Sync`; in-process access is
//!   serialized internally. Across processes, SQLite's WAL handles the
//!   database and an advisory file lock serializes mutations so a clean
//!   cannot race a store. (Keep the store on a local disk — advisory locks
//!   on network filesystems are unreliable.)
//!
//! # Async usage
//!
//! All I/O is intentionally blocking (SQLite is a blocking library). From
//! async code, wrap calls in `tokio::task::spawn_blocking`.
//!
//! # Example
//!
//! ```
//! use apvm_storage::{
//!     ArtifactStore, BuildMetadata, BuildSource, LookupKey, LookupRequest, SourceArtifact,
//! };
//!
//! # fn main() -> apvm_storage::Result<()> {
//! let base = tempfile::tempdir().expect("tempdir");
//! let store = ArtifactStore::open(base.path())?;
//!
//! // A build produced two variant artifacts; put them in the cache.
//! let zip = base.path().join("backwpup-5.6.0.zip");
//! std::fs::write(&zip, b"zip-bytes").expect("write");
//! let metadata = BuildMetadata::new(
//!     "backwpup",
//!     "5.6.0",
//!     BuildSource::PullRequest(123),
//!     "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
//!     "feature/faster-backups".to_string(),
//! );
//! let artifacts = vec![SourceArtifact {
//!     variant_id: Some("free".to_string()),
//!     path: zip.clone(),
//!     target_name: "backwpup-5.6.0.zip".to_string(),
//! }];
//! store.store(&metadata, &artifacts)?;
//!
//! // Cache hit by short commit — the version must match exactly.
//! let request = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
//!     .version("5.6.0");
//! assert!(store.lookup_build(&request)?.is_hit());
//!
//! // Disk accounting and cleanup are first-class.
//! let usage = store.usage()?;
//! assert_eq!(usage.build_count, 1);
//! store.clear_all()?;
//! # Ok(())
//! # }
//! ```

mod db;
mod error;
mod fsx;
mod lock;
mod lookup;
mod maintenance;
mod paths;
mod release;
mod store;
mod types;

pub use error::{Error, Result};
pub use lookup::{LookupHit, LookupKey, LookupRequest, LookupResult, MissReason};
pub use maintenance::{
    CleanOptions, CleanReport, CleanTarget, GcReport, IssueContext, ProjectUsage, RepairReport,
    UsageReport, VerifyIssue, VerifyMode, VerifyProblem,
};
pub use store::{ArtifactStore, StoreOptions};
pub use types::{
    BuildMetadata, BuildSource, ReleaseMetadata, SourceArtifact, SourceLink, StoreReleaseResult,
    StoreResult, StoredArtifact, StoredBuild, StoredRelease,
};
