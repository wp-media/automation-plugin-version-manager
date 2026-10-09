//! End-to-end tests of the public `Apvm` build entry points against a local
//! git repository — real `git ls-remote`, clone, checkout and builder commands,
//! with no network access.
//!
//! They lock the contract consumers (CLI, Node bindings) rely on:
//!
//! - the `build_from_branch` / `build_from_tag` conveniences build exactly the
//!   requested ref (proven by the artifact's bytes, which come from the
//!   checked-out source) and label it correctly;
//! - tag keywords (`tag:latest-stable`, `tag:latest`) and prefix-less refs are
//!   resolved against the remote before cloning;
//! - invalid input (unknown project, bad prefix, malformed PR number, missing
//!   ref, unsupported platform) fails fast — before any clone — with an
//!   actionable error;
//! - a failing build surfaces its error and caches nothing;
//! - the progress timeline is emitted in a stable order.
//!
//! Only refs that resolve without the GitHub API are used (branches, tags):
//! bare digits and commit SHAs go through the GitHub API and would make the
//! suite network-dependent.
//!
//! Requires `git` and `sh` in `PATH`, so the file is Unix-gated.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use apvm_core::build::plugins::{BuildArtifact, Builder, VersionRequirement};
use apvm_core::build::progress::{BuildPhase, BuildStep};
use apvm_core::projects::Project;
use apvm_core::{
    Apvm, BuildContext, BuildEvent, BuildOutput, BuildRequest, CacheMaintenance, Config, Error,
    NullReporter, ProgressReporter, RefSource,
};

// ─────────────────────────────────────────────────────────────────────────────
// Test builders
// ─────────────────────────────────────────────────────────────────────────────

/// Packages the checked-out `f` file as the artifact, so the delivered bytes
/// reveal which commit was actually built.
struct SourceBuilder;

impl Builder for SourceBuilder {
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Required
    }
    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![]
    }
    fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
        vec![BuildStep::new("package", "cp f plugin.zip")]
    }
    fn artifacts(
        &self,
        _: &BuildContext,
        _: &str,
        _: &[&str],
    ) -> apvm_core::Result<Vec<BuildArtifact>> {
        Ok(vec![BuildArtifact {
            variant_id: None,
            source_path: "plugin.zip".to_string(),
            target_name: "plugin.zip".to_string(),
        }])
    }
}

/// A builder whose only build command fails.
struct FailingBuilder;

impl Builder for FailingBuilder {
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Required
    }
    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![]
    }
    fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
        vec![BuildStep::new("explode", "echo kaboom >&2; exit 9")]
    }
    fn artifacts(
        &self,
        _: &BuildContext,
        _: &str,
        _: &[&str],
    ) -> apvm_core::Result<Vec<BuildArtifact>> {
        Ok(vec![])
    }
}

/// A builder that refuses the current platform (as Imagify does on Windows).
struct UnsupportedBuilder;

impl Builder for UnsupportedBuilder {
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Required
    }
    fn ensure_platform_supported(&self) -> apvm_core::Result<()> {
        Err(Error::PlatformUnsupported {
            project: "unsupported".to_string(),
            platform: std::env::consts::OS.to_string(),
            reason: "needs a toolchain this host lacks".to_string(),
        })
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
    ) -> apvm_core::Result<Vec<BuildArtifact>> {
        Ok(vec![])
    }
}

/// Reporter that keeps every event, for timeline assertions.
#[derive(Default)]
struct Recorder(Mutex<Vec<BuildEvent>>);

