//! End-to-end build test for the Imagify plugin, through the real pipeline:
//! a genuine clone of `wp-media/imagify-plugin`, the project's own
//! `bin/build-zip.sh` packaging script, and a real `ArtifactStore`.
//!
//! Unlike [`cache_e2e`](./cache_e2e.rs), this test hits the network and shells
//! out to the full toolchain the script needs (`bash`, `composer`, `npm`,
//! `curl`, `rsync`, `zip`), so it is:
//!
//! - `#[ignore]` — excluded from the default (hermetic, offline) `cargo test`.
//! - `#![cfg(unix)]` — the packaging script is Bash-based.
//!
//! Run it explicitly (needs network + the tools above in `PATH`):
//!
//! ```sh
//! cargo test -p apvm-core --test imagify_build_e2e -- --ignored --nocapture
//! ```
//!
//! It locks the contract that building Imagify from `develop`:
//! - produces exactly one artifact (single-variant plugin),
//! - names it `imagify-<version>.zip` with the version auto-detected from the
//!   `imagify.php` header (never the script's default `imagify.zip`),
//! - and delivers a non-empty file into the requested output directory.
#![cfg(unix)]

use apvm_core::{Apvm, BuildRequest, Config, NullReporter};

/// Build Imagify from `develop` and assert the versioned artifact is delivered.
#[tokio::test]
#[ignore = "network + composer/npm/bash toolchain; run with --ignored"]
async fn builds_imagify_from_develop_with_versioned_artifact() {
    let cache = tempfile::tempdir().expect("cache tempdir");
    let output = tempfile::tempdir().expect("output tempdir");

    // `Apvm::new` loads the known-project registry, which includes Imagify.
    let apvm = Apvm::new(Config::new(cache.path().to_path_buf())).expect("Apvm::new");

    let result = apvm
        .build(
            BuildRequest::new("imagify", "branch:develop", output.path()),
            &NullReporter,
        )
        .await
        .expect("imagify build from develop should succeed");

    // Single-variant plugin → exactly one artifact.
    assert_eq!(
        result.result.artifacts.len(),
        1,
        "imagify must produce exactly one artifact"
    );

    // Version is embedded in imagify.php and auto-detected.
    let version = &result.result.version;
    assert!(
        !version.is_empty(),
        "a version must be detected from source"
    );

    // The artifact is named imagify-<version>.zip (versioned), matching the
    // WP Rocket convention rather than the script's default imagify.zip.
    let artifact = &result.result.artifacts[0];
    assert_eq!(
        artifact.filename,
        format!("imagify-{version}.zip"),
        "artifact must be named imagify-<version>.zip"
    );
    assert!(artifact.variant_id.is_none(), "imagify has no variants");

    // The file must physically exist in the output directory and be non-empty.
    let delivered = output.path().join(&artifact.filename);
    let meta = std::fs::metadata(&delivered)
        .unwrap_or_else(|e| panic!("artifact '{}' missing: {e}", delivered.display()));
    assert!(meta.is_file(), "artifact must be a file");
    assert!(meta.len() > 0, "artifact must be non-empty");
}
