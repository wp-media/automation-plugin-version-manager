//! End-to-end tests exercising the store through its public API against
//! real temporary directories — the closest thing to "how it is meant to
//! be used".

use std::path::{Path, PathBuf};

use apvm_storage::{
    ArtifactStore, BuildMetadata, BuildSource, CleanOptions, CleanTarget, Error, LookupKey,
    LookupRequest, LookupResult, MissReason, ReleaseMetadata, SourceArtifact, VerifyMode,
    VerifyProblem, VersionMatch,
};
use chrono::{Duration, Utc};

// ============================================================================
// Helpers
// ============================================================================

/// A store in a fresh temp directory, plus a scratch dir for source files.
fn make_store() -> (tempfile::TempDir, ArtifactStore, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let scratch = root.path().join("scratch-sources");
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let store = ArtifactStore::open(root.path().join("store")).expect("open store");
    (root, store, scratch)
}

/// Write a source file and describe it as an artifact.
fn artifact(scratch: &Path, name: &str, content: &[u8], variant: Option<&str>) -> SourceArtifact {
    let path = scratch.join(name);
    std::fs::write(&path, content).expect("write source file");
    SourceArtifact {
        variant_id: variant.map(str::to_string),
        path,
        target_name: name.to_string(),
    }
}

const COMMIT_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

fn metadata(project: &str, version: &str, commit: &str) -> BuildMetadata {
    BuildMetadata::new(
        project,
        version,
        BuildSource::Branch("develop".to_string()),
        commit,
        "develop".to_string(),
    )
}

// ============================================================================
// Store / find round trips
// ============================================================================

#[test]
fn store_and_find_round_trip_with_variants() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![
        artifact(
            &scratch,
            "backwpup-free-5.6.0.zip",
            b"free-bytes",
            Some("free"),
        ),
        artifact(
            &scratch,
            "backwpup-pro-en-5.6.0.zip",
            b"pro-en-bytes!",
            Some("pro-en"),
        ),
    ];

    let result = store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();
    assert_eq!(result.newly_stored.len(), 2);
    assert!(result.reused.is_empty());
    assert_eq!(result.build.commit, COMMIT_A);
    assert_eq!(result.build.sources.len(), 1);

    // Find by short, mixed-case, and full commit.
    for commit in ["a1b2c3d", "A1B2C3D", COMMIT_A] {
        let found = store
            .find_by_commit("backwpup", "5.6.0", commit)
            .unwrap()
            .unwrap_or_else(|| panic!("expected hit for {commit}"));
        assert_eq!(found.artifacts.len(), 2);
        for stored in &found.artifacts {
            assert_eq!(
                std::fs::metadata(&stored.path).unwrap().len(),
                stored.size_bytes,
                "artifact file must exist with recorded size"
            );
            assert_eq!(stored.sha256.len(), 64);
        }
    }

    // Variant bookkeeping matches what was stored.
    let variants = store
        .get_existing_variants("backwpup", "5.6.0", "a1b2c3d")
        .unwrap();
    assert_eq!(
        variants,
        vec![Some("free".to_string()), Some("pro-en".to_string())]
    );
    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", "deadbee")
            .unwrap()
            .is_none()
    );
}

#[test]
fn restore_reuses_healthy_files_and_heals_damaged_ones() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(
        &scratch,
        "wp-rocket-3.18.1.zip",
        b"rocket-bytes",
        None,
    )];
    let meta = metadata("wp-rocket", "3.18.1", COMMIT_A);

    let first = store.store(&meta, &artifacts).unwrap();
    assert_eq!(first.newly_stored, vec!["wp-rocket-3.18.1.zip".to_string()]);

    // Second store: nothing to copy.
    let second = store.store(&meta, &artifacts).unwrap();
    assert!(second.newly_stored.is_empty());
    assert_eq!(second.reused, vec!["wp-rocket-3.18.1.zip".to_string()]);

    // Damage the stored file (size change) → find misses, re-store heals.
    let stored_path = &second.build.artifacts[0].path;
    std::fs::write(stored_path, b"truncated").unwrap();
    assert!(
        store
            .find_by_commit("wp-rocket", "3.18.1", "a1b2c3d")
            .unwrap()
            .is_none()
    );

    let healed = store.store(&meta, &artifacts).unwrap();
    assert_eq!(
        healed.newly_stored,
        vec!["wp-rocket-3.18.1.zip".to_string()]
    );
    assert!(
        store
            .find_by_commit("wp-rocket", "3.18.1", "a1b2c3d")
            .unwrap()
            .is_some()
    );
}

#[test]
fn storing_full_hash_upgrades_a_short_hash_record() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", b"bytes", None)];

    store
        .store(&metadata("backwpup", "5.6.0", "a1b2c3d"), &artifacts)
        .unwrap();
    store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();

    // Same build, not a duplicate; the recorded hash is now the full one.
    let builds = store.list_builds(Some("backwpup"), None).unwrap();
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].commit, COMMIT_A);
    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
            .unwrap()
            .is_some()
    );
}

#[test]
fn short_prefix_collisions_get_distinct_directories() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", b"bytes", None)];
    let commit_1 = format!("aaaaaaa{}", "1".repeat(33));
    let commit_2 = format!("aaaaaaa{}", "2".repeat(33));

    store
        .store(&metadata("backwpup", "5.6.0", &commit_1), &artifacts)
        .unwrap();
    store
        .store(&metadata("backwpup", "5.6.0", &commit_2), &artifacts)
        .unwrap();

    let build_1 = store
        .find_by_commit("backwpup", "5.6.0", &commit_1)
        .unwrap()
        .unwrap();
    let build_2 = store
        .find_by_commit("backwpup", "5.6.0", &commit_2)
        .unwrap()
        .unwrap();
    assert_eq!(build_1.commit, commit_1);
    assert_eq!(build_2.commit, commit_2);
    assert_ne!(
        build_1.dir, build_2.dir,
        "colliding prefixes must not share a directory"
    );
}

