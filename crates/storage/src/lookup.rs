//! High-level cache lookup with version-match semantics.
//!
//! This is the API a build pipeline calls *before* building: "is this
//! commit already cached, and does the cached copy satisfy my version and
//! variant requirements?"
//!
//! # Version matching
//!
//! Some plugins are *version-aware*: the version string is baked into the
//! built artifact (e.g. BackWPup, where it is mostly cosmetic — what
//! WordPress displays — except for special cases like data migrations).
//! For those, an artifact built from the same commit under a different
//! requested version may or may not be acceptable:
//!
//! - [`VersionMatch::Strict`]: only a build stored under the exact requested
//!   version is a hit. A same-commit build under another version is reported
//!   as [`MissReason::VersionMismatch`] (with the available versions), so the
//!   caller knows a rebuild is needed *only* because of the version stamp.
//! - [`VersionMatch::Lenient`]: any healthy build of the commit is a hit;
//!   [`LookupHit::version_matched`] tells the caller whether the version
//!   also matched, so it can inform the user.
//!
//! Plugins that are not version-aware should simply use `Lenient`.
//!
//! # Integrity
//!
//! Only healthy builds (manifest-recorded file sizes verified) count as
//! hits. A cached-but-damaged build surfaces as [`MissReason::Incomplete`];
//! re-storing after rebuild self-heals it.

use crate::error::Result;
use crate::store::{ArtifactStore, StoredBuild};
use crate::validate;
use crate::verify::VerifyMode;

/// How strictly the requested version must match a cached build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VersionMatch {
    /// Only builds stored under the exact requested version are hits.
    #[default]
    Strict,
    /// Any healthy build of the commit is a hit, regardless of version.
    Lenient,
}

/// A cache lookup request.
///
/// Build with [`LookupRequest::new`] and refine with the builder methods:
///
/// ```ignore
/// let request = LookupRequest::new("backwpup", &commit_sha)
///     .version("5.6.0")
///     .version_match(VersionMatch::Lenient)
///     .require_variants(&[Some("free".into()), Some("pro".into())]);
///
/// match store.lookup_build(&request)? {
///     LookupResult::Hit(hit) => use_cached(hit.build),
///     LookupResult::Miss(reason) => build_fresh(reason),
/// }
/// ```
#[derive(Debug, Clone)]
pub struct LookupRequest<'a> {
    /// Project name.
    pub project: &'a str,
    /// Commit hash (short or full, any case).
    pub commit: &'a str,
    /// Requested version, when known. `None` means "any version".
    pub version: Option<&'a str>,
    /// How strictly `version` must match (ignored when `version` is `None`).
    pub version_match: VersionMatch,
    /// Variants that must all be present (and healthy) for a hit.
    /// Empty means any cached artifact set is acceptable.
    pub required_variants: &'a [Option<String>],
}

impl<'a> LookupRequest<'a> {
    /// Create a request for a project + commit with default semantics
    /// (any version, no required variants).
    pub fn new(project: &'a str, commit: &'a str) -> Self {
        Self {
            project,
            commit,
            version: None,
            version_match: VersionMatch::default(),
            required_variants: &[],
        }
    }

    /// Require (or prefer, depending on [`VersionMatch`]) a specific version.
    pub fn version(mut self, version: &'a str) -> Self {
        self.version = Some(version);
        self
    }

    /// Set the version-match strictness.
    pub fn version_match(mut self, mode: VersionMatch) -> Self {
        self.version_match = mode;
        self
    }

    /// Require these variants to be present for a hit.
    pub fn require_variants(mut self, variants: &'a [Option<String>]) -> Self {
        self.required_variants = variants;
        self
    }
}

/// A successful lookup.
#[derive(Debug)]
pub struct LookupHit {
    /// The cached build satisfying the request.
    pub build: StoredBuild,
    /// Whether the build's version equals the requested version
    /// (always `true` when no version was requested).
    pub version_matched: bool,
}

/// Why a lookup missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissReason {
    /// Nothing cached for this commit at all.
    NotCached,
    /// Strict version matching: the commit is cached, but only under other
    /// versions. Contains the versions it *is* cached under.
    VersionMismatch {
        /// Versions under which this commit is cached.
        available: Vec<String>,
    },
    /// A build exists but lacks some required variants.
    MissingVariants {
        /// The required variants that are absent or unhealthy.
        missing: Vec<Option<String>>,
    },
    /// A build exists but its files are missing/damaged; rebuilding and
    /// re-storing will self-heal it.
    Incomplete,
}