impl ProgressReporter for Recorder {
    fn report(&self, event: &BuildEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Recorder {
    /// Snapshot of every event received so far.
    fn events(&self) -> Vec<BuildEvent> {
        self.0.lock().unwrap().clone()
    }

    /// Whether `phase` was ever started — e.g. to prove nothing was cloned.
    fn started(&self, phase: BuildPhase) -> bool {
        self.events()
            .iter()
            .any(|e| matches!(e, BuildEvent::PhaseStarted { phase: p, .. } if *p == phase))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// Run git in `dir` with a fixed identity and `date`, so tag creation dates —
/// which order the tag keywords — are deterministic.
fn git(dir: &Path, args: &[&str], date: &str) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=test@apvm.dev",
            "-c",
            "user.name=apvm-test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_DATE", date)
        .current_dir(dir)
        .output()
        .expect("failed to spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit `f` = `content` at `date`.
fn commit(dir: &Path, content: &str, date: &str) {
    std::fs::write(dir.join("f"), content).unwrap();
    git(dir, &["add", "f"], date);
    git(dir, &["commit", "-qm", content], date);
}

/// A repository whose `f` changes with every commit:
///
/// - `one`   (2024-01) — annotated tag `v1.0.0` (stable)
/// - `two`   (2024-06) — annotated tag `v1.1.0-beta1` (prerelease, newest tag)
/// - `three` (2025-01) — `main` HEAD, untagged
fn tagged_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(
        dir,
        &["init", "--quiet", "--initial-branch=main"],
        "2024-01-01T10:00:00",
    );
    commit(dir, "one", "2024-01-01T10:00:00");
    git(
        dir,
        &["tag", "-a", "v1.0.0", "-m", "stable"],
        "2024-01-02T10:00:00",
    );
    commit(dir, "two", "2024-06-01T10:00:00");
    git(
        dir,
        &["tag", "-a", "v1.1.0-beta1", "-m", "beta"],
        "2024-06-02T10:00:00",
    );
    commit(dir, "three", "2025-01-01T10:00:00");
    repo
}

/// The full commit SHA `reference` points at (tags peeled).
fn sha_of(repo: &Path, reference: &str) -> String {
    git(
        repo,
        &["rev-parse", &format!("{reference}^{{commit}}")],
        "2025-01-01T10:00:00",
    )
}

/// An `Apvm` with one project `name` backed by `repo`, caching into `cache`.
fn apvm_for(repo: &Path, cache: &Path, name: &str, builder: Box<dyn Builder>) -> Apvm {
    let mut apvm = Apvm::new_empty(Config::new(cache.to_path_buf())).unwrap();
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

/// The single delivered artifact's contents.
fn delivered(output: &BuildOutput) -> String {
    assert_eq!(output.result.artifacts.len(), 1, "{:?}", output.result);
    std::fs::read_to_string(&output.result.artifacts[0].path).unwrap()
}

/// Number of builds recorded in the cache at `cache`.
fn cached_builds(cache: &Path) -> u64 {
    CacheMaintenance::new(cache)
        .usage()
        .unwrap()
        .map_or(0, |usage| usage.build_count)
}

// ─────────────────────────────────────────────────────────────────────────────
// Convenience entry points
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn build_from_branch_builds_the_branch_head() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));

    let output = apvm
        .build_from_branch(
            "src",
            Some("1.0.0"),
            "main",
            None,
            out.path(),
            &NullReporter,
        )
        .await
        .unwrap();

    assert_eq!(delivered(&output), "three");
    assert_eq!(
        output.result.artifacts[0].path,
        out.path().join("plugin.zip")
    );
    assert_eq!(output.commit, sha_of(repo.path(), "main"));
    assert_eq!(output.commit_short, output.commit[..7]);
    assert_eq!(output.branch, "main");
    assert_eq!(*output.source(), RefSource::Branch("main".to_string()));
    assert_eq!(output.result.version, "1.0.0");
}

#[tokio::test]
async fn build_from_tag_builds_the_tagged_commit_not_the_branch_head() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));

    let output = apvm
        .build_from_tag(
            "src",
            Some("1.0.0"),
            "v1.0.0",
            None,
            out.path(),
            &NullReporter,
        )
        .await
        .unwrap();

    assert_eq!(delivered(&output), "one");
    // The annotated tag is peeled to its commit, never the tag object.
    assert_eq!(output.commit, sha_of(repo.path(), "v1.0.0"));
    assert_eq!(output.branch, "tag/v1.0.0");
    assert_eq!(*output.source(), RefSource::Tag("v1.0.0".to_string()));
}

#[tokio::test]
async fn tag_keywords_pick_the_newest_stable_or_any_tag() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));
    let build = |git_ref: &'static str| {
        let out = tempfile::tempdir().unwrap();
        let request = BuildRequest::new("src", git_ref, out.path())
            .version(Some("1.0.0".to_string()))
            .no_cache(true);
        let apvm = &apvm;
        async move { (apvm.build(request, &NullReporter).await.unwrap(), out) }
    };

    let (stable, _out1) = build("tag:latest-stable").await;
    let (latest, _out2) = build("tag:latest").await;

    // The prerelease is the newest tag, so only `latest` may pick it.
    assert_eq!(delivered(&stable), "one");
    assert_eq!(*stable.source(), RefSource::Tag("v1.0.0".to_string()));
    assert_eq!(delivered(&latest), "two");
    assert_eq!(*latest.source(), RefSource::Tag("v1.1.0-beta1".to_string()));
}

#[tokio::test]
async fn prefix_less_refs_are_detected_as_tag_or_branch() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));
    let request = |git_ref: &str, out: &Path| {
        BuildRequest::new("src", git_ref, out)
            .version(Some("1.0.0".to_string()))
            .no_cache(true)
    };

    let out_tag = tempfile::tempdir().unwrap();
    let tag = apvm
        .build(request("v1.0.0", out_tag.path()), &NullReporter)
        .await
        .unwrap();
    let out_branch = tempfile::tempdir().unwrap();
    let branch = apvm
        .build(request("main", out_branch.path()), &NullReporter)
        .await
        .unwrap();

    assert_eq!(*tag.source(), RefSource::Tag("v1.0.0".to_string()));
    assert_eq!(delivered(&tag), "one");
    assert_eq!(*branch.source(), RefSource::Branch("main".to_string()));
    assert_eq!(delivered(&branch), "three");
}

