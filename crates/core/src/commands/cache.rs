//! Artifact-cache decisions for the build command.
//!
//! This module isolates every interaction with the [`ArtifactStore`] so the
//! build command reads as orchestration and the cache logic stays independently
//! testable (with a temp store, no git/GitHub). Two kinds of helpers live here:
//!
//! - **Pure planners** — [`variant_keys`] and [`predicted_version`] — decide
//!   *what* to look up, from the builder and request alone.
//! - **Best-effort store operations** — [`fast_path_lookup_and_copy`],
//!   [`reuse_and_copy`], [`store_build`] for builds, and
//!   [`reuse_release_assets`], [`store_release`] for release downloads — run
//!   the blocking SQLite + file I/O on [`tokio::task::spawn_blocking`] and
//!   treat **every** failure as a cache miss (or a skipped warm). A cache
//!   problem must never fail a build whose artifacts are otherwise fine.
//!
//! # `deliver` (build) vs. warm-only
//!
//! The reuse helpers take a `deliver` flag. When `true` (a normal build) a
//! cached artifact is copied into the output directory and the returned
//! artifact points there. When `false` (cache warming) nothing is copied and
//! the output directory is never touched — the returned artifact references
//! the file already in the cache. This is what lets `warm_cache` run the exact
//! same pipeline as `build` while delivering nothing to an output directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use apvm_storage::{
    ArtifactStore, BuildMetadata, LookupKey, LookupRequest, LookupResult, ReleaseMetadata,
    SourceArtifact, StoredBuild, StoredRelease,
};

use crate::build::plugins::Builder;
use crate::build::{ArtifactOrigin, ProducedArtifact};

/// The concrete variant keys a build request will deliver.
///
/// - Explicit `requested` variants map 1:1 to `Some(id)`.
/// - An empty request on a multi-variant builder expands to **all** of the
///   builder's [`variants`](Builder::variants) — matching what those builders
///   produce when handed no variants.
/// - An empty request on a single-output builder is the lone variant-less
///   artifact, `None`.
///
/// These keys are what the cache lookup requires and what a partial build
/// reuses/builds against, so they must mirror the builder's own output exactly.
pub(crate) fn variant_keys(builder: &dyn Builder, requested: &[&str]) -> Vec<Option<String>> {
    if !requested.is_empty() {
        return requested.iter().map(|v| Some((*v).to_string())).collect();
    }
    let all = builder.variants();
    if all.is_empty() {
        vec![None]
    } else {
        all.iter().map(|v| Some(v.id.to_string())).collect()
    }
}

/// The version a build can be predicted to produce **before** checkout, for the
/// pre-clone cache lookup.
///
/// Mirrors [`resolve_version`](crate::commands::build) so the predicted key
/// matches what the build will actually produce:
///
/// - an explicit `version` (`--ver`) is always what will be built;
/// - otherwise only [`Required`](crate::build::plugins::VersionRequirement::Required)
///   builders fall back to their default (that is the version their build uses);
/// - [`Optional`](crate::build::plugins::VersionRequirement::Optional) and
///   [`Embedded`](crate::build::plugins::VersionRequirement::Embedded) builders
///   derive the version from source, which cannot be known here — so they return
///   `None`. The caller can still learn it pre-clone by fetching the source
///   version file(s) (see `version_source_files`), or otherwise falls back to a
///   post-checkout lookup.
pub(crate) fn predicted_version(builder: &dyn Builder, version: Option<&str>) -> Option<String> {
    let requirement = builder.version_requirement();

    // Embedded ignores `--ver` entirely and derives the version from source,
    // so nothing is predictable here regardless of what the caller passed.
    if requirement.is_embedded() {
        return None;
    }

    // Required/Optional honor an explicit `--ver`.
    if let Some(v) = version {
        return Some(v.to_string());
    }

    // No `--ver`: only a `Required` builder falls back to its default (that is
    // the version its build uses); an `Optional` builder detects from source
    // and is not predictable statically.
    if requirement.is_required() {
        return builder.default_version().map(str::to_string);
    }
    None
}

/// A successful pre-clone cache hit whose artifacts are already in the output
/// directory.
pub(crate) struct FastPathHit {
    /// Full commit SHA of the cached build that was delivered.
    pub commit: String,
    /// Version of the cached build delivered. Equals the requested version
    /// (a hit requires an exact version match, or no version was requested).
    pub version: String,
    /// The artifacts copied into the output directory (origin = `Cache`).
    pub artifacts: Vec<ProducedArtifact>,
}

