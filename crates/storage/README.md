# apvm-storage

SQLite-backed cache for APVM build artifacts and GitHub Release assets.

One embedded SQLite database (`apvm.db`, via `rusqlite` with the bundled C
library — zero system dependencies) is the single source of truth for all
metadata. Artifact **files** live in plain directories next to it. There are
no manifests and no symlinks.

```text
{base_dir}/apvm.db                                  ← all metadata
{base_dir}/{project}/commits/{version}/{commit}/    ← build artifact files
{base_dir}/{project}/releases/{tag}/                ← release asset files
```

Directory paths are stored relative to `base_dir`, so the whole store can be
moved and reopened.

## Schema (v1, `PRAGMA user_version`)

| Table             | Keyed by                            | Holds                                            |
| ----------------- | ----------------------------------- | ------------------------------------------------ |
| `builds`          | `(project, version, commit)` unique | directory, built-at, last-used timestamps        |
| `build_artifacts` | `(build, filename)` unique          | variant, size, SHA-256                           |
| `build_sources`   | `(build, kind, reference)` unique   | PR/branch/tag/commit links + branch + linked-at  |
| `releases`        | `(project, tag)` unique             | GitHub flags, directory, cached-at, last-used    |
| `release_assets`  | `(release, filename)` unique        | size, SHA-256                                    |

All tables are `STRICT`; deletes cascade; timestamps are epoch milliseconds
(exposed as `chrono::DateTime<Utc>`).

## Guarantees

- **Row ⇒ files.** Stores copy files first (temp file + fsync + atomic
  rename), commit metadata last; deletes remove metadata first, files last.
  Crashes leave only invisible or orphan files — never a record pointing at
  nothing. `gc()` reclaims orphans in both directions.
- **Hits are verified.** Finds/lookups check presence + size before
  reporting a hit; damaged entries degrade to a miss and re-storing heals
  them. `verify(VerifyMode::Checksum)` re-hashes everything on demand.
- **Corruption story.** WAL journal + integrity check on every open; a
  damaged database fails with `Error::DatabaseCorrupted`, and
  `ArtifactStore::repair()` quarantines it, rebuilds a fresh index, and
  re-adopts artifact files found on disk (hashes recomputed).
- **Concurrency.** `ArtifactStore` is `Send + Sync`. Cross-process:
  SQLite/WAL guards the database; an advisory lock (`.apvm.lock`)
  serializes mutating operations so a clean can't race a store. Keep the
  store on a local disk (advisory locks over NFS/SMB are unreliable).
- **Async.** Everything is intentionally blocking; call through
  `tokio::task::spawn_blocking` from async code.

## API tour

```rust,no_run
use apvm_storage::{
    ArtifactStore, BuildMetadata, BuildSource, CleanOptions, CleanTarget, LookupKey,
    LookupRequest, SourceArtifact, VerifyMode, VersionMatch,
};

fn main() -> apvm_storage::Result<()> {
    let store = ArtifactStore::open("/var/lib/apvm/builds")?;

    // Store a build (idempotent; re-stores reuse healthy files).
    let metadata = BuildMetadata::new(
        "backwpup", "5.6.0",
        BuildSource::PullRequest(123),
        "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
        "feature/faster-backups".to_string(),
    );
    let artifacts = vec![SourceArtifact {
        variant_id: Some("pro-en".into()),
        path: "/tmp/build-output/backwpup-pro-en-5.6.0.zip".into(),
        target_name: "backwpup-pro-en-5.6.0.zip".into(),
    }];
    store.store(&metadata, &artifacts)?;

    // Cache lookup with version semantics. BackWPup stamps the version into
    // the artifact, so version-critical flows use Strict; others use the
    // default Lenient and check `version_matched` on the hit.
    let required = vec![Some("pro-en".to_string())];
    let request = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.6.0")
        .version_match(VersionMatch::Strict)
        .require_variants(&required);
    if let Some(hit) = store.lookup_build(&request)?.hit() {
        println!("cached at {}", hit.build.dir.display());
    }

    // Disk accounting and cleanup.
    let usage = store.usage()?;
    println!("cache holds {} bytes across {} builds", usage.total_bytes, usage.build_count);

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let preview = store.clean(&CleanOptions::default().older_than(cutoff).dry_run(true))?;
    println!("cleaning would free {} bytes", preview.bytes_freed);
    store.clear_older_than(cutoff)?;                       // by age (last-used)
    store.clean(&CleanOptions::default()                   // scoped variants
        .project("backwpup")
        .target(CleanTarget::Releases))?;
    store.clear_all()?;                                    // everything

    // Maintenance.
    store.gc()?;                            // reconcile db ↔ disk, sweep temp files
    store.verify(VerifyMode::Checksum)?;    // deep integrity audit
    store.integrity_check()?;               // SQLite-level check
    Ok(())
}
```

Releases mirror the build API: `store_release` / `find_release` (exact-tag,
health-checked) / `has_release` / `list_releases` / `delete_release`.
Release tags are stored verbatim; on-disk directory names are sanitized
derivations, so tags like `release/5.3` are safe everywhere. Keyword tags
(`latest-stable`, ...) must be resolved against the GitHub API *before*
consulting the cache — they are moving targets and are never cached.

## Design notes

- **Why SQLite/rusqlite?** A local artifact cache needs an index that
  supports aggregation (usage by project), range deletes (clean by age) and
  prefix search (short commits) — with real crash-safety. That is exactly
  SQLite's home turf, and `rusqlite` + bundled SQLite is the boring, proven
  way to embed it (cargo itself tracks its global cache the same way).
  Turso was evaluated and is still in beta by its own documentation; a full
  ORM adds machinery without removing risk at this scale. All SQL lives in
  the private `db/` module behind typed functions — nothing else in the
  crate sees a query string.
- **`last_used_at`** is bumped on every hit, so `clean(older_than: …)`
  implements LRU-style aging: entries you keep using never age out, and
  never-used entries age from their build/cache time.
- **Short-commit collisions**: directories are named by the 7-char short
  hash, lengthened automatically (12, then full) if another commit already
  owns that prefix within the same project + version. The database always
  records the full hash it was given.