#[test]
fn sources_link_and_relink_correctly() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", b"bytes", None)];
    let branch = BuildSource::Branch("develop".to_string());
    let commit_old = format!("0000000{}", "a".repeat(33));
    let commit_new = format!("1111111{}", "b".repeat(33));

    // The branch built twice at different commits; a PR points at the newer.
    store
        .store(&metadata("backwpup", "5.6.0", &commit_old), &artifacts)
        .unwrap();
    store
        .store(&metadata("backwpup", "5.6.0", &commit_new), &artifacts)
        .unwrap();
    let mut pr_meta = metadata("backwpup", "5.6.0", &commit_new);
    pr_meta.source = BuildSource::PullRequest(123);
    store.store(&pr_meta, &artifacts).unwrap();

    let by_branch = store.find_by_source("backwpup", &branch).unwrap();
    assert_eq!(by_branch.len(), 2);
    assert_eq!(
        by_branch[0].commit, commit_new,
        "most recently linked first"
    );

    let latest = store
        .find_latest_by_source("backwpup", &branch)
        .unwrap()
        .unwrap();
    assert_eq!(latest.commit, commit_new);
    assert_eq!(
        latest.sources.len(),
        2,
        "branch + PR links on the same build"
    );

    let by_pr = store
        .find_by_source("backwpup", &BuildSource::PullRequest(123))
        .unwrap();
    assert_eq!(by_pr.len(), 1);
    assert!(
        store
            .find_by_source("backwpup", &BuildSource::PullRequest(999))
            .unwrap()
            .is_empty()
    );
}

// ============================================================================
// Lookup semantics (the BackWPup strict-version case)
// ============================================================================

#[test]
fn lookup_strict_and_lenient_version_matching() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![
        artifact(&scratch, "free.zip", b"free", Some("free")),
        artifact(&scratch, "pro-en.zip", b"pro-en", Some("pro-en")),
    ];
    store
        .store(&metadata("backwpup", "5.5.9", COMMIT_A), &artifacts)
        .unwrap();

    // Strict + wrong version → VersionMismatch listing what exists.
    let strict = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.6.0")
        .version_match(VersionMatch::Strict);
    match store.lookup_build(&strict).unwrap() {
        LookupResult::Miss(MissReason::VersionMismatch { available }) => {
            assert_eq!(available, vec!["5.5.9".to_string()]);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }

    // Lenient + wrong version → hit, flagged as version-mismatched.
    let lenient = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d")).version("5.6.0");
    let hit = store
        .lookup_build(&lenient)
        .unwrap()
        .hit()
        .expect("lenient hit");
    assert!(!hit.version_matched);
    assert_eq!(hit.build.version, "5.5.9");

    // Strict + right version → clean hit.
    let exact = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.5.9")
        .version_match(VersionMatch::Strict);
    let hit = store
        .lookup_build(&exact)
        .unwrap()
        .hit()
        .expect("strict hit");
    assert!(hit.version_matched);

    // Source keys work the same way.
    let branch = BuildSource::Branch("develop".to_string());
    let by_source = LookupRequest::new("backwpup", LookupKey::Source(&branch));
    assert!(store.lookup_build(&by_source).unwrap().is_hit());
}

#[test]
fn lookup_reports_missing_variants_not_cached_and_incomplete() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "free.zip", b"free", Some("free"))];
    store
        .store(&metadata("backwpup", "5.5.9", COMMIT_A), &artifacts)
        .unwrap();

    // Unknown commit → NotCached.
    let unknown = LookupRequest::new("backwpup", LookupKey::Commit("deadbee"));
    assert!(matches!(
        store.lookup_build(&unknown).unwrap(),
        LookupResult::Miss(MissReason::NotCached)
    ));

    // Required variant absent → MissingVariants names exactly what to build.
    let required = vec![Some("free".to_string()), Some("pro-de".to_string())];
    let request =
        LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d")).require_variants(&required);
    match store.lookup_build(&request).unwrap() {
        LookupResult::Miss(MissReason::MissingVariants { missing }) => {
            assert_eq!(missing, vec![Some("pro-de".to_string())]);
        }
        other => panic!("expected MissingVariants, got {other:?}"),
    }

    // Damaged files → Incomplete.
    let build = store
        .find_commit_in_versions("backwpup", "a1b2c3d")
        .unwrap()
        .unwrap();
    std::fs::remove_file(&build.artifacts[0].path).unwrap();
    let request = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"));
    assert!(matches!(
        store.lookup_build(&request).unwrap(),
        LookupResult::Miss(MissReason::Incomplete)
    ));
}

// ============================================================================
// Releases
// ============================================================================

#[test]
fn release_cache_lifecycle_with_awkward_tag() {
    let (_root, store, scratch) = make_store();
    let tag = "release/5.3 β"; // slash + space + non-ASCII: unsafe as a dir name
    let assets = vec![artifact(
        &scratch,
        "backwpup-pro-5.3.zip",
        b"release-bytes",
        None,
    )];

    let mut meta = ReleaseMetadata::new("backwpup", tag);
    meta.version = Some("5.3.0".to_string());
    meta.draft = true;
    let first = store.store_release(&meta, &assets).unwrap();
    assert_eq!(first.newly_stored.len(), 1);
    assert!(first.release.draft);
    assert!(first.release.dir.exists());

    // Exact-tag lookup, health-checked.
    let found = store
        .find_release("backwpup", tag)
        .unwrap()
        .expect("release hit");
    assert_eq!(found.tag, tag);
    assert_eq!(found.assets.len(), 1);
    assert!(store.has_release("backwpup", tag).unwrap());
    assert!(store.find_release("backwpup", "v9.9.9").unwrap().is_none());

    // Re-store refreshes GitHub-reported flags and reuses the asset files.
    meta.draft = false;
    meta.published_at = Some(Utc::now());
    let second = store.store_release(&meta, &assets).unwrap();
    assert!(second.newly_stored.is_empty());
    assert_eq!(second.reused.len(), 1);
    assert!(!second.release.draft);
    assert!(second.release.published_at.is_some());
    assert_eq!(second.release.dir, first.release.dir);

    assert_eq!(store.list_releases("backwpup").unwrap().len(), 1);
    assert!(store.delete_release("backwpup", tag).unwrap());
    assert!(!store.delete_release("backwpup", tag).unwrap());
    assert!(store.find_release("backwpup", tag).unwrap().is_none());
    assert!(!first.release.dir.exists());
}

