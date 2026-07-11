//! High-level cache lookup with exact version-match semantics.
//!
//! A cached build satisfies a request only when it is a healthy build of the
//! requested key **at the requested version**. Many plugins are version-aware
//! (the version string is baked into the artifact — BackWPup stamps it into the
//! zip and the plugin headers; WP Rocket / Imagify rewrite it into their
//! source), so a build of the *right commit* at the *wrong version* is a
//! genuinely different artifact and must never be served in its place. When no
//! version is requested, any healthy build of the key is accepted.
//!
//! On a miss, [`MissReason`] says *why* — not cached at all, only other
//! versions cached, required variants missing, or files damaged — so a
//! caller can print an actionable message or decide to rebuild only what is
//! missing.

use chrono::Utc;

use crate::db;
use crate::error::Result;
use crate::paths;
use crate::store::{ArtifactStore, artifacts_healthy, touch_build_quiet};
use crate::types::{BuildSource, StoredBuild};

/// What to look up: a commit or a source reference.
#[derive(Debug, Clone, Copy)]
pub enum LookupKey<'a> {
    /// A commit SHA, short (≥ 7 hex chars) or full.
    Commit(&'a str),
    /// A build source (PR, branch, tag, ...); candidates are the builds the
    /// source has been linked to, most recently linked first.
    Source(&'a BuildSource),
}

/// A cache lookup request. Construct with [`LookupRequest::new`] and refine
/// with the builder methods; the struct is `#[non_exhaustive]` so new knobs
/// can be added without breaking callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LookupRequest<'a> {
    /// Project to search in.
    pub project: &'a str,
    /// What to search by.
    pub key: LookupKey<'a>,
    /// Requested version, if the caller has one. When set, only a build at
    /// exactly this version is a hit; when `None`, any healthy build of the
    /// key is accepted.
    pub version: Option<&'a str>,
    /// Variants that must be present and healthy for a hit (`None` entries
    /// mean the variant-less artifact). Empty = any healthy artifact set.
    pub required_variants: &'a [Option<String>],
}

impl<'a> LookupRequest<'a> {
    /// A request with no version pin and no variant requirements.
    pub fn new(project: &'a str, key: LookupKey<'a>) -> Self {
        Self {
            project,
            key,
            version: None,
            required_variants: &[],
        }
    }

    /// Pin the requested version. Only a build at exactly this version hits.
    #[must_use]
    pub fn version(mut self, version: &'a str) -> Self {
        self.version = Some(version);
        self
    }

    /// Require these variants to be present and healthy.
    #[must_use]
    pub fn require_variants(mut self, variants: &'a [Option<String>]) -> Self {
        self.required_variants = variants;
        self
    }
}

/// A successful lookup.
#[derive(Debug, Clone)]
pub struct LookupHit {
    /// The healthy cached build. Its version equals the requested version
    /// (when one was requested).
    pub build: StoredBuild,
}

/// Why a lookup missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissReason {
    /// Nothing is cached for the key.
    NotCached,
    /// Builds of the key exist, but none at the requested version.
    /// `available` lists the cached versions, newest first.
    VersionMismatch {
        /// Versions of the key that *are* cached.
        available: Vec<String>,
    },
    /// A build exists with healthy files, but not all required variants are
    /// stored. `missing` is what would have to be built.
    MissingVariants {
        /// The required variants that are absent or unhealthy.
        missing: Vec<Option<String>>,
    },
    /// Cached entries exist but their files are missing or damaged on disk;
    /// rebuilding (and re-storing) heals them.
    Incomplete,
}

/// Result of [`ArtifactStore::lookup_build`].
#[derive(Debug, Clone)]
pub enum LookupResult {
    /// A healthy build satisfying the request.
    Hit(LookupHit),
    /// No satisfying build; the reason says why.
    Miss(MissReason),
}

impl LookupResult {
    /// `true` for [`LookupResult::Hit`].
    pub fn is_hit(&self) -> bool {
        matches!(self, Self::Hit(_))
    }

    /// Unwrap into the hit, if any.
    pub fn hit(self) -> Option<LookupHit> {
        match self {
            Self::Hit(hit) => Some(hit),
            Self::Miss(_) => None,
        }
    }
}

/// A candidate build with its evaluation against the request.
struct Candidate {
    build: StoredBuild,
    files_ok: bool,
    missing_variants: Vec<Option<String>>,
}

