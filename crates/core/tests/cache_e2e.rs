//! End-to-end build-cache tests through the real pipeline: a local git
//! repository, real builder commands, and a real `ArtifactStore` — no network.
//!
//! These lock the caching contract of `Apvm::build`:
//!
//! - a first build misses, builds fresh, and **warms** the cache (even with
//!   `no_cache`, which only skips cache *reads*);
//! - an identical second build is a pre-clone **fast-path hit** (no clone, no
//!   build) delivering byte-identical artifacts;
//! - a request for extra variants **reuses** the cached ones and builds only
//!   the missing ones (mixed per-artifact provenance).
//!
//! They also lock the contract of `Apvm::warm_cache`, which runs the same
//! pipeline but delivers nothing to an output directory:
//!
//! - a warm of an uncached ref **builds + caches** it (delivering nothing), and
//!   a following `build` of the same ref is then a full cache hit;
//! - a warm of an already-cached ref **reuses** everything (nothing rebuilt);
//! - a warm requesting extra variants builds only the missing ones, and every
//!   warmed artifact lives inside the cache store (never an output directory).
//!
//! Requires `git` and `sh` in `PATH` (the same assumption the in-crate git
//! tests make), so the file is Unix-gated.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use apvm_core::build::BuildContext;
use apvm_core::build::plugins::{BuildArtifact, BuildVariant, Builder, VersionRequirement};
use apvm_core::build::progress::BuildStep;
use apvm_core::projects::Project;
use apvm_core::{
    Apvm, ArtifactOrigin, BuildEvent, BuildOutput, BuildRequest, ClosureReporter, Config,
    NullReporter, WarmRequest,
};

// ─────────────────────────────────────────────────────────────────────────────
// Test builders (run through `sh`, produce deterministic zip stand-ins)
// ─────────────────────────────────────────────────────────────────────────────

/// Single-output builder: one `myplugin.zip` artifact stamped with the version.
struct SingleBuilder;

impl Builder for SingleBuilder {
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Required
    }
    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![]
    }
    fn build_commands(&self, _: &BuildContext, version: &str, _: &[&str]) -> Vec<BuildStep> {
        vec![BuildStep::new(
            "make artifact",
            format!("printf 'plugin-%s' '{version}' > myplugin.zip"),
        )]
    }
    fn artifacts(
        &self,
        _: &BuildContext,
        _: &str,
        _: &[&str],
    ) -> apvm_core::Result<Vec<BuildArtifact>> {
        Ok(vec![BuildArtifact {
            variant_id: None,
            source_path: "myplugin.zip".to_string(),
            target_name: "myplugin.zip".to_string(),
        }])
    }
}

/// Two-variant builder (`free` / `pro`), mirroring how the real multi-variant
/// builders behave: an empty variant list builds everything.
struct MultiBuilder;

impl MultiBuilder {
    fn to_build<'a>(variants: &'a [&'a str]) -> Vec<&'a str> {
        if variants.is_empty() {
            vec!["free", "pro"]
        } else {
            variants.to_vec()
        }
    }
}

impl Builder for MultiBuilder {
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Required
    }
    fn variants(&self) -> Vec<BuildVariant> {
        vec![
            BuildVariant {
                id: "free",
                name: "Free",
                description: "free variant",
            },
            BuildVariant {
                id: "pro",
                name: "Pro",
                description: "pro variant",
            },
        ]
    }
    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![]
    }
    fn build_commands(&self, _: &BuildContext, version: &str, variants: &[&str]) -> Vec<BuildStep> {
        Self::to_build(variants)
            .iter()
            .map(|v| {
                BuildStep::new(
                    format!("build {v}"),
                    format!("printf '{v}-%s' '{version}' > {v}.zip"),
                )
            })
            .collect()
    }
    fn artifacts(
        &self,
        _: &BuildContext,
        _: &str,
        variants: &[&str],
    ) -> apvm_core::Result<Vec<BuildArtifact>> {
        Ok(Self::to_build(variants)
            .iter()
            .map(|v| BuildArtifact {
                variant_id: Some(v.to_string()),
                source_path: format!("{v}.zip"),
                target_name: format!("{v}.zip"),
            })
            .collect())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fixture helpers
// ─────────────────────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("failed to spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Create a local repository with one commit on `main`, usable as a clone URL.
fn init_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("tempdir");
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "test@apvm.dev"]);
    git(repo.path(), &["config", "user.name", "apvm-test"]);
    std::fs::write(repo.path().join("readme.txt"), b"fixture").expect("write");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "init"]);
    // Normalize the branch name regardless of the machine's init.defaultBranch.
    git(repo.path(), &["branch", "-M", "main"]);
    repo
}