// ============================================================================
// Usage + cleaning
// ============================================================================

#[test]
fn usage_reports_recorded_bytes_and_counts() {
    let (_root, store, scratch) = make_store();
    store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", &[0u8; 1000], Some("free"))],
        )
        .unwrap();
    store
        .store_release(
            &ReleaseMetadata::new("wp-rocket", "v3.18.1"),
            &[artifact(&scratch, "r.zip", &[0u8; 500], None)],
        )
        .unwrap();

    let usage = store.usage().unwrap();
    assert_eq!(usage.build_count, 1);
    assert_eq!(usage.release_count, 1);
    assert_eq!(usage.builds_bytes, 1000);
    assert_eq!(usage.releases_bytes, 500);
    assert_eq!(usage.total_bytes, 1500);
    assert_eq!(usage.file_count, 2);
    assert!(
        usage.database_bytes > 0,
        "database file sizes must be counted"
    );
    assert!(usage.oldest_build.is_some());

    assert_eq!(usage.projects.len(), 2);
    assert_eq!(usage.projects[0].project, "backwpup");
    assert_eq!(usage.projects[0].builds_bytes, 1000);
    assert_eq!(usage.projects[1].project, "wp-rocket");
    assert_eq!(usage.projects[1].releases_bytes, 500);
}

#[test]
fn clean_by_age_respects_last_use_and_dry_run() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", &[0u8; 100], None)];
    let old_commit = format!("0000000{}", "a".repeat(33));
    let new_commit = format!("1111111{}", "b".repeat(33));

    let old_meta =
        metadata("backwpup", "5.5.0", &old_commit).with_built_at(Utc::now() - Duration::days(30));
    store.store(&old_meta, &artifacts).unwrap();
    store
        .store(&metadata("backwpup", "5.6.0", &new_commit), &artifacts)
        .unwrap();

    let cutoff = Utc::now() - Duration::days(7);

    // Dry run: reports the old build, deletes nothing.
    let preview = store
        .clean(&CleanOptions::default().older_than(cutoff).dry_run(true))
        .unwrap();
    assert!(preview.dry_run);
    assert_eq!(preview.builds_deleted, 1);
    assert_eq!(preview.bytes_freed, 100);
    assert_eq!(store.list_builds(None, None).unwrap().len(), 2);

    // A cache hit refreshes last-used and shields the entry from cleaning.
    store
        .find_by_commit("backwpup", "5.5.0", &old_commit)
        .unwrap()
        .unwrap();
    let after_touch = store.clear_older_than(cutoff).unwrap();
    assert_eq!(after_touch.builds_deleted, 0);

    // Backdate again via re-store? No — verify the aged path with a fresh
    // store entry instead: a second old build that nobody touches.
    let stale_commit = format!("2222222{}", "c".repeat(33));
    let stale_meta =
        metadata("backwpup", "5.4.0", &stale_commit).with_built_at(Utc::now() - Duration::days(30));
    store.store(&stale_meta, &artifacts).unwrap();
    let report = store.clear_older_than(cutoff).unwrap();
    assert_eq!(report.builds_deleted, 1);
    assert_eq!(report.bytes_freed, 100);
    assert!(report.failures.is_empty());
    assert!(
        store
            .find_by_commit("backwpup", "5.4.0", &stale_commit)
            .unwrap()
            .is_none()
    );
}

#[test]
fn clean_scopes_by_project_and_target() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", &[0u8; 10], None)];
    store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();
    store
        .store(&metadata("wp-rocket", "3.18.1", COMMIT_A), &artifacts)
        .unwrap();
    store
        .store_release(
            &ReleaseMetadata::new("backwpup", "v5.3.2"),
            &[artifact(&scratch, "r.zip", &[0u8; 10], None)],
        )
        .unwrap();

    // Builds-only clean scoped to one project.
    let report = store
        .clean(
            &CleanOptions::default()
                .project("backwpup")
                .target(CleanTarget::Builds),
        )
        .unwrap();
    assert_eq!((report.builds_deleted, report.releases_deleted), (1, 0));
    assert!(
        store
            .find_by_commit("wp-rocket", "3.18.1", "a1b2c3d")
            .unwrap()
            .is_some()
    );
    assert!(store.has_release("backwpup", "v5.3.2").unwrap());

    // clear_all wipes the rest and empties the directory tree.
    let report = store.clear_all().unwrap();
    assert_eq!((report.builds_deleted, report.releases_deleted), (1, 1));
    let usage = store.usage().unwrap();
    assert_eq!(usage.total_bytes, 0);
    assert_eq!(usage.build_count + usage.release_count, 0);
    assert!(
        !store.base_dir().join("backwpup").exists(),
        "empty project dirs are pruned"
    );
}

// ============================================================================
// gc / verify
// ============================================================================