impl ArtifactStore {
    /// Decide whether a cached build satisfies `request`, and if not, why.
    ///
    /// Hits update the build's last-used timestamp (feeding age-based
    /// [`clean`](ArtifactStore::clean)); candidates whose files are damaged
    /// are never returned.
    ///
    /// # Errors
    ///
    /// [`crate::Error::InvalidInput`] for malformed project/version/commit
    /// inputs, [`crate::Error::Database`] for SQLite failures. An empty or
    /// damaged cache is a [`LookupResult::Miss`], not an error.
    pub fn lookup_build(&self, request: &LookupRequest<'_>) -> Result<LookupResult> {
        paths::validate_project(request.project)?;
        if let Some(version) = request.version {
            paths::validate_version(version)?;
        }

        let conn = self.conn();
        let rows = match request.key {
            LookupKey::Commit(commit) => {
                let commit = paths::validate_commit(commit)?;
                db::builds::by_commit(&conn, request.project, &commit, None)?
            }
            LookupKey::Source(source) => {
                db::builds::by_source(&conn, request.project, source.kind(), &source.reference())?
            }
        };
        if rows.is_empty() {
            return Ok(LookupResult::Miss(MissReason::NotCached));
        }

        // Evaluate every candidate; unreadable rows degrade to a miss, not
        // an error.
        let mut candidates: Vec<Candidate> = Vec::with_capacity(rows.len());
        for row in &rows {
            match self.build_from_row(&conn, row) {
                Ok(build) => candidates.push(evaluate(build, request.required_variants)),
                Err(err) => tracing::warn!(error = %err, "skipping unreadable build row"),
            }
        }
        if candidates.is_empty() {
            return Ok(LookupResult::Miss(MissReason::Incomplete));
        }

        // A hit requires the version to match exactly. Consider only
        // version-matching candidates (all of them when no version was
        // requested); they are already ordered newest-first.
        let matching: Vec<usize> = (0..candidates.len())
            .filter(|&i| {
                request
                    .version
                    .is_none_or(|v| candidates[i].build.version == v)
            })
            .collect();

        if let Some(&picked) = matching
            .iter()
            .find(|&&i| candidates[i].files_ok && candidates[i].missing_variants.is_empty())
        {
            let candidate = candidates.swap_remove(picked);
            touch_build_quiet(&conn, candidate.build.id, db::to_ms(Utc::now()));
            return Ok(LookupResult::Hit(LookupHit {
                build: candidate.build,
            }));
        }

        Ok(LookupResult::Miss(miss_reason(&candidates, &matching)))
    }
}

/// Health-check a build against the required variants.
fn evaluate(build: StoredBuild, required_variants: &[Option<String>]) -> Candidate {
    let files_ok = artifacts_healthy(&build);
    let healthy_variants: Vec<&Option<String>> = build
        .artifacts
        .iter()
        .filter(|artifact| crate::fsx::file_size(&artifact.path) == Some(artifact.size_bytes))
        .map(|artifact| &artifact.variant_id)
        .collect();
    let missing_variants = required_variants
        .iter()
        .filter(|required| !healthy_variants.contains(required))
        .cloned()
        .collect();
    Candidate {
        build,
        files_ok,
        missing_variants,
    }
}

/// Explain the best-available candidate's shortfall, most actionable first.
///
/// `matching` are the indices of candidates at the requested version (all
/// candidates when no version was requested).
fn miss_reason(candidates: &[Candidate], matching: &[usize]) -> MissReason {
    if matching.is_empty() {
        // The key is cached, but never at the requested version.
        return MissReason::VersionMismatch {
            available: available_versions(candidates),
        };
    }
    // Files intact but variants missing → tell the caller what to build.
    let fewest_missing = matching
        .iter()
        .map(|&i| &candidates[i])
        .filter(|candidate| candidate.files_ok && !candidate.missing_variants.is_empty())
        .min_by_key(|candidate| candidate.missing_variants.len());
    match fewest_missing {
        Some(candidate) => MissReason::MissingVariants {
            missing: candidate.missing_variants.clone(),
        },
        None => MissReason::Incomplete,
    }
}

/// Distinct cached versions across the candidates, newest first.
fn available_versions(candidates: &[Candidate]) -> Vec<String> {
    let mut versions: Vec<String> = Vec::new();
    for candidate in candidates {
        if !versions.contains(&candidate.build.version) {
            versions.push(candidate.build.version.clone());
        }
    }
    versions.sort_by(|a, b| paths::cmp_versions(b, a));
    versions
}