/// An `Apvm` instance with one registered project backed by `repo`, caching
/// into `cache_dir`.
fn apvm_for(repo: &Path, cache_dir: &Path, name: &str, builder: Box<dyn Builder>) -> Apvm {
    let mut apvm = Apvm::new_empty(Config::new(cache_dir.to_path_buf())).expect("Apvm::new_empty");
    assert!(apvm.cache_active(), "cache must be active for these tests");
    apvm.register_project(Project {
        name: name.to_string(),
        repo_url: repo.to_string_lossy().to_string(),
        owner: "test".to_string(),
        repo: "fixture".to_string(),
        default_branch: "main".to_string(),
        is_private: false,
        has_releases: false,
        builder,
    });
    apvm
}

fn origin_of(output: &BuildOutput, variant: Option<&str>) -> ArtifactOrigin {
    output
        .result
        .artifacts
        .iter()
        .find(|a| a.variant_id.as_deref() == variant)
        .unwrap_or_else(|| panic!("artifact for variant {variant:?} not delivered"))
        .origin
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Full cycle: `no_cache` build warms the cache, the next build is a
/// fast-path hit with byte-identical artifacts.
#[tokio::test]
async fn no_cache_build_warms_then_next_build_hits() {
    let repo = init_repo();
    let cache = tempfile::tempdir().unwrap();
    let apvm = apvm_for(
        repo.path(),
        &cache.path().join("store"),
        "single",
        Box::new(SingleBuilder),
    );
    let request = |out: &Path| {
        BuildRequest::new("single", "branch:main", out).version(Some("1.0.0".to_string()))
    };

    // Build 1: empty cache + no_cache → fresh build; must still warm the cache.
    let out1 = tempfile::tempdir().unwrap();
    let build1 = apvm
        .build(request(out1.path()).no_cache(true), &NullReporter)
        .await
        .expect("build 1");
    assert!(!build1.from_cache(), "first build must be fresh");
    assert_eq!(origin_of(&build1, None), ArtifactOrigin::Built);
    assert!(!build1.cache_version_mismatch);
    assert!(out1.path().join("myplugin.zip").is_file());

    // Build 2: same commit → pre-clone fast-path hit (proves build 1 warmed).
    let out2 = tempfile::tempdir().unwrap();
    let build2 = apvm
        .build(request(out2.path()), &NullReporter)
        .await
        .expect("build 2");
    assert!(
        build2.from_cache(),
        "second build must be served from cache"
    );
    assert_eq!(origin_of(&build2, None), ArtifactOrigin::Cache);
    assert_eq!(build1.commit, build2.commit, "same commit both builds");
    assert_eq!(build2.result.version, "1.0.0");

    // The cached copy is the genuine artifact.
    let first = std::fs::read(out1.path().join("myplugin.zip")).unwrap();
    let second = std::fs::read(out2.path().join("myplugin.zip")).unwrap();
    assert_eq!(first, second, "cached bytes must match the built artifact");
}

/// Partial reuse: with only `free` cached, requesting `free,pro` reuses
/// `free` (origin cache), builds only `pro` (origin built), and a repeat
/// request is then a full hit.
#[tokio::test]
async fn partial_reuse_builds_only_missing_variants() {
    let repo = init_repo();
    let cache = tempfile::tempdir().unwrap();
    let apvm = apvm_for(
        repo.path(),
        &cache.path().join("store"),
        "multi",
        Box::new(MultiBuilder),
    );
    let request = |out: &Path, variants: &[&str]| {
        BuildRequest::new("multi", "branch:main", out)
            .version(Some("2.0.0".to_string()))
            .variants(variants.iter().map(|v| v.to_string()).collect())
    };

    // Build 1: only `free` → cached.
    let out1 = tempfile::tempdir().unwrap();
    let build1 = apvm
        .build(request(out1.path(), &["free"]), &NullReporter)
        .await
        .expect("build 1");
    assert!(!build1.from_cache());
    assert_eq!(build1.result.artifacts.len(), 1);

    // Build 2: `free` + `pro` → free reused from cache, pro built fresh.
    let out2 = tempfile::tempdir().unwrap();
    let build2 = apvm
        .build(request(out2.path(), &["free", "pro"]), &NullReporter)
        .await
        .expect("build 2");
    assert!(
        !build2.from_cache(),
        "a partial build is not fully from cache"
    );
    assert_eq!(build2.result.artifacts.len(), 2);
    assert_eq!(origin_of(&build2, Some("free")), ArtifactOrigin::Cache);
    assert_eq!(origin_of(&build2, Some("pro")), ArtifactOrigin::Built);
    assert!(out2.path().join("free.zip").is_file());
    assert!(out2.path().join("pro.zip").is_file());

    // Build 3: both variants now cached → full fast-path hit.
    let out3 = tempfile::tempdir().unwrap();
    let build3 = apvm
        .build(request(out3.path(), &["free", "pro"]), &NullReporter)
        .await
        .expect("build 3");
    assert!(build3.from_cache(), "both variants cached → full hit");
    assert_eq!(origin_of(&build3, Some("free")), ArtifactOrigin::Cache);
    assert_eq!(origin_of(&build3, Some("pro")), ArtifactOrigin::Cache);
}

// ─────────────────────────────────────────────────────────────────────────────
// warm_cache: same pipeline, delivers nothing to an output directory
// ─────────────────────────────────────────────────────────────────────────────

/// Warming an uncached ref builds and caches it while delivering nothing; a
/// subsequent build of the same ref is then a full cache hit, and the warmed
/// artifact lives inside the cache store (never an output directory).
#[tokio::test]
async fn warm_cache_populates_cache_without_output() {
    let repo = init_repo();
    let cache = tempfile::tempdir().unwrap();
    let store_dir = cache.path().join("store");
    let apvm = apvm_for(repo.path(), &store_dir, "single", Box::new(SingleBuilder));

    // Warm: builds fresh (nothing cached yet) but delivers nothing anywhere.
    let warm = apvm
        .warm_cache(
            WarmRequest::new("single", "branch:main").version(Some("1.0.0".to_string())),
            &NullReporter,
        )
        .await
        .expect("warm");
    assert_eq!(warm.result.artifacts.len(), 1);
    assert_eq!(
        origin_of(&warm, None),
        ArtifactOrigin::Built,
        "first warm builds fresh"
    );

    // The warmed artifact lives inside the cache store, not an output dir.
    let warmed = &warm.result.artifacts[0];
    assert!(
        warmed.path.starts_with(&store_dir),
        "warm artifact must live in the cache store, got {}",
        warmed.path.display()
    );
    assert!(warmed.path.is_file(), "warmed cache file must exist");

    // A later real build of the same ref is now a full cache hit — proof the
    // warm populated the cache — with byte-identical artifacts.
    let out = tempfile::tempdir().unwrap();
    let build = apvm
        .build(
            BuildRequest::new("single", "branch:main", out.path())
                .version(Some("1.0.0".to_string())),
            &NullReporter,
        )
        .await
        .expect("build");
    assert!(build.from_cache(), "build after warm must hit the cache");
    assert_eq!(origin_of(&build, None), ArtifactOrigin::Cache);
    let delivered = out.path().join("myplugin.zip");
    assert!(delivered.is_file());
    assert_eq!(
        std::fs::read(&warmed.path).unwrap(),
        std::fs::read(&delivered).unwrap(),
        "cached bytes must match what the warm produced"
    );
}

/// Warming a ref that is already fully cached reuses everything — nothing is
/// rebuilt.
#[tokio::test]
async fn warm_cache_on_cached_commit_reuses_everything() {
    let repo = init_repo();
    let cache = tempfile::tempdir().unwrap();
    let apvm = apvm_for(
        repo.path(),
        &cache.path().join("store"),
        "single",
        Box::new(SingleBuilder),
    );

    // Prime the cache with a normal build.
    let out = tempfile::tempdir().unwrap();
    apvm.build(
        BuildRequest::new("single", "branch:main", out.path()).version(Some("1.0.0".to_string())),
        &NullReporter,
    )
    .await
    .expect("prime build");

    // Warm the same ref: already cached ⇒ everything reused, nothing built.
    let warm = apvm
        .warm_cache(
            WarmRequest::new("single", "branch:main").version(Some("1.0.0".to_string())),
            &NullReporter,
        )
        .await
        .expect("warm");
    assert!(
        warm.from_cache(),
        "an already-cached commit must be fully reused"
    );
    assert_eq!(origin_of(&warm, None), ArtifactOrigin::Cache);
}

/// Warming extra variants builds only the missing ones (mixed provenance), and
/// every warmed artifact lives in the cache store; a later build of the full
/// set is then a full hit.
#[tokio::test]
async fn warm_cache_partial_builds_only_missing_variants() {
    let repo = init_repo();
    let cache = tempfile::tempdir().unwrap();
    let store_dir = cache.path().join("store");
    let apvm = apvm_for(repo.path(), &store_dir, "multi", Box::new(MultiBuilder));

    // Prime only `free` via a build.
    let out = tempfile::tempdir().unwrap();
    apvm.build(
        BuildRequest::new("multi", "branch:main", out.path())
            .version(Some("2.0.0".to_string()))
            .variants(vec!["free".to_string()]),
        &NullReporter,
    )
    .await
    .expect("prime free");

    // Warm free + pro: free reused, pro built — both now cached, nothing
    // delivered to an output directory.
    let warm = apvm
        .warm_cache(
            WarmRequest::new("multi", "branch:main")
                .version(Some("2.0.0".to_string()))
                .variants(vec!["free".to_string(), "pro".to_string()]),
            &NullReporter,
        )
        .await
        .expect("warm");
    assert_eq!(warm.result.artifacts.len(), 2);
    assert_eq!(origin_of(&warm, Some("free")), ArtifactOrigin::Cache);
    assert_eq!(origin_of(&warm, Some("pro")), ArtifactOrigin::Built);
    for artifact in &warm.result.artifacts {
        assert!(
            artifact.path.starts_with(&store_dir),
            "warmed artifact must live in the cache store, got {}",
            artifact.path.display()
        );
        assert!(artifact.path.is_file());
    }

    // A build of both variants is now a full cache hit.
    let out2 = tempfile::tempdir().unwrap();
    let build = apvm
        .build(
            BuildRequest::new("multi", "branch:main", out2.path())
                .version(Some("2.0.0".to_string()))
                .variants(vec!["free".to_string(), "pro".to_string()]),
            &NullReporter,
        )
        .await
        .expect("build both");
    assert!(
        build.from_cache(),
        "both variants cached after warm → full hit"
    );
    assert_eq!(origin_of(&build, Some("free")), ArtifactOrigin::Cache);
    assert_eq!(origin_of(&build, Some("pro")), ArtifactOrigin::Cache);
}

/// Warming when the artifact cache is disabled still runs the whole pipeline
/// and builds, but caches nothing and warns — it must never error (best-effort,
/// matching the rest of the cache contract).
#[tokio::test]
async fn warm_cache_with_caching_disabled_warns_and_builds() {
    use std::sync::{Arc, Mutex};

    let repo = init_repo();
    let cache = tempfile::tempdir().unwrap();

    // Caching disabled at the instance level ⇒ no store is opened.
    let mut apvm =
        Apvm::new_empty(Config::new(cache.path().join("store")).set_cache_enabled(false))
            .expect("Apvm::new_empty");
    assert!(!apvm.cache_active(), "cache must be inactive for this test");
    apvm.register_project(Project {
        name: "single".to_string(),
        repo_url: repo.path().to_string_lossy().to_string(),
        owner: "test".to_string(),
        repo: "fixture".to_string(),
        default_branch: "main".to_string(),
        is_private: false,
        has_releases: false,
        builder: Box::new(SingleBuilder),
    });

    // Capture warning events emitted during the warm.
    let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let reporter = ClosureReporter::new({
        let warnings = Arc::clone(&warnings);
        move |event| {
            if let BuildEvent::Warning(message) = event {
                warnings.lock().unwrap().push(message.clone());
            }
        }
    });

    let warm = apvm
        .warm_cache(
            WarmRequest::new("single", "branch:main").version(Some("1.0.0".to_string())),
            &reporter,
        )
        .await
        .expect("warm must still succeed (best-effort) when caching is disabled");

    // The pipeline still ran and produced the artifact...
    assert_eq!(warm.result.artifacts.len(), 1);
    assert_eq!(origin_of(&warm, None), ArtifactOrigin::Built);

    // ...and the caller was warned that nothing was cached.
    let warnings = warnings.lock().unwrap();
    assert!(
        warnings
            .iter()
            .any(|m| m.contains("disabled or unavailable")),
        "expected a cache-disabled warning, got: {warnings:?}"
    );
}