#[test]
fn gc_reconciles_both_directions() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", &[0u8; 64], None)];
    let result = store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();

    // Direction 1: directory vanishes behind the store's back → row dropped.
    std::fs::remove_dir_all(&result.build.dir).unwrap();

    // Direction 2: an unrecorded directory in a managed location → removed.
    let orphan = store.base_dir().join("backwpup/commits/9.9.9/deadbee");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("stray.zip"), [0u8; 32]).unwrap();

    // Fresh temp files must survive (they may belong to a live writer).
    let fresh_temp = store.base_dir().join(".apvm-tmp-fresh");
    std::fs::write(&fresh_temp, b"x").unwrap();

    let report = store.gc().unwrap();
    assert_eq!(report.stale_build_rows, 1);
    assert_eq!(report.orphan_dirs_removed, 1);
    assert_eq!(report.orphan_bytes_removed, 32);
    assert_eq!(report.stale_temp_files_removed, 0);
    assert!(!orphan.exists());
    assert!(fresh_temp.exists());
    assert!(store.list_builds(None, None).unwrap().is_empty());

    // Idempotent: a second gc finds nothing.
    let report = store.gc().unwrap();
    assert_eq!(report.stale_build_rows + report.orphan_dirs_removed, 0);
}

#[test]
fn verify_detects_missing_resized_and_tampered_files() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![
        artifact(&scratch, "ok.zip", b"pristine", None),
        artifact(&scratch, "gone.zip", b"disappears", None),
        artifact(&scratch, "resized.zip", b"shrinks", None),
        artifact(&scratch, "tampered.zip", b"12345678", None),
    ];
    let result = store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();
    assert!(store.verify(VerifyMode::Checksum).unwrap().is_empty());

    let dir = &result.build.dir;
    std::fs::remove_file(dir.join("gone.zip")).unwrap();
    std::fs::write(dir.join("resized.zip"), b"x").unwrap();
    std::fs::write(dir.join("tampered.zip"), b"87654321").unwrap(); // same size

    // Size mode: sees the missing and resized files, not the tampering.
    let issues = store.verify(VerifyMode::Size).unwrap();
    assert_eq!(issues.len(), 2);

    // Checksum mode: sees all three, with precise problems.
    let issues = store.verify(VerifyMode::Checksum).unwrap();
    assert_eq!(issues.len(), 3);
    let problem_for = |name: &str| {
        issues
            .iter()
            .find(|issue| issue.filename == name)
            .unwrap_or_else(|| panic!("expected issue for {name}"))
    };
    assert_eq!(problem_for("gone.zip").problem, VerifyProblem::Missing);
    assert!(matches!(
        problem_for("resized.zip").problem,
        VerifyProblem::SizeMismatch { .. }
    ));
    assert!(matches!(
        problem_for("tampered.zip").problem,
        VerifyProblem::ChecksumMismatch { .. }
    ));

    assert!(store.integrity_check().is_ok());
}

// ============================================================================
// Corruption, repair, relocation, schema
// ============================================================================

#[test]
fn corrupt_database_fails_open_and_repair_readopts_builds() {
    let (root, store, scratch) = make_store();
    let base = store.base_dir().to_path_buf();
    let artifacts = vec![artifact(&scratch, "a.zip", b"survives-corruption", None)];
    let original = store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();
    let original_sha = original.build.artifacts[0].sha256.clone();
    drop(store);

    // Clobber the database (and stale WAL sidecars) with garbage.
    std::fs::write(base.join("apvm.db"), b"garbage, not a sqlite file").unwrap();
    let _ = std::fs::remove_file(base.join("apvm.db-wal"));
    let _ = std::fs::remove_file(base.join("apvm.db-shm"));

    let err = ArtifactStore::open(&base).unwrap_err();
    assert!(
        matches!(err, Error::DatabaseCorrupted { .. }),
        "got: {err:?}"
    );

    // Repair: quarantine + fresh index + adopt files found on disk.
    let (store, report) = ArtifactStore::repair(&base).unwrap();
    let quarantined = report
        .quarantined_database
        .clone()
        .expect("quarantine path");
    assert!(quarantined.exists());
    assert_eq!(report.builds_adopted, 1);
    assert_eq!(report.artifacts_adopted, 1);

    // The adopted build is findable again; content hash was recomputed and
    // matches (same bytes). Source links are gone by design.
    let found = store
        .find_by_commit("backwpup", "5.6.0", COMMIT_A)
        .unwrap()
        .expect("adopted build must be findable");
    assert_eq!(found.artifacts[0].sha256, original_sha);
    assert!(found.sources.is_empty());

    // Repairing a healthy store is a no-op.
    drop(store);
    let (_store, report) = ArtifactStore::repair(&base).unwrap();
    assert!(report.quarantined_database.is_none());
    assert_eq!(report.builds_adopted, 0);
    drop(root);
}

#[test]
fn store_survives_relocation() {
    let root = tempfile::tempdir().unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let old_base = root.path().join("store-old");
    let new_base = root.path().join("store-new");

    {
        let store = ArtifactStore::open(&old_base).unwrap();
        store
            .store(
                &metadata("backwpup", "5.6.0", COMMIT_A),
                &[artifact(&scratch, "a.zip", b"movable", None)],
            )
            .unwrap();
    }
    std::fs::rename(&old_base, &new_base).unwrap();

    let store = ArtifactStore::open(&new_base).unwrap();
    let found = store
        .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
        .unwrap()
        .unwrap();
    assert!(
        found.dir.starts_with(&new_base),
        "paths must resolve inside the new base"
    );
    assert!(found.artifacts[0].path.exists());
}

#[test]
fn database_from_a_newer_apvm_is_refused_not_mangled() {
    let (_root, store, _scratch) = make_store();
    let db_path = store.db_path().to_path_buf();
    let base = store.base_dir().to_path_buf();
    drop(store);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "user_version", 999).unwrap();
    drop(conn);

    let err = ArtifactStore::open(&base).unwrap_err();
    assert!(
        matches!(err, Error::UnsupportedSchema { found: 999, .. }),
        "got: {err:?}"
    );
}

// ============================================================================
// Concurrency + input validation
// ============================================================================

