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
  reporting a hit; such damaged entries degrade to a miss and re-storing
  heals them. Same-size corruption passes that check: only
  `verify(VerifyMode::Checksum)` sees it, and `gc_with(VerifyMode::Checksum)`
  removes it. In general `gc_with(mode)` removes what `verify(mode)` reports —
  damaged file records, their bad files, and builds/releases left without
  files — so the next store caches them again; files it cannot read are kept
  and listed in `GcReport::failures` (`gc()` = `gc_with(VerifyMode::Size)`).
- **Swapped databases are detectable.** `is_current()` tells a long-lived
  handle that its database file was renamed aside (as older apvm versions'
  repair did), deleted or replaced (repair now resets it in place); mutations through such a handle fail with
  `Error::StaleHandle` instead of updating a database the store no longer
  uses — open the store again.
- **Deletes stay inside the store.** `clean`, `gc` and `delete_*` remove only
  store-shaped directories reached without symlinks, and never a file or
  directory another record still names up to letter case (case-insensitive
  filesystems). Entries differing only in case get distinct directories.
- **Corruption story.** WAL journal + integrity check on every open; a
  damaged database fails with `Error::DatabaseCorrupted` (unreadable rows:
  `Error::Data`; corruption met later: `Error::is_corruption()`), and
  `ArtifactStore::repair()` keeps a copy (`apvm.db.corrupt-<ms>`), **resets
  the database in place** (SQLite's reset procedure; unreadable rows are
  cleared), and re-adopts the artifact files found on disk (hashes
  recomputed). The file is never renamed or replaced, so connections in
  any process stay bound to it — a rename could make another process's
  connection take the new file's shared memory for its own (SIGBUS). A file
  SQLite can no longer read as a database is only reset once nobody has it
  open: while another connection holds it, repair is refused
  (`Error::DatabaseInUse`, nothing changed). A readable but damaged
  database is reset under its users, who then see the repaired index. A *deleted* or blank (0-byte)
  database with builds left is `StoreState::Orphaned`: `open` refuses
  (`Error::MissingDatabase`) and `repair()` rebuilds it. Repair is
  crash-safe: it hashes first, writes a marker (`apvm.db.repairing`) before
  touching the database and removes it once the re-filled index commits in
  one transaction; while the marker exists the store is `Orphaned`, so an
  interrupted repair never leaves a half-filled index that `gc` would
  trust.
- **Only its own files.** `ArtifactStore::inspect()` classifies a directory
  (`StoreState`: missing, empty, present, orphaned, foreign) without touching
  it. A store is only created in a missing or *vacant* directory — nothing
  but OS metadata files (`.DS_Store`, `Thumbs.db`, `desktop.ini`) and its own
  files — so `gc` can never meet store-shaped data someone else puts beside
  it. Refused with `Error::ForeignDirectory`: any other directory when a
  store would be created, and store-shaped content without the store's
  `.apvm.lock` file (created before a store's first build, never removed),
  whatever `apvm.db` holds. Also refused: someone else's SQLite file at
  `apvm.db` (`Error::ForeignDatabase`, never written to) and a symlinked
  `apvm.db`, `.apvm.lock` or repair marker (never followed). A refused
  repair leaves the directory as it found it. `open_existing()` never
  creates anything. `gc` and `repair` walk only store-produced names and
  never follow symlinks; repair adopts only build output (no hidden files,
  no `Thumbs.db` / `desktop.ini`). Paths read back from the database are
  checked too: a tampered record is unreadable (`Error::Data`) to lookups,
  `VerifyProblem::InvalidRecord` to verify and gc, and never written
  through.
- **Concurrency.** `ArtifactStore` is `Send + Sync`. Cross-process:
  SQLite/WAL guards the database (concurrent first opens are safe:
  migrations run in an `IMMEDIATE` transaction, and the WAL switch retries
  within the busy timeout; an open that finds the file replaced — as older
  apvm versions' repair did — reopens on the new one); an advisory lock
  (`.apvm.lock`) serializes mutating operations — database creation
  included — so a clean can't race a store and a create can't race a
  repair. Keep the store on a local disk (advisory locks over NFS/SMB are
  unreliable).
- **Async.** Everything is intentionally blocking; call through
  `tokio::task::spawn_blocking` from async code.

## API tour

```rust,no_run
use apvm_storage::{
    ArtifactStore, BuildMetadata, BuildSource, CleanOptions, CleanTarget, LookupKey,
    LookupRequest, SourceArtifact, VerifyMode,
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

    // Cache lookup with exact version matching. A build of the same commit at
    // a different version is a miss (the version is baked into the artifact),
    // so the caller rebuilds at the requested version.
    let required = vec![Some("pro-en".to_string())];
    let request = LookupRequest::new("backwpup", LookupKey::Commit("a1b2c3d"))
        .version("5.6.0")
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

    // Filters can be checked without any store — e.g. to reject bad input
    // before knowing whether a cache exists (`clean` also runs this).
    CleanOptions::default().project("backwpup").validate()?;

    // Maintenance.
    store.gc()?;                            // reconcile db ↔ disk, sweep temp files
    store.verify(VerifyMode::Checksum)?;    // deep integrity audit
    store.gc_with(VerifyMode::Checksum)?;   // …and remove what it reports
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