/// Pre-clone fast path (behavior-matrix cases A/B): look the commit up and,
/// on a full hit, materialize the requested variants.
///
/// A hit requires the cached build to match `version` exactly (when one is
/// supplied); a build of the same commit at a different version is a miss, so
/// the caller rebuilds/downloads at the requested version.
///
/// With `deliver = true` the variants are copied into `output_dir`; with
/// `deliver = false` (cache warming) nothing is copied and `output_dir` is
/// ignored — the returned artifacts reference the files already in the cache,
/// which is all a warm needs since the commit is already fully cached.
///
/// Returns `None` on a miss, on any storage/copy error (treated as a miss), or
/// if the blocking task panics — in every case the caller proceeds to build
/// normally, so the cache is never able to break a build.
pub(crate) async fn fast_path_lookup_and_copy(
    store: Arc<ArtifactStore>,
    project: String,
    commit: String,
    version: Option<String>,
    keys: Vec<Option<String>>,
    output_dir: PathBuf,
    deliver: bool,
) -> Option<FastPathHit> {
    let task = tokio::task::spawn_blocking(move || -> std::io::Result<Option<FastPathHit>> {
        let mut request = LookupRequest::new(&project, LookupKey::Commit(&commit));
        if let Some(v) = &version {
            request = request.version(v);
        }
        request = request.require_variants(&keys);

        let hit = match store.lookup_build(&request) {
            Ok(LookupResult::Hit(hit)) => hit,
            Ok(LookupResult::Miss(reason)) => {
                tracing::debug!(commit = %commit, ?reason, "cache miss (fast path)");
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!(error = %e, "cache lookup failed; treating as miss");
                return Ok(None);
            }
        };

        let artifacts = collect_variants(&hit.build, &keys, &output_dir, deliver)?;
        Ok(Some(FastPathHit {
            commit: hit.build.commit.clone(),
            version: hit.build.version.clone(),
            artifacts,
        }))
    });

    match task.await {
        Ok(Ok(hit)) => hit,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to copy cached artifacts; rebuilding");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "cache task failed; rebuilding");
            None
        }
    }
}

/// Outcome of post-checkout partial-reuse planning + copy.
pub(crate) struct ReuseOutcome {
    /// Cache-origin artifacts already copied into the output directory.
    pub reused: Vec<ProducedArtifact>,
    /// Variant keys still needing a build.
    pub to_build: Vec<Option<String>>,
}

/// Post-checkout partial reuse (behavior-matrix cases D/E/F): for the
/// authoritative `(version, commit)`, find whichever requested variants are
/// already cached and healthy, and report the rest as needing a build.
///
/// With `deliver = true` the cached variants are copied into `output_dir`;
/// with `deliver = false` (cache warming) nothing is copied and `output_dir`
/// is ignored — the reused artifacts reference the files already in the cache.
///
/// Any storage failure, or a per-variant copy failure, degrades safely: the
/// affected variants are simply added to `to_build` (a copy failure never
/// silently drops an artifact), and a total failure yields "build everything".
pub(crate) async fn reuse_and_copy(
    store: Arc<ArtifactStore>,
    project: String,
    version: String,
    commit: String,
    keys: Vec<Option<String>>,
    output_dir: PathBuf,
    deliver: bool,
) -> ReuseOutcome {
    let fallback = keys.clone();
    let task = tokio::task::spawn_blocking(move || {
        let build = match store.find_by_commit(&project, &version, &commit) {
            Ok(Some(build)) => build,
            Ok(None) => {
                return ReuseOutcome {
                    reused: Vec::new(),
                    to_build: keys,
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "cache lookup failed; building all variants");
                return ReuseOutcome {
                    reused: Vec::new(),
                    to_build: keys,
                };
            }
        };
        // Only a delivering build needs the output directory; warming never
        // writes there.
        if deliver && let Err(e) = std::fs::create_dir_all(&output_dir) {
            tracing::warn!(error = %e, "failed to prepare output dir; building all variants");
            return ReuseOutcome {
                reused: Vec::new(),
                to_build: keys,
            };
        }

        let mut reused = Vec::new();
        let mut to_build = Vec::new();
        for key in keys {
            match build.artifact(key.as_deref()) {
                Some(artifact) if !deliver => {
                    // Warm: the file is already in the cache; reference it in
                    // place without copying anything to an output directory.
                    reused.push(cache_artifact(artifact, artifact.path.clone()));
                }
                Some(artifact) => {
                    let dest = output_dir.join(&artifact.filename);
                    match std::fs::copy(&artifact.path, &dest) {
                        Ok(_) => reused.push(cache_artifact(artifact, dest)),
                        Err(e) => {
                            tracing::warn!(error = %e, variant = ?key, "failed to copy cached variant; rebuilding it");
                            to_build.push(key);
                        }
                    }
                }
                None => to_build.push(key),
            }
        }
        ReuseOutcome { reused, to_build }
    });

    task.await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cache reuse task failed; building all variants");
        ReuseOutcome {
            reused: Vec::new(),
            to_build: fallback,
        }
    })
}