impl std::fmt::Display for MissReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCached => write!(f, "commit not cached"),
            Self::VersionMismatch { available } => write!(
                f,
                "commit cached under other version(s): {}",
                available.join(", ")
            ),
            Self::MissingVariants { missing } => {
                let names: Vec<String> = missing
                    .iter()
                    .map(|v| v.clone().unwrap_or_else(|| "<default>".to_string()))
                    .collect();
                write!(f, "cached build lacks variant(s): {}", names.join(", "))
            }
            Self::Incomplete => write!(f, "cached build is damaged or incomplete"),
        }
    }
}

/// Result of a cache lookup.
#[derive(Debug)]
pub enum LookupResult {
    /// A cached build satisfies the request.
    Hit(LookupHit),
    /// No cached build satisfies the request; the reason says why.
    Miss(MissReason),
}

impl LookupResult {
    /// Convenience: the hit, if any.
    pub fn hit(self) -> Option<LookupHit> {
        match self {
            Self::Hit(hit) => Some(hit),
            Self::Miss(_) => None,
        }
    }

    /// Convenience: whether this is a hit.
    pub fn is_hit(&self) -> bool {
        matches!(self, Self::Hit(_))
    }
}

/// Outcome of evaluating one candidate build against a request.
enum CandidateOutcome {
    /// Candidate satisfies the request.
    Usable,
    /// Files missing or damaged.
    Incomplete,
    /// Required variants absent/unhealthy.
    MissingVariants(Vec<Option<String>>),
}

impl ArtifactStore {
    /// Look up a cached build for a commit with version/variant semantics.
    ///
    /// See the [module docs](crate::lookup) for the decision model. In
    /// short: the exact requested version is preferred; other versions of
    /// the same commit are hits only under [`VersionMatch::Lenient`]
    /// (newest first), and otherwise explain the miss via
    /// [`MissReason::VersionMismatch`].
    pub fn lookup_build(&self, request: &LookupRequest<'_>) -> Result<LookupResult> {
        validate::validate_project(request.project)?;
        validate::validate_commit(request.commit)?;
        if let Some(version) = request.version {
            validate::validate_version(version)?;
        }

        // Candidate builds paired with whether they match the requested
        // version, ordered by preference: exact version first, then other
        // versions newest-first. Candidates are loaded *raw* (no integrity
        // filter) so a damaged cached build is classified as `Incomplete`
        // rather than misreported as `NotCached`.
        let mut candidates: Vec<(StoredBuild, bool)> = Vec::new();

        if let Some(version) = request.version {
            if let Some(build) =
                self.find_by_commit_raw(request.project, version, request.commit)?
            {
                candidates.push((build, true));
            }
            for build in self.find_commit_in_versions_raw(request.project, request.commit)? {
                if build.manifest.version != version {
                    candidates.push((build, false));
                }
            }
        } else {
            for build in self.find_commit_in_versions_raw(request.project, request.commit)? {
                candidates.push((build, true));
            }
        }

        if candidates.is_empty() {
            return Ok(LookupResult::Miss(MissReason::NotCached));
        }

        // Versions the commit is cached under (for VersionMismatch reporting).
        let available: Vec<String> = candidates
            .iter()
            .filter(|(_, matched)| !matched)
            .map(|(b, _)| b.manifest.version.clone())
            .collect();

        // Whether a non-matching version may satisfy the request at all.
        let cross_version_ok =
            request.version.is_none() || request.version_match == VersionMatch::Lenient;

        // Track the most meaningful miss reason while scanning candidates:
        // an exact-version candidate's failure beats a generic NotCached.
        let mut miss_reason: Option<MissReason> = None;

        for (build, version_matched) in candidates {
            if !version_matched && !cross_version_ok {
                continue;
            }

            match self.evaluate_candidate(&build, request)? {
                CandidateOutcome::Usable => {
                    return Ok(LookupResult::Hit(LookupHit {
                        build,
                        version_matched,
                    }));
                }
                CandidateOutcome::Incomplete => {
                    miss_reason.get_or_insert(MissReason::Incomplete);
                }
                CandidateOutcome::MissingVariants(missing) => {
                    miss_reason.get_or_insert(MissReason::MissingVariants { missing });
                }
            }
        }

        // No usable candidate. Prefer the concrete failure of an evaluated
        // candidate; otherwise the only candidates were excluded by strict
        // version matching.
        let reason = match miss_reason {
            Some(reason) => reason,
            None if available.is_empty() => MissReason::NotCached,
            None => MissReason::VersionMismatch { available },
        };

        Ok(LookupResult::Miss(reason))
    }