#[test]
fn concurrent_stores_from_multiple_threads_all_land() {
    let (_root, store, scratch) = make_store();
    let sources: Vec<SourceArtifact> = (0..4)
        .map(|i| {
            artifact(
                &scratch,
                &format!("t{i}.zip"),
                format!("bytes-{i}").as_bytes(),
                None,
            )
        })
        .collect();

    std::thread::scope(|scope| {
        for (i, source) in sources.iter().enumerate() {
            let store = &store;
            scope.spawn(move || {
                let commit = format!("{i}{i}{i}{i}{i}{i}{i}{}", "f".repeat(33));
                store
                    .store(
                        &metadata("backwpup", "5.6.0", &commit),
                        std::slice::from_ref(source),
                    )
                    .expect("concurrent store");
            });
        }
    });

    assert_eq!(store.list_builds(Some("backwpup"), None).unwrap().len(), 4);
    assert!(store.verify(VerifyMode::Checksum).unwrap().is_empty());
}

#[test]
fn invalid_inputs_are_rejected_up_front() {
    let (_root, store, scratch) = make_store();
    let good = vec![artifact(&scratch, "a.zip", b"x", None)];

    let cases = [
        metadata("WP-Rocket", "3.18.1", COMMIT_A), // uppercase project
        metadata("apvm.db", "3.18.1", COMMIT_A),   // reserved project
        metadata("backwpup", "beta", COMMIT_A),    // version without digit
        metadata("backwpup", "5.6.0", "a1b2c3"),   // commit too short
        metadata("backwpup", "5.6.0", "zzzzzzz"),  // commit not hex
    ];
    for meta in &cases {
        assert!(
            matches!(store.store(meta, &good), Err(Error::InvalidInput { .. })),
            "expected InvalidInput for {meta:?}"
        );
    }

    // Duplicate target names collide even across case (case-insensitive fs).
    let duplicate = vec![
        artifact(&scratch, "Same.zip", b"x", None),
        SourceArtifact {
            variant_id: None,
            path: scratch.join("Same.zip"),
            target_name: "same.zip".to_string(),
        },
    ];
    assert!(matches!(
        store.store(&metadata("backwpup", "5.6.0", COMMIT_A), &duplicate),
        Err(Error::InvalidInput { .. })
    ));

    // Missing source file has its own error.
    let ghost = vec![SourceArtifact {
        variant_id: None,
        path: scratch.join("does-not-exist.zip"),
        target_name: "ghost.zip".to_string(),
    }];
    assert!(matches!(
        store.store(&metadata("backwpup", "5.6.0", COMMIT_A), &ghost),
        Err(Error::SourceFileMissing { .. })
    ));

    // Empty artifact list is meaningless for a cache.
    assert!(matches!(
        store.store(&metadata("backwpup", "5.6.0", COMMIT_A), &[]),
        Err(Error::InvalidInput { .. })
    ));
}

#[test]
fn store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ArtifactStore>();
}

// ============================================================================
// Destructive-op safety against a logically-tampered database
// ============================================================================

/// A `dir_path` that points outside the store (only reachable by tampering
/// with the database, which `quick_check` cannot catch) must never cause a
/// destructive operation to remove files outside the store — or the store
/// root itself.
#[test]
fn tampered_dir_path_cannot_escape_on_delete_or_clean() {
    let root = tempfile::tempdir().unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let base = root.path().join("store");

    // A precious directory OUTSIDE the store that a traversal would hit.
    let victim = root.path().join("precious-outside");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keepme.txt"), b"do not delete").unwrap();

    let store = ArtifactStore::open(&base).unwrap();
    store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", b"real", None)],
        )
        .unwrap();

    // Tamper: rewrite the build's dir_path to a traversal that escapes to
    // the precious directory. WAL lets a second connection write; the
    // store's connection sees the committed change on its next query.
    {
        let conn = rusqlite::Connection::open(base.join("apvm.db")).unwrap();
        conn.execute(
            "UPDATE builds SET dir_path = ?1 WHERE project = 'backwpup'",
            ["../../precious-outside"],
        )
        .unwrap();
    }

    // delete_build must drop the row WITHOUT following the escape.
    assert!(store.delete_build("backwpup", "5.6.0", COMMIT_A).unwrap());
    assert!(
        victim.join("keepme.txt").exists(),
        "delete_build escaped the store and removed external files"
    );
    assert!(base.exists(), "store root must survive");

    // Same protection for clean(): tamper a fresh row, then clear_all.
    store
        .store(
            &metadata("backwpup", "5.7.0", COMMIT_A),
            &[artifact(&scratch, "b.zip", b"real2", None)],
        )
        .unwrap();
    {
        let conn = rusqlite::Connection::open(base.join("apvm.db")).unwrap();
        conn.execute(
            "UPDATE builds SET dir_path = ?1 WHERE version = '5.7.0'",
            ["../../precious-outside"],
        )
        .unwrap();
    }
    let report = store.clear_all().unwrap();
    assert_eq!(report.builds_deleted, 1);
    assert_eq!(
        report.bytes_freed, 0,
        "nothing real was freed (escape refused)"
    );
    assert_eq!(report.failures.len(), 1);
    assert!(
        victim.join("keepme.txt").exists(),
        "clean escaped the store and removed external files"
    );
    assert!(base.exists(), "store root must survive clean");
}

// ============================================================================
// gc safety: foreign data inside the store directory
// ============================================================================

