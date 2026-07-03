//! APVM Storage Library
//!
//! A production-grade, filesystem-backed cache for built plugin artifacts
//! and downloaded release assets. Point it at a builds cache directory and
//! it deduplicates per commit, tracks every build in JSON manifests, and
//! answers "is this already built?" before a pipeline spends minutes
//! rebuilding.
//!
//! # Directory Structure
//!
//! ```text
//! {base_dir}/
//! ├── .apvm-store.json                  - store marker + layout schema version
//! └── {project}/
//!     ├── releases/                     - cached GitHub Release assets (keyed by tag)
//!     │   └── {tag}/
//!     │       ├── release-manifest.json
//!     │       └── *.zip
//!     └── {major.minor}/
//!         └── {version}/
//!             ├── by-commit/
//!             │   └── {commit_short}/   - the single physical copy per commit
//!             │       ├── build-manifest.json
//!             │       └── *.zip
//!             └── by-source/            - navigation links, one dir per source
//!                 ├── pr-123/{commit}      → ../../by-commit/{commit}
//!                 └── branch-develop/{commit} → ../../by-commit/{commit}
//! ```
//!
//! # Guarantees
//!
//! - **Validated inputs**: every caller-provided value that becomes a path
//!   component is validated (`validate.rs`); nothing can escape the
//!   base directory or shadow store metadata.
//! - **Crash consistency**: manifests and artifacts are written to temp
//!   files, fsynced, and atomically renamed — readers never see torn files
//!   (`fsx.rs`).
//! - **Concurrency**: mutations hold an exclusive cross-process lock; reads
//!   are lock-free and integrity-checked.
//! - **Self-healing**: damaged entries read as cache misses and are
//!   repaired by the next store of the same build.
//! - **Collision safety**: short-commit directories are cross-checked
//!   against the full hash in the manifest; a real 7-hex-char collision is
//!   a loud [`Error::CommitCollision`], never silent deduplication.
//!
//! # Typical cache flow
//!
//! ```ignore
//! use apvm_storage::{ArtifactStore, LookupRequest, LookupResult, VersionMatch};
//!
//! let store = ArtifactStore::new(config.builds_dir);
//!
//! // Releases (no commit available — keyed by tag):
//! if let Some(release) = store.find_release("backwpup", "v5.6.0")? {
//!     return Ok(release.files);
//! }
//!
//! // Builds (keyed by resolved commit):
//! let request = LookupRequest::new("backwpup", &commit_sha)
//!     .version("5.6.0")
//!     // Version-aware plugin + strict flag off → accept any version:
//!     .version_match(VersionMatch::Lenient);
//! match store.lookup_build(&request)? {
//!     LookupResult::Hit(hit) => Ok(hit.build.files),
//!     LookupResult::Miss(reason) => {
//!         tracing::info!("cache miss: {reason}");
//!         let output = build_it()?;
//!         store.store(&output.artifacts, &output.metadata)?;
//!         // ...
//!     }
//! }
//! ```

pub mod error;
mod fsx;
pub mod link;
pub mod lookup;
pub mod manifest;
pub mod path;
pub mod query;
pub mod release;
pub mod store;
mod validate;
pub mod verify;

pub use error::{Error, Result};
pub use lookup::{LookupHit, LookupRequest, LookupResult, MissReason, VersionMatch};
pub use manifest::{ArtifactEntry, BuildManifest, SourceEntry};
pub use path::{BuildSource, PathBuilder, compare_versions, major_minor};
pub use query::BuildQuery;
pub use release::{ReleaseManifest, ReleaseMetadata, StoreReleaseResult, StoredRelease};
pub use store::{
    ArtifactStore, BuildMetadata, DeleteSourceResult, SourceArtifact, StoreResult, StoredBuild,
};
pub use verify::{VerifyIssue, VerifyMode};