/// Best-effort warm: store the delivered artifacts under their authoritative
/// `(version, commit)`.
///
/// Idempotent and self-healing — already-cached files are recognized and not
/// recopied, so passing the full requested set (reused + built) simply keeps
/// the row complete and refreshes its last-used timestamp. Never fails the
/// build; storage errors are logged and swallowed.
///
/// Returns the resulting [`StoredBuild`] on success (its artifacts carry their
/// canonical in-cache paths), or `None` when there was nothing to store or the
/// warm failed. `warm_cache` uses the returned build to re-point its output
/// artifacts at the cache; a normal build ignores the return value.
pub(crate) async fn store_build(
    store: Arc<ArtifactStore>,
    metadata: BuildMetadata,
    artifacts: Vec<SourceArtifact>,
) -> Option<StoredBuild> {
    if artifacts.is_empty() {
        return None;
    }
    let task = tokio::task::spawn_blocking(move || store.store(&metadata, &artifacts));
    match task.await {
        Ok(Ok(outcome)) => {
            tracing::debug!(
                newly_stored = outcome.newly_stored.len(),
                reused = outcome.reused.len(),
                "warmed artifact cache"
            );
            Some(outcome.build)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to warm artifact cache");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "cache store task failed");
            None
        }
    }
}

// ============================================================================
// Release assets (keyed by tag + filename)
// ============================================================================
//
// A release can have several variant-specific assets (e.g. BackWPup's
// `backwpup-free-5.6.8.zip`, `-pro-en-`, `-pro-de-`). Each variant is a
// *distinct filename*, so caching by (tag, filename) keeps them separate; the
// variant itself is encoded in the filename and recovered by the builder
// (`variant_from_release_asset`) when the delivered artifacts are assembled.
// The storage layer doesn't carry a variant column for release assets — it
// keys them by filename — which is why these helpers work on filenames.

/// A release asset copied from the cache into the output directory.
pub(crate) struct CachedAsset {
    /// Asset filename.
    pub filename: String,
    /// Absolute path of the copy delivered to the output directory.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
}

/// Outcome of release-asset reuse.
pub(crate) struct ReleaseReuseOutcome {
    /// Assets copied from the cache into the output directory.
    pub cached: Vec<CachedAsset>,
    /// Requested asset filenames still needing a download.
    pub to_download: Vec<String>,
}

