//! End-to-end integration exercise of the storage crate.
//!
//! Simulates a realistic build-pipeline lifecycle against a real (temporary)
//! filesystem: multi-project stores, PR/branch/tag builds with commit churn,
//! multi-variant plugins, the releases cache, strict/lenient cache lookups,
//! damage + self-healing, deletion flows, store relocation, and cross-store
//! concurrency. Everything runs inside `tempfile::TempDir`s, so no state
//! survives the test run.

use std::path::{Path, PathBuf};

use apvm_storage::{
    ArtifactStore, BuildMetadata, BuildSource, LookupRequest, LookupResult, MissReason,
    ReleaseMetadata, SourceArtifact, VerifyMode, VersionMatch,
};
use tempfile::TempDir;

/// A fake commit hash: 40 hex chars derived from a repeating seed digit.
fn commit(seed: u8) -> String {
    format!("{:x}", seed % 16).repeat(40)
}

/// Write a fake artifact file and return a `SourceArtifact` for it.
fn artifact(dir: &Path, name: &str, variant: Option<&str>, content: &str) -> SourceArtifact {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    SourceArtifact {
        variant_id: variant.map(str::to_string),
        path,
        target_name: name.to_string(),
    }
}

fn metadata(
    project: &str,
    version: &str,
    source: BuildSource,
    commit: &str,
    branch: &str,
) -> BuildMetadata {
    BuildMetadata::new(
        project.to_string(),
        version.to_string(),
        source,
        commit.to_string(),
        branch.to_string(),
    )
}