    /// Evaluate whether one candidate build satisfies a request.
    fn evaluate_candidate(
        &self,
        build: &StoredBuild,
        request: &LookupRequest<'_>,
    ) -> Result<CandidateOutcome> {
        // Integrity first: a damaged build satisfies nothing.
        if !build.verify(VerifyMode::Size)?.is_empty() {
            return Ok(CandidateOutcome::Incomplete);
        }

        // Variant coverage (files already verified above, so manifest
        // presence is sufficient here).
        let missing: Vec<Option<String>> = request
            .required_variants
            .iter()
            .filter(|v| !build.manifest.has_variant(v.as_deref()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Ok(CandidateOutcome::MissingVariants(missing));
        }

        Ok(CandidateOutcome::Usable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::BuildSource;
    use crate::store::{BuildMetadata, SourceArtifact};
    use tempfile::TempDir;

    const COMMIT: &str = "abc1234567890abcdef1234567890abcdef12345";

    fn temp_store() -> (TempDir, ArtifactStore) {
        let dir = TempDir::new().unwrap();
        let store = ArtifactStore::new(dir.path().join("store"));
        (dir, store)
    }

    /// Store a build for `COMMIT` under the given version with the given
    /// variants (one file per variant).
    fn store_variants(
        dir: &std::path::Path,
        store: &ArtifactStore,
        version: &str,
        variants: &[Option<&str>],
    ) {
        let artifacts: Vec<SourceArtifact> = variants
            .iter()
            .map(|v| {
                let name = format!("backwpup-{version}-{}.zip", v.unwrap_or("default"));
                let path = dir.join(&name);
                std::fs::write(&path, format!("content {name}")).unwrap();
                SourceArtifact {
                    variant_id: v.map(|s| s.to_string()),
                    path,
                    target_name: name,
                }
            })
            .collect();
        let meta = BuildMetadata::new(
            "backwpup".to_string(),
            version.to_string(),
            BuildSource::Branch("develop".into()),
            COMMIT.to_string(),
            "develop".to_string(),
        );
        store.store(&artifacts, &meta).unwrap();
    }

    #[test]
    fn test_lookup_not_cached() {
        let (_dir, store) = temp_store();
        let result = store
            .lookup_build(&LookupRequest::new("backwpup", COMMIT))
            .unwrap();
        assert!(matches!(result, LookupResult::Miss(MissReason::NotCached)));
    }

    #[test]
    fn test_lookup_exact_version_hit() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        let request = LookupRequest::new("backwpup", COMMIT).version("5.6.0");
        let result = store.lookup_build(&request).unwrap();
        let hit = result.hit().expect("expected hit");
        assert!(hit.version_matched);
        assert_eq!(hit.build.manifest.version, "5.6.0");
    }

    #[test]
    fn test_lookup_short_commit_hits_full_stored() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        // Lookup by 7-char short of the stored full hash.
        let request = LookupRequest::new("backwpup", "abc1234").version("5.6.0");
        assert!(store.lookup_build(&request).unwrap().is_hit());
    }

    #[test]
    fn test_lookup_strict_version_mismatch() {
        let (dir, store) = temp_store();
        // Commit cached under 5.6.0, requested as 5.7.0 (version-aware
        // plugin, strict): must miss and report where it IS cached.
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        let request = LookupRequest::new("backwpup", COMMIT)
            .version("5.7.0")
            .version_match(VersionMatch::Strict);
        let result = store.lookup_build(&request).unwrap();
        match result {
            LookupResult::Miss(MissReason::VersionMismatch { available }) => {
                assert_eq!(available, vec!["5.6.0".to_string()]);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_lookup_lenient_version_mismatch_hits() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        // Lenient: the 5.6.0 build of the same commit is acceptable for a
        // 5.7.0 request, flagged as version_matched = false.
        let request = LookupRequest::new("backwpup", COMMIT)
            .version("5.7.0")
            .version_match(VersionMatch::Lenient);
        let result = store.lookup_build(&request).unwrap();
        let hit = result.hit().expect("expected lenient hit");
        assert!(!hit.version_matched);
        assert_eq!(hit.build.manifest.version, "5.6.0");
    }

    #[test]
    fn test_lookup_no_version_requested() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        let result = store
            .lookup_build(&LookupRequest::new("backwpup", COMMIT))
            .unwrap();
        let hit = result.hit().expect("expected hit");
        // No constraint given: reported as matched.
        assert!(hit.version_matched);
    }

    #[test]
    fn test_lookup_required_variants_hit_and_miss() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[Some("free"), Some("pro")]);

        let both = [Some("free".to_string()), Some("pro".to_string())];
        let request = LookupRequest::new("backwpup", COMMIT)
            .version("5.6.0")
            .require_variants(&both);
        assert!(store.lookup_build(&request).unwrap().is_hit());

        let with_extra = [Some("free".to_string()), Some("enterprise".to_string())];
        let request = LookupRequest::new("backwpup", COMMIT)
            .version("5.6.0")
            .require_variants(&with_extra);
        match store.lookup_build(&request).unwrap() {
            LookupResult::Miss(MissReason::MissingVariants { missing }) => {
                assert_eq!(missing, vec![Some("enterprise".to_string())]);
            }
            other => panic!("expected MissingVariants, got {other:?}"),
        }
    }