#[test]
fn gc_never_touches_foreign_trees_that_mimic_the_layout() {
    let (_root, store, scratch) = make_store();
    store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", b"real", None)],
        )
        .unwrap();

    // A hand-placed tree whose name could never come from the store
    // (uppercase project) but whose shape mimics the managed layout.
    let foreign = store.base_dir().join("My-Backups/commits/1.0/abc1234");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("precious.txt"), b"user data").unwrap();

    // Same for an invalid version name under a real project.
    let bad_version = store
        .base_dir()
        .join("backwpup/commits/not-a-version/abc1234");
    std::fs::create_dir_all(&bad_version).unwrap();
    std::fs::write(bad_version.join("precious2.txt"), b"more user data").unwrap();

    let report = store.gc().unwrap();
    assert_eq!(report.orphan_dirs_removed, 0);
    assert!(
        foreign.join("precious.txt").exists(),
        "foreign project tree must survive gc"
    );
    assert!(
        bad_version.join("precious2.txt").exists(),
        "foreign version tree must survive gc"
    );

    // The genuine build is untouched too.
    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
            .unwrap()
            .is_some()
    );
}

#[test]
fn gc_sweeps_temp_files_at_every_managed_depth() {
    let (_root, store, scratch) = make_store();
    let result = store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", b"x", None)],
        )
        .unwrap();

    // Plant stale temp files at every level gc sweeps, then backdate them.
    let locations = [
        store.base_dir().join(".apvm-tmp-root"),
        store.base_dir().join("backwpup/.apvm-tmp-project"),
        store
            .base_dir()
            .join("backwpup/commits/5.6.0/.apvm-tmp-version"),
        result.build.dir.join(".apvm-tmp-leaf"),
    ];
    for path in &locations {
        std::fs::write(path, b"leftover").unwrap();
        // Backdate the mtime beyond the staleness threshold (1h).
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(old).unwrap();
    }

    let report = store.gc().unwrap();
    assert_eq!(report.stale_temp_files_removed, locations.len() as u64);
    for path in &locations {
        assert!(!path.exists(), "{} must be swept", path.display());
    }
    // The build itself is untouched.
    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
            .unwrap()
            .is_some()
    );
}

// ============================================================================
// clean: honest byte accounting when a directory cannot be removed
// ============================================================================

#[cfg(unix)]
#[test]
fn clean_subtracts_bytes_for_directories_it_could_not_remove() {
    use std::os::unix::fs::PermissionsExt;

    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", &[0u8; 100], None)];
    let commit_1 = format!("aaaaaaa{}", "1".repeat(33));
    let commit_2 = format!("bbbbbbb{}", "2".repeat(33));
    let kept = store
        .store(&metadata("backwpup", "5.5.0", &commit_1), &artifacts)
        .unwrap();
    store
        .store(&metadata("backwpup", "5.6.0", &commit_2), &artifacts)
        .unwrap();

    // Make one victim directory undeletable: strip write permission from its
    // parent so the directory entry cannot be unlinked.
    let parent = kept.build.dir.parent().unwrap().to_path_buf();
    let mut perms = std::fs::metadata(&parent).unwrap().permissions();
    let original_mode = perms.mode();
    perms.set_mode(0o555);
    std::fs::set_permissions(&parent, perms).unwrap();

    let report = store.clear_all().unwrap();

    // Restore permissions before asserting so cleanup always succeeds.
    let mut restore = std::fs::metadata(&parent).unwrap().permissions();
    restore.set_mode(original_mode);
    std::fs::set_permissions(&parent, restore).unwrap();

    assert_eq!(report.builds_deleted, 2);
    assert_eq!(report.failures.len(), 1, "one dir removal must fail");
    assert_eq!(
        report.bytes_freed, 100,
        "bytes of the failed removal must not be counted as freed"
    );

    // The metadata is gone either way; gc reclaims the leftover files now
    // that permissions are restored.
    assert!(store.list_builds(None, None).unwrap().is_empty());
    let gc = store.gc().unwrap();
    assert_eq!(gc.orphan_dirs_removed, 1);
}

// ============================================================================
// Lock: mutating operations exclude each other across handles
// ============================================================================

#[test]
fn store_and_clean_serialize_across_store_handles() {
    let root = tempfile::tempdir().unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let base = root.path().join("store");

    // Two independent handles to the same store (as two processes would).
    let store_a = ArtifactStore::open(&base).unwrap();
    let store_b = ArtifactStore::open(&base).unwrap();

    let commits: Vec<String> = (0..6)
        .map(|i| format!("{i}{i}{i}{i}{i}{i}{i}{}", "e".repeat(33)))
        .collect();

    // Interleave stores (handle A) with full cleans (handle B) from many
    // threads. The advisory lock must keep every operation atomic: no store
    // may ever observe a half-deleted state, no clean a half-stored one.
    std::thread::scope(|scope| {
        for commit in &commits {
            let store_a = &store_a;
            let scratch = scratch.clone();
            scope.spawn(move || {
                let name = format!("{}.zip", &commit[..7]);
                let file = artifact(&scratch, &name, b"payload", None);
                store_a
                    .store(&metadata("backwpup", "5.6.0", commit), &[file])
                    .expect("store during concurrent cleans");
            });
        }
        for _ in 0..3 {
            let store_b = &store_b;
            scope.spawn(move || {
                store_b.clear_all().expect("clean during concurrent stores");
            });
        }
    });

    // Whatever survived must be fully consistent: every recorded build
    // passes a checksum audit and gc finds nothing to reconcile.
    assert!(store_a.verify(VerifyMode::Checksum).unwrap().is_empty());
    let gc = store_a.gc().unwrap();
    assert_eq!(gc.stale_build_rows, 0, "no row may point at missing files");
}

// ============================================================================
// Additional lookup / listing / helper coverage
// ============================================================================

#[test]
fn find_commit_in_versions_prefers_newest_build() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![artifact(&scratch, "a.zip", b"x", None)];

    store
        .store(
            &metadata("backwpup", "5.5.0", COMMIT_A).with_built_at(Utc::now() - Duration::days(10)),
            &artifacts,
        )
        .unwrap();
    store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();

    let found = store
        .find_commit_in_versions("backwpup", "a1b2c3d")
        .unwrap()
        .unwrap();
    assert_eq!(found.version, "5.6.0", "newest build wins");

    let versions = store.list_versions("backwpup").unwrap();
    assert_eq!(versions, vec!["5.6.0".to_string(), "5.5.0".to_string()]);
}