/// The full lifecycle a build pipeline would drive, in one scenario:
/// build → store → lookup (hit/miss semantics) → dedupe → damage →
/// self-heal → delete → cleanup.
#[test]
fn full_pipeline_lifecycle() {
    let work = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    let store = ArtifactStore::new(store_dir.path().join("builds-cache"));

    let commit_a = commit(0xa);
    let commit_b = commit(0xb);

    // --- 1. Fresh store: every lookup is a NotCached miss ---
    let request = LookupRequest::new("backwpup", &commit_a).version("5.6.0");
    assert!(matches!(
        store.lookup_build(&request).unwrap(),
        LookupResult::Miss(MissReason::NotCached)
    ));

    // --- 2. PR build of a two-variant plugin ---
    let free = artifact(
        work.path(),
        "backwpup-free-5.6.0.zip",
        Some("free"),
        "free!",
    );
    let pro = artifact(work.path(), "backwpup-pro-5.6.0.zip", Some("pro"), "pro!!");
    let meta_pr = metadata(
        "backwpup",
        "5.6.0",
        BuildSource::PullRequest(123),
        &commit_a,
        "feature/next-thing",
    );
    let result = store.store(&[free, pro], &meta_pr).unwrap();
    assert_eq!(result.stored_files.len(), 2);
    assert!(!result.was_deduplicated);

    // --- 3. Lookup by short AND full hash, requiring both variants ---
    let both = [Some("free".to_string()), Some("pro".to_string())];
    for query in [&commit_a[..7], commit_a.as_str()] {
        let request = LookupRequest::new("backwpup", query)
            .version("5.6.0")
            .require_variants(&both);
        let hit = match store.lookup_build(&request).unwrap() {
            LookupResult::Hit(hit) => hit,
            LookupResult::Miss(reason) => panic!("expected hit for '{query}', got miss: {reason}"),
        };
        assert!(hit.version_matched);
        assert_eq!(hit.build.files.len(), 2);
        assert!(hit.build.files.iter().all(|f| f.exists()));
        // Full checksum audit passes right after storing.
        assert!(hit.build.verify(VerifyMode::Checksum).unwrap().is_empty());
    }

    // --- 4. Same commit rebuilt from a branch: dedupe + second source ---
    let free2 = artifact(
        work.path(),
        "backwpup-free-5.6.0.zip",
        Some("free"),
        "free!",
    );
    let pro2 = artifact(work.path(), "backwpup-pro-5.6.0.zip", Some("pro"), "pro!!");
    let meta_branch = metadata(
        "backwpup",
        "5.6.0",
        BuildSource::Branch("develop".into()),
        &commit_a,
        "develop",
    );
    let result = store.store(&[free2, pro2], &meta_branch).unwrap();
    assert!(result.was_deduplicated);
    assert!(result.stored_files.is_empty());
    assert_eq!(result.manifest.sources.len(), 2, "PR + branch sources");

    // Both sources now find the same physical build.
    let by_pr = store
        .find_by_source("backwpup", "5.6.0", &BuildSource::PullRequest(123))
        .unwrap();
    let by_branch = store
        .find_by_source("backwpup", "5.6.0", &BuildSource::Branch("develop".into()))
        .unwrap();
    assert_eq!(by_pr.len(), 1);
    assert_eq!(by_branch.len(), 1);
    assert_eq!(by_pr[0].commit_dir, by_branch[0].commit_dir);

    // --- 5. Version-aware plugin semantics (the BackWPup case) ---
    // Same commit requested as 5.7.0. Strict: miss with the exact reason.
    let strict = LookupRequest::new("backwpup", &commit_a)
        .version("5.7.0")
        .version_match(VersionMatch::Strict);
    match store.lookup_build(&strict).unwrap() {
        LookupResult::Miss(MissReason::VersionMismatch { available }) => {
            assert_eq!(available, vec!["5.6.0".to_string()]);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
    // Lenient: hit, flagged as a different version.
    let lenient = LookupRequest::new("backwpup", &commit_a)
        .version("5.7.0")
        .version_match(VersionMatch::Lenient);
    match store.lookup_build(&lenient).unwrap() {
        LookupResult::Hit(hit) => {
            assert!(!hit.version_matched);
            assert_eq!(hit.build.manifest.version, "5.6.0");
        }
        other => panic!("expected lenient hit, got {other:?}"),
    }

    // --- 6. Branch moves to commit B; the branch's latest follows it ---
    let free3 = artifact(work.path(), "backwpup-free-5.6.0.zip", Some("free"), "v2!");
    let meta_b = metadata(
        "backwpup",
        "5.6.0",
        BuildSource::Branch("develop".into()),
        &commit_b,
        "develop",
    );
    store.store(&[free3], &meta_b).unwrap();
    let latest = store
        .find_latest_by_source("backwpup", "5.6.0", &BuildSource::Branch("develop".into()))
        .unwrap()
        .unwrap();
    assert_eq!(latest.manifest.commit, commit_b);

    // --- 7. Damage a stored file: lookup reports Incomplete, store heals ---
    let commit_a_short = &commit_a[..7];
    let commit_dir = store
        .paths()
        .commit_dir("backwpup", "5.6.0", commit_a_short);
    std::fs::remove_file(commit_dir.join("backwpup-pro-5.6.0.zip")).unwrap();

    let request = LookupRequest::new("backwpup", &commit_a)
        .version("5.6.0")
        .require_variants(&both);
    assert!(matches!(
        store.lookup_build(&request).unwrap(),
        LookupResult::Miss(MissReason::Incomplete)
    ));
    // The healthy variant still reports; the damaged one does not.
    let variants = store
        .get_existing_variants("backwpup", "5.6.0", commit_a_short)
        .unwrap();
    assert!(variants.contains(&Some("free".to_string())));
    assert!(!variants.contains(&Some("pro".to_string())));

    // Re-store (as a pipeline would after rebuilding): only the damaged
    // file is copied again.
    let free4 = artifact(
        work.path(),
        "backwpup-free-5.6.0.zip",
        Some("free"),
        "free!",
    );
    let pro4 = artifact(work.path(), "backwpup-pro-5.6.0.zip", Some("pro"), "pro!!");
    let result = store.store(&[free4, pro4], &meta_pr).unwrap();
    assert_eq!(
        result.stored_files.len(),
        1,
        "only the damaged file re-copied"
    );
    assert!(store.lookup_build(&request).unwrap().is_hit());

    // --- 8. Inventory queries see exactly the physical builds ---
    assert_eq!(store.query().project("backwpup").count().unwrap(), 2);
    assert_eq!(
        store
            .query()
            .project("backwpup")
            .source(&BuildSource::PullRequest(123))
            .count()
            .unwrap(),
        1
    );

    // --- 9. Delete the PR source; commit A survives via the branch source ---
    let deletion = store
        .delete_source(
            "backwpup",
            "5.6.0",
            &BuildSource::PullRequest(123),
            true, // delete orphans
        )
        .unwrap();
    assert_eq!(deletion.commits_affected, vec![commit_a_short.to_string()]);
    assert!(
        deletion.orphan_commits_deleted.is_empty(),
        "branch still refs it"
    );
    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", &commit_a)
            .unwrap()
            .is_some()
    );

    // --- 10. Delete everything; the tree is swept clean ---
    assert!(
        store
            .delete_by_commit("backwpup", "5.6.0", &commit_a)
            .unwrap()
    );
    assert!(
        store
            .delete_by_commit("backwpup", "5.6.0", &commit_b)
            .unwrap()
    );
    assert!(store.list_projects().unwrap().is_empty());
}

/// The releases cache lifecycle: store, hit, dedupe, damage, heal, delete.
#[test]
fn releases_cache_lifecycle() {
    let work = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    let store = ArtifactStore::new(store_dir.path().join("builds-cache"));

    // Miss before caching.
    assert!(
        store
            .find_release("wp-rocket", "v3.18.0")
            .unwrap()
            .is_none()
    );

    // Cache a downloaded release (as the pipeline would after the GitHub
    // API resolved `release:latest-stable` → v3.18.0).
    let zip = artifact(work.path(), "wp-rocket_3.18.0.zip", None, "release bytes");
    let mut meta = ReleaseMetadata::new(
        "wp-rocket".to_string(),
        "v3.18.0".to_string(),
        "3.18.0".to_string(),
    );
    meta.prerelease = false;
    let result = store.store_release(&[zip], &meta).unwrap();
    assert_eq!(result.stored_files.len(), 1);

    // Hit, with intact content.
    let release = store.find_release("wp-rocket", "v3.18.0").unwrap().unwrap();
    assert_eq!(release.manifest.version, "3.18.0");
    let cached = release.file_by_name("wp-rocket_3.18.0.zip").unwrap();
    assert_eq!(std::fs::read_to_string(cached).unwrap(), "release bytes");

    // Re-cache is a no-op (dedupe).
    let zip2 = artifact(work.path(), "wp-rocket_3.18.0.zip", None, "release bytes");
    let result = store.store_release(&[zip2], &meta).unwrap();
    assert!(result.was_deduplicated);

    // Damage → miss → re-store heals.
    std::fs::remove_file(result.release_dir.join("wp-rocket_3.18.0.zip")).unwrap();
    assert!(
        store
            .find_release("wp-rocket", "v3.18.0")
            .unwrap()
            .is_none()
    );
    let zip3 = artifact(work.path(), "wp-rocket_3.18.0.zip", None, "release bytes");
    store.store_release(&[zip3], &meta).unwrap();
    assert!(
        store
            .find_release("wp-rocket", "v3.18.0")
            .unwrap()
            .is_some()
    );

    // Releases and builds coexist without polluting each other's listings.
    let commit_c = commit(0xc);
    let build = artifact(work.path(), "wp-rocket_3.18.0-dev.zip", None, "dev build");
    let build_meta = metadata(
        "wp-rocket",
        "3.18.0",
        BuildSource::Tag("v3.18.0".into()),
        &commit_c,
        "main",
    );
    store.store(&[build], &build_meta).unwrap();
    assert_eq!(store.list_versions("wp-rocket").unwrap(), vec!["3.18.0"]);
    assert_eq!(store.list_releases("wp-rocket").unwrap().len(), 1);
    assert_eq!(store.query().project("wp-rocket").count().unwrap(), 1);

    // Delete the release; the build tree is untouched.
    assert!(store.delete_release("wp-rocket", "v3.18.0").unwrap());
    assert!(
        store
            .find_release("wp-rocket", "v3.18.0")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .find_by_commit("wp-rocket", "3.18.0", &commit_c)
            .unwrap()
            .is_some()
    );
}

/// A store must be relocatable: move the whole base directory and verify
/// everything still resolves (source links are relative on Unix; find paths
/// never traverse links at all).
#[test]
fn store_survives_relocation() {
    let work = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    let old_base = store_dir.path().join("cache-v1");
    let commit_d = commit(0xd);

    {
        let store = ArtifactStore::new(old_base.clone());
        let zip = artifact(work.path(), "plugin.zip", None, "bytes");
        let meta = metadata(
            "backwpup",
            "5.6.0",
            BuildSource::Branch("develop".into()),
            &commit_d,
            "develop",
        );
        store.store(&[zip], &meta).unwrap();
    }

    // Relocate the entire store directory.
    let new_base = store_dir.path().join("cache-v2");
    std::fs::rename(&old_base, &new_base).unwrap();

    let store = ArtifactStore::new(new_base);
    let build = store
        .find_by_commit("backwpup", "5.6.0", &commit_d)
        .unwrap()
        .expect("build must survive relocation");
    assert!(build.files[0].exists());
    assert!(
        !store
            .find_by_source("backwpup", "5.6.0", &BuildSource::Branch("develop".into()))
            .unwrap()
            .is_empty()
    );
    // Full checksum audit after the move.
    assert!(build.verify(VerifyMode::Checksum).unwrap().is_empty());
}

/// Concurrent pipelines (separate `ArtifactStore` instances, as separate
/// processes would have) storing builds and releases at once: the store-wide
/// lock must keep every manifest consistent.
#[test]
fn concurrent_mixed_workload() {
    let store_dir = TempDir::new().unwrap();
    let base: PathBuf = store_dir.path().join("builds-cache");
    let commit_e = commit(0xe);

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let base = base.clone();
            let commit_e = commit_e.clone();
            std::thread::spawn(move || {
                let work = TempDir::new().unwrap();
                let store = ArtifactStore::new(base);
                if i % 2 == 0 {
                    // Even threads: store a distinct variant of one commit.
                    let name = format!("variant-{i}.zip");
                    let zip = artifact(work.path(), &name, Some(&format!("v{i}")), "x");
                    let meta = metadata(
                        "backwpup",
                        "5.6.0",
                        BuildSource::PullRequest(i as u64),
                        &commit_e,
                        "develop",
                    );
                    store.store(&[zip], &meta).unwrap();
                } else {
                    // Odd threads: cache a distinct release.
                    let name = format!("release-{i}.zip");
                    let zip = artifact(work.path(), &name, None, "y");
                    let meta = ReleaseMetadata::new(
                        "backwpup".to_string(),
                        format!("v9.{i}.0"),
                        format!("9.{i}.0"),
                    );
                    store.store_release(&[zip], &meta).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let store = ArtifactStore::new(base);
    // All four variants of the commit survived the concurrent manifest
    // read-modify-write cycles, and all four PR sources are recorded.
    let build = store
        .find_by_commit("backwpup", "5.6.0", &commit_e)
        .unwrap()
        .unwrap();
    assert_eq!(build.manifest.artifacts.len(), 4);
    assert_eq!(build.manifest.sources.len(), 4);
    // All four releases cached.
    assert_eq!(store.list_releases("backwpup").unwrap().len(), 4);
    // Nothing left half-written anywhere.
    assert!(build.verify(VerifyMode::Checksum).unwrap().is_empty());
}

/// Hostile/malformed input must be rejected up front — nothing may escape
/// the store directory or corrupt state.
#[test]
fn hostile_input_is_rejected() {
    let work = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    let store = ArtifactStore::new(store_dir.path().join("builds-cache"));
    let zip = artifact(work.path(), "ok.zip", None, "bytes");
    let commit_f = commit(0xf);

    // Traversal / separator / reserved-name attempts across all fields.
    for bad_project in ["..", "../up", "a/b", "a\\b", "", ".hidden", "CON"] {
        let meta = metadata(
            bad_project,
            "1.0.0",
            BuildSource::PullRequest(1),
            &commit_f,
            "b",
        );
        assert!(
            store.store(std::slice::from_ref(&zip), &meta).is_err(),
            "project {bad_project:?} must be rejected"
        );
    }
    for bad_version in ["..", "1.0/x", "", "releases"] {
        let meta = metadata(
            "proj",
            bad_version,
            BuildSource::PullRequest(1),
            &commit_f,
            "b",
        );
        assert!(
            store.store(std::slice::from_ref(&zip), &meta).is_err(),
            "version {bad_version:?} must be rejected"
        );
    }
    for bad_commit in ["", "short", "not-hexadecimal-at-all!", &"f".repeat(65)] {
        let meta = metadata(
            "proj",
            "1.0.0",
            BuildSource::PullRequest(1),
            bad_commit,
            "b",
        );
        assert!(
            store.store(std::slice::from_ref(&zip), &meta).is_err(),
            "commit {bad_commit:?} must be rejected"
        );
    }
    for bad_name in [
        "../../up.zip",
        "build-manifest.json",
        "a/b.zip",
        "",
        ".apvm-tmp-x",
    ] {
        let evil = SourceArtifact {
            variant_id: None,
            path: zip.path.clone(),
            target_name: bad_name.to_string(),
        };
        let meta = metadata("proj", "1.0.0", BuildSource::PullRequest(1), &commit_f, "b");
        assert!(
            store.store(&[evil], &meta).is_err(),
            "target name {bad_name:?} must be rejected"
        );
    }

    // After all those rejections the store contains nothing.
    assert!(store.list_projects().unwrap().is_empty());

    // Sources with hostile names are *sanitized*, not rejected — they must
    // stay inside the store.
    let meta = metadata(
        "proj",
        "1.0.0",
        BuildSource::Branch("../../../etc/passwd".into()),
        &commit_f,
        "b",
    );
    store.store(&[zip], &meta).unwrap();
    let sources = store.list_sources("proj", "1.0.0").unwrap();
    assert_eq!(
        sources,
        vec![BuildSource::Branch("../../../etc/passwd".into())],
        "exact hostile source recovered from manifest, stored safely"
    );
    // And the link directory really is inside the store.
    let source_dir = store.paths().source_dir("proj", "1.0.0", &sources[0]);
    assert!(source_dir.starts_with(store.base_dir()));
    assert!(source_dir.exists());
}