/// Find whichever requested release assets are already cached (by exact tag +
/// filename), and report the rest as needing a download.
///
/// With `deliver = true` the cached assets are copied into `output_dir`; with
/// `deliver = false` (cache warming) nothing is copied and `output_dir` is
/// ignored — the returned assets reference the files already in the cache.
///
/// `requested` is the set of asset filenames the caller wants — already
/// filtered to the requested variants, since each variant is a distinct
/// filename. Any storage failure, or a per-asset copy failure, degrades
/// safely: the affected asset is added to `to_download`, and a total failure
/// yields "download everything" — never an error.
pub(crate) async fn reuse_release_assets(
    store: Arc<ArtifactStore>,
    project: String,
    tag: String,
    requested: Vec<String>,
    output_dir: PathBuf,
    deliver: bool,
) -> ReleaseReuseOutcome {
    let fallback = requested.clone();
    let task = tokio::task::spawn_blocking(move || {
        let release = match store.find_release(&project, &tag) {
            Ok(Some(release)) => release,
            Ok(None) => {
                return ReleaseReuseOutcome {
                    cached: Vec::new(),
                    to_download: requested,
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "release cache lookup failed; downloading all assets");
                return ReleaseReuseOutcome {
                    cached: Vec::new(),
                    to_download: requested,
                };
            }
        };
        // Only a delivering download needs the output directory; warming never
        // writes there.
        if deliver && let Err(e) = std::fs::create_dir_all(&output_dir) {
            tracing::warn!(error = %e, "failed to prepare output dir; downloading all assets");
            return ReleaseReuseOutcome {
                cached: Vec::new(),
                to_download: requested,
            };
        }

        let mut cached = Vec::new();
        let mut to_download = Vec::new();
        for name in requested {
            match release.assets.iter().find(|asset| asset.filename == name) {
                Some(asset) if !deliver => {
                    // Warm: the asset is already cached; reference it in place.
                    cached.push(CachedAsset {
                        filename: asset.filename.clone(),
                        path: asset.path.clone(),
                        size: asset.size_bytes,
                    });
                }
                Some(asset) => {
                    let dest = output_dir.join(&asset.filename);
                    match std::fs::copy(&asset.path, &dest) {
                        Ok(_) => cached.push(CachedAsset {
                            filename: asset.filename.clone(),
                            path: dest,
                            size: asset.size_bytes,
                        }),
                        Err(e) => {
                            tracing::warn!(error = %e, asset = %name, "failed to copy cached asset; downloading it");
                            to_download.push(name);
                        }
                    }
                }
                None => to_download.push(name),
            }
        }
        ReleaseReuseOutcome {
            cached,
            to_download,
        }
    });

    task.await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "release reuse task failed; downloading all assets");
        ReleaseReuseOutcome {
            cached: Vec::new(),
            to_download: fallback,
        }
    })
}

/// Best-effort warm: cache a release's delivered assets under its tag.
///
/// Idempotent and self-healing, mirroring [`store_build`]; already-cached
/// assets are recognized and not recopied. Never fails the download.
///
/// Returns the resulting [`StoredRelease`] on success (its assets carry their
/// canonical in-cache paths), or `None` when there was nothing to store or the
/// warm failed. `warm_cache` uses the returned release to re-point its output
/// artifacts at the cache; a normal download ignores the return value.
pub(crate) async fn store_release(
    store: Arc<ArtifactStore>,
    metadata: ReleaseMetadata,
    assets: Vec<SourceArtifact>,
) -> Option<StoredRelease> {
    if assets.is_empty() {
        return None;
    }
    let task = tokio::task::spawn_blocking(move || store.store_release(&metadata, &assets));
    match task.await {
        Ok(Ok(outcome)) => {
            tracing::debug!(
                newly_stored = outcome.newly_stored.len(),
                reused = outcome.reused.len(),
                "warmed release cache"
            );
            Some(outcome.release)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to warm release cache");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "release store task failed");
            None
        }
    }
}

/// Materialize every `key` from a stored build into cache-origin
/// [`ProducedArtifact`]s.
///
/// With `deliver = true` each variant is copied into `output_dir` and the
/// returned artifact points there. With `deliver = false` (cache warming)
/// nothing is copied and `output_dir` is untouched — the returned artifact
/// references the file already in the cache.
///
/// Errors if a key is absent (defensive — the fast path's `require_variants`
/// should guarantee presence) or a copy fails, so the caller can fall back to
/// building.
fn collect_variants(
    build: &StoredBuild,
    keys: &[Option<String>],
    output_dir: &Path,
    deliver: bool,
) -> std::io::Result<Vec<ProducedArtifact>> {
    if deliver {
        std::fs::create_dir_all(output_dir)?;
    }
    let mut produced = Vec::with_capacity(keys.len());
    for key in keys {
        let artifact = build.artifact(key.as_deref()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("cached build is missing variant {key:?}"),
            )
        })?;
        let path = if deliver {
            let dest = output_dir.join(&artifact.filename);
            std::fs::copy(&artifact.path, &dest)?;
            dest
        } else {
            artifact.path.clone()
        };
        produced.push(cache_artifact(artifact, path));
    }
    Ok(produced)
}

