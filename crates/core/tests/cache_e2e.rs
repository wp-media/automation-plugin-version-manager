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
//! Requires `git` and `sh` in `PATH` (the same assumption the in-crate git
//! tests make), so the file is Unix-gated.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use apvm_core::build::BuildContext;
use apvm_core::build::plugins::{BuildArtifact, BuildVariant, Builder, VersionRequirement};
use apvm_core::build::progress::BuildStep;
use apvm_core::projects::Project;
use apvm_core::{Apvm, ArtifactOrigin, BuildOutput, BuildRequest, Config, NullReporter};

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