    #[test]
    fn test_lookup_incomplete_when_file_deleted() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        // Damage the cache behind the manifest.
        let commit_dir = store.paths().commit_dir("backwpup", "5.6.0", "abc1234");
        std::fs::remove_file(commit_dir.join("backwpup-5.6.0-default.zip")).unwrap();

        let request = LookupRequest::new("backwpup", COMMIT).version("5.6.0");
        let result = store.lookup_build(&request).unwrap();
        assert!(matches!(result, LookupResult::Miss(MissReason::Incomplete)));
    }

    #[test]
    fn test_lookup_incomplete_without_version_constraint() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        // Damage the only cached build.
        let commit_dir = store.paths().commit_dir("backwpup", "5.6.0", "abc1234");
        std::fs::remove_file(commit_dir.join("backwpup-5.6.0-default.zip")).unwrap();

        // With no version constraint the damaged build must still be
        // reported as Incomplete (rebuild + re-store heals), not NotCached.
        let result = store
            .lookup_build(&LookupRequest::new("backwpup", COMMIT))
            .unwrap();
        assert!(matches!(result, LookupResult::Miss(MissReason::Incomplete)));
    }

    #[test]
    fn test_lookup_lenient_falls_back_when_exact_version_damaged() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);
        store_variants(dir.path(), &store, "5.7.0", &[None]);

        // Damage the exact-version build; the other version stays healthy.
        let commit_dir = store.paths().commit_dir("backwpup", "5.6.0", "abc1234");
        std::fs::remove_file(commit_dir.join("backwpup-5.6.0-default.zip")).unwrap();

        let request = LookupRequest::new("backwpup", COMMIT)
            .version("5.6.0")
            .version_match(VersionMatch::Lenient);
        let hit = store.lookup_build(&request).unwrap().hit().unwrap();
        assert!(!hit.version_matched);
        assert_eq!(hit.build.manifest.version, "5.7.0");
    }

    #[test]
    fn test_lookup_prefers_exact_version_over_other_versions() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);
        store_variants(dir.path(), &store, "5.7.0", &[None]);

        let request = LookupRequest::new("backwpup", COMMIT)
            .version("5.6.0")
            .version_match(VersionMatch::Lenient);
        let hit = store.lookup_build(&request).unwrap().hit().unwrap();
        assert!(hit.version_matched);
        assert_eq!(hit.build.manifest.version, "5.6.0");
    }

    #[test]
    fn test_lookup_full_hash_prefix_mismatch_is_not_cached() {
        let (dir, store) = temp_store();
        store_variants(dir.path(), &store, "5.6.0", &[None]);

        // Same 7-char short as the stored build, different full hash: the
        // stored build is a different commit, so this must be NotCached —
        // never a hit, never VersionMismatch.
        let impostor = format!("{}{}", &COMMIT[..7], "0".repeat(33));
        let request = LookupRequest::new("backwpup", &impostor).version("5.6.0");
        let result = store.lookup_build(&request).unwrap();
        assert!(matches!(result, LookupResult::Miss(MissReason::NotCached)));
    }

    #[test]
    fn test_lookup_rejects_invalid_input() {
        let (_dir, store) = temp_store();
        assert!(
            store
                .lookup_build(&LookupRequest::new("../evil", COMMIT))
                .is_err()
        );
        assert!(
            store
                .lookup_build(&LookupRequest::new("backwpup", "nothex!"))
                .is_err()
        );
    }
}