/// Build a cache-origin [`ProducedArtifact`] for a stored artifact copied to
/// `dest`.
fn cache_artifact(artifact: &apvm_storage::StoredArtifact, dest: PathBuf) -> ProducedArtifact {
    ProducedArtifact::new(
        artifact.variant_id.clone(),
        dest,
        artifact.filename.clone(),
        artifact.size_bytes,
    )
    .with_origin(ArtifactOrigin::Cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::plugins::{BuildArtifact, BuildVariant, VersionRequirement};
    use crate::build::progress::BuildStep;
    use apvm_storage::BuildSource;

    // ---- Test builder: configurable variants + version requirement --------

    struct FakeBuilder {
        variants: Vec<&'static str>,
        requirement: VersionRequirement,
        default_version: Option<&'static str>,
    }

    impl Builder for FakeBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            self.requirement
        }
        fn default_version(&self) -> Option<&'static str> {
            self.default_version
        }
        fn variants(&self) -> Vec<BuildVariant> {
            self.variants
                .iter()
                .map(|id| BuildVariant {
                    id,
                    name: id,
                    description: id,
                })
                .collect()
        }
        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }
        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            vec![]
        }
        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }
    }

    fn multi_variant() -> FakeBuilder {
        FakeBuilder {
            variants: vec!["free", "pro-de", "pro-en"],
            requirement: VersionRequirement::Required,
            default_version: Some("9.99.99"),
        }
    }

    fn single_output_embedded() -> FakeBuilder {
        FakeBuilder {
            variants: vec![],
            requirement: VersionRequirement::Embedded,
            default_version: None,
        }
    }

    fn optional_no_default() -> FakeBuilder {
        FakeBuilder {
            variants: vec![],
            requirement: VersionRequirement::Optional,
            default_version: None,
        }
    }

    /// Optional builder that *advertises* a default — used to confirm the
    /// default is NOT used as the predicted version (Optional detects from
    /// source, so the default would be a wrong cache key).
    fn optional_with_default() -> FakeBuilder {
        FakeBuilder {
            variants: vec![],
            requirement: VersionRequirement::Optional,
            default_version: Some("9.99.99"),
        }
    }

    // ---- variant_keys -----------------------------------------------------

    #[test]
    fn variant_keys_explicit_request() {
        let b = multi_variant();
        assert_eq!(
            variant_keys(&b, &["free", "pro-en"]),
            vec![Some("free".to_string()), Some("pro-en".to_string())]
        );
    }

    #[test]
    fn variant_keys_empty_request_expands_to_all_variants() {
        let b = multi_variant();
        assert_eq!(
            variant_keys(&b, &[]),
            vec![
                Some("free".to_string()),
                Some("pro-de".to_string()),
                Some("pro-en".to_string()),
            ]
        );
    }

    #[test]
    fn variant_keys_single_output_is_variant_less() {
        let b = single_output_embedded();
        assert_eq!(variant_keys(&b, &[]), vec![None]);
    }

    // ---- predicted_version ------------------------------------------------

    #[test]
    fn predicted_version_embedded_is_none() {
        // Embedded derives its version from source and ignores `--ver`, so it
        // is never predictable — with or without an explicit version.
        let b = single_output_embedded();
        assert_eq!(predicted_version(&b, Some("3.0.0")), None);
        assert_eq!(predicted_version(&b, None), None);
    }

    #[test]
    fn predicted_version_uses_explicit_then_default() {
        let b = multi_variant();
        assert_eq!(
            predicted_version(&b, Some("5.1.0")),
            Some("5.1.0".to_string())
        );
        // Required builder with no `--ver` falls back to its default.
        assert_eq!(predicted_version(&b, None), Some("9.99.99".to_string()));
    }

    #[test]
    fn predicted_version_optional_never_uses_default() {
        // Optional builders detect from source when no `--ver` is given, so the
        // default must NOT become the predicted key (it would mismatch what the
        // build actually produces). An explicit `--ver` is still honored.
        let no_default = optional_no_default();
        assert_eq!(predicted_version(&no_default, None), None);
        assert_eq!(
            predicted_version(&no_default, Some("3.17.4")),
            Some("3.17.4".to_string())
        );

        let with_default = optional_with_default();
        assert_eq!(
            predicted_version(&with_default, None),
            None,
            "an Optional builder's default is not the version it builds"
        );
        assert_eq!(
            predicted_version(&with_default, Some("3.17.4")),
            Some("3.17.4".to_string())
        );
    }

    // ---- store-backed helpers (temp ArtifactStore, no git/GitHub) ---------

    /// Seed a build into a temp store with the given variants at `version`.
    /// Returns the store base dir and the store.
    fn seed_store(
        variants: &[(Option<&str>, &str)], // (variant_id, filename)
        version: &str,
        commit: &str,
    ) -> (tempfile::TempDir, Arc<ArtifactStore>) {
        let base = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(base.path()).unwrap();
        let src = tempfile::tempdir().unwrap();

        let mut source_artifacts = Vec::new();
        for (variant_id, filename) in variants {
            let path = src.path().join(filename);
            std::fs::write(&path, format!("contents-of-{filename}")).unwrap();
            source_artifacts.push(SourceArtifact {
                variant_id: variant_id.map(str::to_string),
                path,
                target_name: filename.to_string(),
            });
        }
        let metadata = BuildMetadata::new(
            "backwpup",
            version,
            BuildSource::PullRequest(123),
            commit,
            "develop".to_string(),
        );
        store.store(&metadata, &source_artifacts).unwrap();
        (base, Arc::new(store))
    }

    #[tokio::test]
    async fn fast_path_full_hit_copies_all_requested() {
        let (_base, store) = seed_store(
            &[(Some("free"), "free.zip"), (Some("pro-en"), "pro-en.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        let keys = vec![Some("free".to_string()), Some("pro-en".to_string())];

        let hit = fast_path_lookup_and_copy(
            store,
            "backwpup".to_string(),
            "a1b2c3d".to_string(), // short SHA still matches
            Some("5.6.0".to_string()),
            keys,
            out.path().to_path_buf(),
            true,
        )
        .await
        .expect("expected a full cache hit");

        assert_eq!(hit.version, "5.6.0");
        assert_eq!(hit.artifacts.len(), 2);
        assert!(
            hit.artifacts
                .iter()
                .all(|a| a.origin == ArtifactOrigin::Cache)
        );
        assert!(out.path().join("free.zip").is_file());
        assert!(out.path().join("pro-en.zip").is_file());
    }

    #[tokio::test]
    async fn fast_path_misses_when_a_variant_is_absent() {
        let (_base, store) = seed_store(
            &[(Some("free"), "free.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        // Request free + pro-en, but only free is cached → miss (not a partial).
        let keys = vec![Some("free".to_string()), Some("pro-en".to_string())];

        let hit = fast_path_lookup_and_copy(
            store,
            "backwpup".to_string(),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string(),
            Some("5.6.0".to_string()),
            keys,
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert!(hit.is_none());
        assert!(
            !out.path().join("free.zip").exists(),
            "no partial copy on a full-hit miss"
        );
    }

    #[tokio::test]
    async fn fast_path_misses_on_version_mismatch() {
        // The cache holds this commit at 5.6.0; pinning a different version must
        // MISS (a build at another version is never served in its place), so the
        // caller rebuilds/downloads at the requested version.
        let (_base, store) = seed_store(
            &[(Some("free"), "free.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        let keys = vec![Some("free".to_string())];

        let hit = fast_path_lookup_and_copy(
            store,
            "backwpup".to_string(),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string(),
            Some("9.9.9".to_string()),
            keys,
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert!(
            hit.is_none(),
            "a different cached version must not be served"
        );
        assert!(
            !out.path().join("free.zip").exists(),
            "a version mismatch must not copy anything"
        );
    }

    #[tokio::test]
    async fn reuse_partitions_present_and_missing() {
        let (_base, store) = seed_store(
            &[(Some("free"), "free.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        let keys = vec![Some("free".to_string()), Some("pro-en".to_string())];

        let outcome = reuse_and_copy(
            store,
            "backwpup".to_string(),
            "5.6.0".to_string(),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string(),
            keys,
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert_eq!(outcome.reused.len(), 1, "free is cached and reused");
        assert_eq!(outcome.reused[0].variant_id.as_deref(), Some("free"));
        assert_eq!(outcome.reused[0].origin, ArtifactOrigin::Cache);
        assert_eq!(outcome.to_build, vec![Some("pro-en".to_string())]);
        assert!(out.path().join("free.zip").is_file());
    }

    #[tokio::test]
    async fn reuse_uncached_commit_builds_everything() {
        let (_base, store) = seed_store(
            &[(Some("free"), "free.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        let keys = vec![Some("free".to_string())];

        // Different commit → nothing cached → build all requested.
        let outcome = reuse_and_copy(
            store,
            "backwpup".to_string(),
            "5.6.0".to_string(),
            "ffffffffffffffffffffffffffffffffffffffff".to_string(),
            keys.clone(),
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert!(outcome.reused.is_empty());
        assert_eq!(outcome.to_build, keys);
    }

    #[tokio::test]
    async fn store_build_warms_the_cache() {
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::open(base.path()).unwrap());
        let src = tempfile::tempdir().unwrap();
        let zip = src.path().join("free.zip");
        std::fs::write(&zip, b"zip").unwrap();

        let metadata = BuildMetadata::new(
            "backwpup",
            "5.6.0",
            BuildSource::PullRequest(1),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
            "develop".to_string(),
        );
        let artifacts = vec![SourceArtifact {
            variant_id: Some("free".to_string()),
            path: zip,
            target_name: "free.zip".to_string(),
        }];

        store_build(Arc::clone(&store), metadata, artifacts).await;

        // The build is now findable.
        let found = store
            .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
            .unwrap();
        assert!(found.is_some());
    }

    // ---- release helpers --------------------------------------------------

    /// Seed a cached release for `project`/`tag` with the given asset filenames.
    fn seed_release(
        project: &str,
        tag: &str,
        assets: &[&str],
    ) -> (tempfile::TempDir, Arc<ArtifactStore>) {
        let base = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(base.path()).unwrap();
        let src = tempfile::tempdir().unwrap();
        let source_assets: Vec<SourceArtifact> = assets
            .iter()
            .map(|name| {
                let path = src.path().join(name);
                std::fs::write(&path, format!("asset-{name}")).unwrap();
                SourceArtifact {
                    variant_id: None,
                    path,
                    target_name: name.to_string(),
                }
            })
            .collect();
        store
            .store_release(&ReleaseMetadata::new(project, tag), &source_assets)
            .unwrap();
        (base, Arc::new(store))
    }

    // Realistic BackWPup release assets: each variant is a distinct filename,
    // so the (tag, filename)-keyed cache distinguishes them.
    const FREE: &str = "backwpup-free-5.6.8.zip";
    const PRO_EN: &str = "backwpup-pro-en-5.6.8.zip";
    const PRO_DE: &str = "backwpup-pro-de-5.6.8.zip";

    #[tokio::test]
    async fn release_reuse_full_hit_copies_all_variants() {
        // All three variant assets cached; requesting all three → full hit.
        let (_base, store) = seed_release("backwpup", "v5.6.8", &[FREE, PRO_EN, PRO_DE]);
        let out = tempfile::tempdir().unwrap();

        let outcome = reuse_release_assets(
            store,
            "backwpup".to_string(),
            "v5.6.8".to_string(),
            vec![FREE.to_string(), PRO_EN.to_string(), PRO_DE.to_string()],
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert_eq!(outcome.cached.len(), 3);
        assert!(outcome.to_download.is_empty(), "nothing left to download");
        assert!(out.path().join(FREE).is_file());
        assert!(out.path().join(PRO_EN).is_file());
        assert!(out.path().join(PRO_DE).is_file());
    }

    #[tokio::test]
    async fn release_reuse_new_variant_downloads_only_missing() {
        // Only the `free` variant is cached; requesting free + pro-en reuses
        // free and downloads only pro-en — the per-variant partial case.
        let (_base, store) = seed_release("backwpup", "v5.6.8", &[FREE]);
        let out = tempfile::tempdir().unwrap();

        let outcome = reuse_release_assets(
            store,
            "backwpup".to_string(),
            "v5.6.8".to_string(),
            vec![FREE.to_string(), PRO_EN.to_string()],
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert_eq!(outcome.cached.len(), 1);
        assert_eq!(outcome.cached[0].filename, FREE);
        assert_eq!(outcome.to_download, vec![PRO_EN.to_string()]);
        assert!(out.path().join(FREE).is_file());
        assert!(!out.path().join(PRO_EN).exists());
    }

    #[tokio::test]
    async fn release_reuse_does_not_confuse_variants() {
        // `free` cached; requesting *only* pro-en must NOT reuse free — it
        // downloads pro-en. Guards against variant collision.
        let (_base, store) = seed_release("backwpup", "v5.6.8", &[FREE]);
        let out = tempfile::tempdir().unwrap();

        let outcome = reuse_release_assets(
            store,
            "backwpup".to_string(),
            "v5.6.8".to_string(),
            vec![PRO_EN.to_string()],
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert!(
            outcome.cached.is_empty(),
            "free must not satisfy a pro-en request"
        );
        assert_eq!(outcome.to_download, vec![PRO_EN.to_string()]);
    }

    #[tokio::test]
    async fn release_reuse_uncached_tag_downloads_all() {
        let (_base, store) = seed_release("backwpup", "v5.6.8", &[FREE]);
        let out = tempfile::tempdir().unwrap();

        // A different tag isn't cached → everything must be downloaded.
        let requested = vec![FREE.to_string()];
        let outcome = reuse_release_assets(
            store,
            "backwpup".to_string(),
            "v9.9.9".to_string(),
            requested.clone(),
            out.path().to_path_buf(),
            true,
        )
        .await;

        assert!(outcome.cached.is_empty());
        assert_eq!(outcome.to_download, requested);
    }

    #[tokio::test]
    async fn store_release_warms_the_cache() {
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::open(base.path()).unwrap());
        let src = tempfile::tempdir().unwrap();
        let zip = src.path().join("free.zip");
        std::fs::write(&zip, b"asset").unwrap();

        let metadata = ReleaseMetadata::new("backwpup", "v5.6.8");
        let assets = vec![SourceArtifact {
            variant_id: None,
            path: zip,
            target_name: "free.zip".to_string(),
        }];

        store_release(Arc::clone(&store), metadata, assets).await;

        assert!(store.has_release("backwpup", "v5.6.8").unwrap());
    }

    // ---- warm mode (`deliver = false`): reference the cache, write nothing ----

    /// The output directory must be empty (contain no entries).
    fn assert_output_empty(dir: &std::path::Path) {
        assert!(
            std::fs::read_dir(dir).unwrap().next().is_none(),
            "warm mode must not write anything to the output directory"
        );
    }

    #[tokio::test]
    async fn fast_path_warm_references_cache_without_copying() {
        let (base, store) = seed_store(
            &[(Some("free"), "free.zip"), (Some("pro-en"), "pro-en.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        let keys = vec![Some("free".to_string()), Some("pro-en".to_string())];

        let hit = fast_path_lookup_and_copy(
            store,
            "backwpup".to_string(),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string(),
            Some("5.6.0".to_string()),
            keys,
            out.path().to_path_buf(),
            false, // deliver = false → warm
        )
        .await
        .expect("full hit expected");

        assert_eq!(hit.artifacts.len(), 2);
        assert!(
            hit.artifacts
                .iter()
                .all(|a| a.origin == ArtifactOrigin::Cache)
        );
        // Every artifact references the file already inside the cache...
        for artifact in &hit.artifacts {
            assert!(
                artifact.path.starts_with(base.path()),
                "warm artifact must reference the in-cache file, got {}",
                artifact.path.display()
            );
            assert!(artifact.path.is_file(), "referenced cache file must exist");
        }
        // ...and nothing was copied to the output directory.
        assert_output_empty(out.path());
    }

    #[tokio::test]
    async fn reuse_warm_references_cache_without_copying() {
        let (base, store) = seed_store(
            &[(Some("free"), "free.zip")],
            "5.6.0",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        );
        let out = tempfile::tempdir().unwrap();
        let keys = vec![Some("free".to_string()), Some("pro-en".to_string())];

        let outcome = reuse_and_copy(
            store,
            "backwpup".to_string(),
            "5.6.0".to_string(),
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string(),
            keys,
            out.path().to_path_buf(),
            false, // warm
        )
        .await;

        assert_eq!(outcome.reused.len(), 1, "free is cached and reused");
        assert_eq!(outcome.reused[0].variant_id.as_deref(), Some("free"));
        assert_eq!(outcome.reused[0].origin, ArtifactOrigin::Cache);
        assert!(
            outcome.reused[0].path.starts_with(base.path()),
            "reused artifact must reference the in-cache file"
        );
        assert_eq!(outcome.to_build, vec![Some("pro-en".to_string())]);
        assert_output_empty(out.path());
    }

    #[tokio::test]
    async fn release_reuse_warm_references_cache_without_copying() {
        let (base, store) = seed_release("backwpup", "v5.6.8", &[FREE, PRO_EN]);
        let out = tempfile::tempdir().unwrap();

        let outcome = reuse_release_assets(
            store,
            "backwpup".to_string(),
            "v5.6.8".to_string(),
            vec![FREE.to_string(), PRO_EN.to_string()],
            out.path().to_path_buf(),
            false, // warm
        )
        .await;

        assert_eq!(outcome.cached.len(), 2);
        assert!(outcome.to_download.is_empty(), "both assets already cached");
        for asset in &outcome.cached {
            assert!(
                asset.path.starts_with(base.path()),
                "warm asset must reference the cached file, got {}",
                asset.path.display()
            );
            assert!(asset.path.is_file(), "referenced cache file must exist");
        }
        assert_output_empty(out.path());
    }
}