// ─────────────────────────────────────────────────────────────────────────────
// Fail-fast validation (nothing is cloned)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_project_is_rejected() {
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = Apvm::new_empty(Config::new(cache.path().to_path_buf())).unwrap();

    let result = apvm
        .build_from_branch("ghost", None, "main", None, out.path(), &NullReporter)
        .await;

    assert!(
        matches!(&result, Err(Error::ProjectNotFound(name)) if name == "ghost"),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_unsupported_platform_fails_before_any_work() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(
        repo.path(),
        cache.path(),
        "unsupported",
        Box::new(UnsupportedBuilder),
    );
    let recorder = Recorder::default();

    let result = apvm
        .build_from_branch(
            "unsupported",
            Some("1.0.0"),
            "main",
            None,
            out.path(),
            &recorder,
        )
        .await;

    assert!(
        matches!(result, Err(Error::PlatformUnsupported { .. })),
        "{result:?}"
    );
    assert!(recorder.events().is_empty(), "{:?}", recorder.events());
}

#[tokio::test]
async fn a_malformed_pr_number_is_rejected_without_network() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));
    let recorder = Recorder::default();

    let request = BuildRequest::new("src", "pr:abc", out.path()).version(Some("1.0.0".into()));
    let result = apvm.build(request, &recorder).await;

    assert!(
        matches!(&result, Err(Error::Git(m)) if m == "Invalid PR number: 'abc'"),
        "{result:?}"
    );
    assert!(!recorder.started(BuildPhase::Clone));
}

#[tokio::test]
async fn an_unknown_prefix_is_rejected_with_the_valid_ones() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));

    let request = BuildRequest::new("src", "foo:bar", out.path()).version(Some("1.0.0".into()));
    let result = apvm.build(request, &NullReporter).await;

    let Err(Error::Git(message)) = result else {
        panic!("expected a git error, got {result:?}");
    };
    assert!(
        message.starts_with("Invalid reference 'foo:bar'"),
        "{message}"
    );
    assert!(
        message.contains("pr:, tag:, branch:, commit:, release:"),
        "{message}"
    );
}

#[tokio::test]
async fn a_missing_branch_fails_before_cloning() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));
    let recorder = Recorder::default();

    let result = apvm
        .build_from_branch(
            "src",
            Some("1.0.0"),
            "no-such-branch",
            None,
            out.path(),
            &recorder,
        )
        .await;

    let Err(Error::Git(message)) = result else {
        panic!("expected a git error, got {result:?}");
    };
    assert!(message.contains("no-such-branch"), "{message}");
    assert!(recorder.started(BuildPhase::Preflight));
    assert!(
        !recorder.started(BuildPhase::Clone),
        "nothing may be cloned"
    );
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Failure handling and progress timeline
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_failing_build_reports_the_command_error_and_caches_nothing() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "bad", Box::new(FailingBuilder));

    let result = apvm
        .build_from_branch(
            "bad",
            Some("1.0.0"),
            "main",
            None,
            out.path(),
            &NullReporter,
        )
        .await;

    let Err(Error::Build(message)) = result else {
        panic!("expected a build error, got {result:?}");
    };
    assert!(message.contains("exit code Some(9)"), "{message}");
    assert!(message.contains("kaboom"), "{message}");
    assert_eq!(
        cached_builds(cache.path()),
        0,
        "a failed build is not cached"
    );
}

#[tokio::test]
async fn a_clone_build_emits_its_timeline_in_order() {
    let repo = tagged_repo();
    let cache = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let apvm = apvm_for(repo.path(), cache.path(), "src", Box::new(SourceBuilder));
    let recorder = Recorder::default();

    apvm.build_from_branch("src", Some("1.0.0"), "main", None, out.path(), &recorder)
        .await
        .unwrap();

    let events = recorder.events();
    let position = |pred: &dyn Fn(&BuildEvent) -> bool| {
        events
            .iter()
            .position(pred)
            .unwrap_or_else(|| panic!("event missing from {events:?}"))
    };
    let phase = |wanted: BuildPhase| {
        position(
            &move |e: &BuildEvent| matches!(e, BuildEvent::PhaseStarted { phase, .. } if *phase == wanted),
        )
    };
    let resolved = position(&|e| matches!(e, BuildEvent::ReferenceResolved { .. }));

    // Consumers show the resolved ref before the (slow) clone starts.
    assert!(phase(BuildPhase::Preflight) < resolved);
    assert!(resolved < phase(BuildPhase::Clone));
    assert!(phase(BuildPhase::Clone) < phase(BuildPhase::Checkout));
    assert!(phase(BuildPhase::Checkout) < phase(BuildPhase::Build));
    assert!(phase(BuildPhase::Build) < phase(BuildPhase::CollectArtifacts));
    assert!(
        matches!(events.last(), Some(BuildEvent::BuildSucceeded { .. })),
        "the success event closes the timeline: {events:?}"
    );
    assert_eq!(cached_builds(cache.path()), 1, "the build warmed the cache");
}