#[test]
fn has_release_does_not_count_as_usage_but_find_does() {
    let (_root, store, scratch) = make_store();
    store
        .store_release(
            &ReleaseMetadata::new("backwpup", "v5.3.2"),
            &[artifact(&scratch, "r.zip", b"bytes", None)],
        )
        .unwrap();

    let before = store.list_releases("backwpup").unwrap()[0].last_used_at;

    // has_release: no usage bump.
    assert!(store.has_release("backwpup", "v5.3.2").unwrap());
    let after_has = store.list_releases("backwpup").unwrap()[0].last_used_at;
    assert_eq!(before, after_has);

    // find_release: bumps last-used (feeds age-based cleaning).
    std::thread::sleep(std::time::Duration::from_millis(5));
    store.find_release("backwpup", "v5.3.2").unwrap().unwrap();
    let after_find = store.list_releases("backwpup").unwrap()[0].last_used_at;
    assert!(after_find > before);
}

#[test]
fn stored_build_variant_helpers() {
    let (_root, store, scratch) = make_store();
    let artifacts = vec![
        artifact(&scratch, "free.zip", b"free", Some("free")),
        artifact(&scratch, "plain.zip", b"plain", None),
    ];
    let result = store
        .store(&metadata("backwpup", "5.6.0", COMMIT_A), &artifacts)
        .unwrap();

    let build = &result.build;
    assert!(build.has_variant(Some("free")));
    assert!(build.has_variant(None));
    assert!(!build.has_variant(Some("pro")));
    assert_eq!(
        build.artifact(Some("free")).map(|a| a.filename.as_str()),
        Some("free.zip")
    );
    assert_eq!(
        build.artifact(None).map(|a| a.filename.as_str()),
        Some("plain.zip")
    );
    assert!(build.artifact(Some("pro")).is_none());
}

#[test]
fn verify_covers_release_assets_too() {
    let (_root, store, scratch) = make_store();
    let result = store
        .store_release(
            &ReleaseMetadata::new("backwpup", "v5.3.2"),
            &[artifact(&scratch, "r.zip", b"release-bytes", None)],
        )
        .unwrap();

    std::fs::remove_file(&result.release.assets[0].path).unwrap();

    let issues = store.verify(VerifyMode::Presence).unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].filename, "r.zip");
    assert!(matches!(issues[0].problem, VerifyProblem::Missing));
    assert!(matches!(
        &issues[0].context,
        apvm_storage::IssueContext::Release { tag } if tag == "v5.3.2"
    ));
}

#[test]
fn traversal_shaped_release_tags_stay_inside_the_store() {
    let (_root, store, scratch) = make_store();
    let tag = "../../../etc/evil";

    let result = store
        .store_release(
            &ReleaseMetadata::new("backwpup", tag),
            &[artifact(&scratch, "r.zip", b"contained", None)],
        )
        .unwrap();

    // The asset directory must live inside the store's base directory.
    assert!(
        result.release.dir.starts_with(store.base_dir()),
        "sanitized tag dir must stay inside the store, got {}",
        result.release.dir.display()
    );
    // And the exact hostile tag round-trips through the database.
    let found = store.find_release("backwpup", tag).unwrap().unwrap();
    assert_eq!(found.tag, tag);
}

#[test]
fn zero_byte_artifacts_are_stored_and_verified() {
    let (_root, store, scratch) = make_store();
    let result = store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "empty.zip", b"", None)],
        )
        .unwrap();
    assert_eq!(result.build.artifacts[0].size_bytes, 0);

    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
            .unwrap()
            .is_some(),
        "zero-byte artifacts are legitimate and healthy"
    );
    assert!(store.verify(VerifyMode::Checksum).unwrap().is_empty());

    // Re-store reuses the (zero-byte) file rather than copying again.
    let again = store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "empty.zip", b"", None)],
        )
        .unwrap();
    assert_eq!(again.reused, vec!["empty.zip".to_string()]);
}

#[test]
fn repair_skips_unadoptable_entries_and_counts_release_dirs() {
    let (_root, store, scratch) = make_store();
    let base = store.base_dir().to_path_buf();
    store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", b"adoptable", None)],
        )
        .unwrap();
    store
        .store_release(
            &ReleaseMetadata::new("backwpup", "v5.3.2"),
            &[artifact(&scratch, "r.zip", b"asset", None)],
        )
        .unwrap();
    drop(store);

    // Plant an unadoptable commit dir (name is not hex) next to the real one.
    std::fs::create_dir_all(base.join("backwpup/commits/5.6.0/not-hex")).unwrap();
    std::fs::write(
        base.join("backwpup/commits/5.6.0/not-hex/file.zip"),
        b"skip me",
    )
    .unwrap();

    std::fs::write(base.join("apvm.db"), b"corrupt").unwrap();
    let _ = std::fs::remove_file(base.join("apvm.db-wal"));
    let _ = std::fs::remove_file(base.join("apvm.db-shm"));

    let (store, report) = ArtifactStore::repair(&base).unwrap();
    assert_eq!(report.builds_adopted, 1);
    assert!(report.entries_skipped >= 1, "not-hex dir must be skipped");
    assert_eq!(
        report.orphan_release_dirs, 1,
        "release dirs are counted, not adopted"
    );

    // The skipped dir is still on disk (repair never deletes).
    assert!(
        base.join("backwpup/commits/5.6.0/not-hex/file.zip")
            .exists()
    );
    // The adopted build works.
    assert!(
        store
            .find_by_commit("backwpup", "5.6.0", COMMIT_A)
            .unwrap()
            .is_some()
    );
}

// ============================================================================
// Incremental variants: adding a variant to an existing build in a *later*
// store call (same project + version + commit) must add it without touching
// the variants already stored.
// ============================================================================

