//! APVM Storage Library
//!
//! Manages artifact storage and discovery with deduplication support.
//!
//! # Directory Structure
//!
//! ```text
//! {base_dir}/
//! └── {project}/
//!     └── {major.minor}/
//!         └── {version}/
//!             ├── by-commit/
//!             │   └── {commit_short}/
//!             │       ├── build-manifest.json
//!             │       └── *.zip
//!             └── by-source/
//!                 ├── pr-123 → ../by-commit/abc1234
//!                 └── branch-develop → ../by-commit/abc1234
//! ```
//!
//! # Features
//!
//! - **Deduplication**: Same commit = single copy of artifacts
//! - **Cross-platform links**: Symlinks on Unix, Junctions on Windows
//! - **Build manifests**: JSON metadata for each build
//! - **Query system**: Find builds by project, version, source, or commit
//! - **Variant support**: Optional multi-variant builds

pub mod error;
pub mod link;
pub mod manifest;
pub mod path;
pub mod query;
pub mod store;

pub use error::{Error, Result};
pub use manifest::{ArtifactEntry, BuildManifest, SourceEntry};
pub use path::{BuildSource, PathBuilder};
pub use query::BuildQuery;
pub use store::{
    ArtifactStore, BuildMetadata, DeleteSourceResult, SourceArtifact, StoreResult, StoredBuild,
};