#[test]
fn storing_a_second_variant_later_preserves_the_first() {
    let (_root, store, scratch) = make_store();

    // Day 0: only the pro-en variant is built and cached.
    let first = store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(
                &scratch,
                "backwpup-pro-en.zip",
                b"pro-en-bytes",
                Some("pro-en"),
            )],
        )
        .unwrap();
    assert_eq!(first.newly_stored, vec!["backwpup-pro-en.zip".to_string()]);
    let build_id = first.build.id;
    let build_dir = first.build.dir.clone();
    let pro_en_path = first.build.artifacts[0].path.clone();
    assert_eq!(std::fs::read(&pro_en_path).unwrap(), b"pro-en-bytes");

    // Days later: the free variant is built for the SAME commit + version.
    let second = store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(
                &scratch,
                "backwpup-free.zip",
                b"free-bytes",
                Some("free"),
            )],
        )
        .unwrap();

    // Only the new variant is copied; the old one is not re-copied or reused
    // (it was not part of this call at all).
    assert_eq!(second.newly_stored, vec!["backwpup-free.zip".to_string()]);
    assert!(second.reused.is_empty());

    // Same build row and directory — not a duplicate build.
    assert_eq!(second.build.id, build_id);
    assert_eq!(second.build.dir, build_dir);
    assert_eq!(store.list_builds(Some("backwpup"), None).unwrap().len(), 1);

    // The build now carries BOTH variants, and the pre-existing file is intact.
    let mut names: Vec<&str> = second
        .build
        .artifacts
        .iter()
        .map(|a| a.filename.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["backwpup-free.zip", "backwpup-pro-en.zip"]);
    assert_eq!(
        std::fs::read(&pro_en_path).unwrap(),
        b"pro-en-bytes",
        "the previously stored artifact file must be untouched"
    );

    // Both variants report as present and healthy.
    let variants = store
        .get_existing_variants("backwpup", "5.6.0", "a1b2c3d")
        .unwrap();
    assert!(variants.contains(&Some("pro-en".to_string())));
    assert!(variants.contains(&Some("free".to_string())));

    // A lookup requiring both variants is now satisfied.
    let both = vec![Some("pro-en".to_string()), Some("free".to_string())];
    let request = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.6.0")
        .require_variants(&both);
    assert!(store.lookup_build(&request).unwrap().is_hit());

    // A checksum audit of the whole store passes (both files match records).
    assert!(store.verify(VerifyMode::Checksum).unwrap().is_empty());
}

// ============================================================================
// Same commit, different versions: a strict rebuild of another version must
// coexist with the original as an independent build/directory.
// ============================================================================

#[test]
fn strict_rebuild_of_another_version_coexists_with_the_original() {
    let (_root, store, scratch) = make_store();

    // Cached: the commit built as 5.6.0.
    let v56 = store
        .store(
            &metadata("backwpup", "5.6.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", b"bytes", None)],
        )
        .unwrap();
    // Directory layout is {project}/commits/{version}/{commit}.
    assert!(
        v56.build.dir.ends_with("backwpup/commits/5.6.0/a1b2c3d"),
        "unexpected layout: {}",
        v56.build.dir.display()
    );

    // Strict lookup for the same commit at 5.7.0: miss, telling the caller
    // which versions ARE cached (so it knows to rebuild).
    let strict_57 = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.7.0")
        .version_match(VersionMatch::Strict);
    match store.lookup_build(&strict_57).unwrap() {
        LookupResult::Miss(MissReason::VersionMismatch { available }) => {
            assert_eq!(available, vec!["5.6.0".to_string()]);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }

    // The strict caller rebuilds and stores 5.7.0 of the same commit.
    let v57 = store
        .store(
            &metadata("backwpup", "5.7.0", COMMIT_A),
            &[artifact(&scratch, "a.zip", b"bytes", None)],
        )
        .unwrap();

    // Two independent builds/directories now coexist for one commit.
    assert_ne!(v56.build.dir, v57.build.dir);
    assert!(v56.build.dir.exists() && v57.build.dir.exists());
    assert!(v57.build.dir.ends_with("backwpup/commits/5.7.0/a1b2c3d"));
    assert_eq!(store.list_builds(Some("backwpup"), None).unwrap().len(), 2);

    // Each version is findable independently at its own version.
    assert_eq!(
        store
            .find_by_commit("backwpup", "5.6.0", "a1b2c3d")
            .unwrap()
            .unwrap()
            .version,
        "5.6.0"
    );
    assert_eq!(
        store
            .find_by_commit("backwpup", "5.7.0", "a1b2c3d")
            .unwrap()
            .unwrap()
            .version,
        "5.7.0"
    );

    // With both cached, a strict lookup at each version hits cleanly...
    for version in ["5.6.0", "5.7.0"] {
        let strict = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
            .version(version)
            .version_match(VersionMatch::Strict);
        let hit = store
            .lookup_build(&strict)
            .unwrap()
            .hit()
            .unwrap_or_else(|| panic!("strict hit expected for {version}"));
        assert!(hit.version_matched);
        assert_eq!(hit.build.version, version);
    }

    // ...and a lenient lookup prefers the exact-version build over the other.
    let lenient_57 = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d")).version("5.7.0");
    let hit = store.lookup_build(&lenient_57).unwrap().hit().unwrap();
    assert!(hit.version_matched);
    assert_eq!(hit.build.version, "5.7.0");

    // A strict miss for a THIRD, uncached version lists both cached versions,
    // newest first.
    let strict_58 = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.8.0")
        .version_match(VersionMatch::Strict);
    match store.lookup_build(&strict_58).unwrap() {
        LookupResult::Miss(MissReason::VersionMismatch { available }) => {
            assert_eq!(available, vec!["5.7.0".to_string(), "5.6.0".to_string()]);
        }
        other => panic!("expected VersionMismatch listing both versions, got {other:?}"),
    }
}
