# apvm-storage

Production-grade, filesystem-backed cache for built plugin artifacts and
downloaded GitHub Release assets. Point it at a builds cache directory and it
deduplicates artifacts per commit, tracks every build in JSON manifests, and
answers "is this already built?" before a pipeline spends minutes rebuilding.

> **Naming note**: the crate is called *storage* rather than *cache* on
> purpose — it holds the authoritative copy of built artifacts (a cache
> implies the content is evictable and reproducible elsewhere at zero cost;
> rebuilding is exactly the cost this crate exists to avoid). The directory
> it manages is what the CLI/config layers expose as the *builds cache dir*.

## Directory layout

```text
{base_dir}/
├── .apvm-store.json                  # store marker + layout schema version
├── .apvm-store.lock                  # cross-process mutation lock
└── {project}/
    ├── releases/                     # cached GitHub Release assets, keyed by tag
    │   └── {tag}/
    │       ├── release-manifest.json
    │       └── *.zip
    └── {major.minor}/
        └── {version}/
            ├── by-commit/
            │   └── {commit_short}/   # the single physical copy per commit
            │       ├── build-manifest.json
            │       └── *.zip
            └── by-source/            # navigation links, one dir per source
                ├── pr-123/{commit}              → ../../by-commit/{commit}
                └── branch-develop/{commit}      → ../../by-commit/{commit}
```

- `releases/` can never collide with a `{major.minor}` directory: major.minor
  names always contain a `.`, `releases` does not.
- Source/tag names are sanitized for the filesystem; names that needed
  sanitizing get a deterministic 8-hex-char suffix so distinct sources never
  share a directory. The *exact* original source values are recorded in the
  manifests.
- Links are relative symlinks on Unix (the store is relocatable) and NTFS
  junctions on Windows (created via the Win32 API, no admin rights needed).

## Guarantees

| Concern | Mechanism |
|---|---|
| Path safety | Every caller-provided value that becomes a path component is validated (`validate.rs`): no traversal, no separators, no Windows-reserved names, no shadowing of store metadata files. |
| Crash consistency | Manifests and artifacts are written to temp files, fsynced, and atomically renamed (`fsx.rs`). Readers never see torn files; stale temps are swept on the next store. |
| Concurrency | Mutations hold an exclusive cross-process lock (`File::lock`); reads are lock-free and stay consistent thanks to atomic renames. |
| Integrity | Manifests record size + SHA-256 per file. Read paths verify sizes and treat damaged entries as cache misses; `VerifyMode::Checksum` is available for full audits. |
| Self-healing | A damaged/incomplete entry reads as a miss; the next `store()` / `store_release()` of the same build re-copies only what is broken. |
| Short-commit collisions | Commit dirs use 7-char shorts for readability, but the full hash in the manifest is always cross-checked. A genuine collision is a loud `Error::CommitCollision`, never silent dedup. |
| Forward compatibility | Manifests and the store marker carry schema versions. Data written by a newer crate version is never overwritten (`Error::UnsupportedSchema`). |

## Cache flow

```rust,ignore
use apvm_storage::{ArtifactStore, LookupRequest, LookupResult, VersionMatch};

let store = ArtifactStore::new(builds_cache_dir);

// Releases resolve to a tag, not a commit — check the releases cache:
if let Some(release) = store.find_release("backwpup", "v5.6.0")? {
    return Ok(release.files); // verified cache hit, no download
}

// Everything else resolves to a commit — check the build cache:
let request = LookupRequest::new("backwpup", &commit_sha)
    .version("5.6.0")
    .version_match(VersionMatch::Lenient) // or Strict, see below
    .require_variants(&requested_variants);

match store.lookup_build(&request)? {
    LookupResult::Hit(hit) => {
        if !hit.version_matched {
            // Lenient match: same commit, different stamped version.
        }
        Ok(hit.build.files)
    }
    LookupResult::Miss(reason) => {
        // reason: NotCached | VersionMismatch { available } |
        //         MissingVariants { missing } | Incomplete
        let output = build_it()?;
        store.store(&output.to_source_artifacts(), &output.to_build_metadata("backwpup"))?;
        // ...
    }
}
```

### Strict vs lenient version matching

Version-aware plugins (e.g. BackWPup) bake the requested version string into
the artifact. The same commit built as `5.6.0` and as `5.7.0` differs only by
that stamp — usually cosmetic, occasionally load-bearing (upgrade notices,
data migrations). `VersionMatch` lets the caller decide per request:

- **`Strict`** — only an exact-version build is a hit. A same-commit build
  under another version misses with `VersionMismatch { available }`, telling
  the caller precisely why a rebuild is happening.
- **`Lenient`** — any healthy build of the commit is a hit;
  `LookupHit::version_matched` reports whether the version also matched.

Plugins that are not version-aware should use `Lenient`.

### Releases cache

GitHub Releases don't reliably expose their underlying commit, so cached
releases are keyed by tag in a separate `releases/` subtree, with the same
guarantees (`store_release`, `find_release`, `list_releases`,
`delete_release`). Moving-target keywords (`latest-stable`, …) must be
resolved to a concrete tag against the GitHub API *before* consulting the
cache — they are deliberately never cached.

## Module map

| Module | Responsibility |
|---|---|
| `store` | `ArtifactStore`: store/find/list/delete builds, locking, store marker |
| `release` | Releases cache: manifests + store/find/list/delete by tag |
| `lookup` | Version-aware cache lookup (`Strict`/`Lenient`, required variants, miss reasons) |
| `query` | Fluent inventory queries (project/version/source/commit filters) |
| `manifest` | Schema-versioned JSON manifests, atomic save, invariant checks |
| `verify` | Integrity checks (presence / size / checksum) |
| `path` | Single source of truth for the on-disk layout + name sanitization |
| `validate` | Input validation for everything that becomes a path component |
| `link` | Cross-platform directory links (symlink / junction) |
| `fsx` | Atomic writes, streamed hashing copies, temp management, store lock |
| `error` | Contextual, `#[non_exhaustive]` error type |

## Testing

```bash
cargo test -p apvm-storage
```

The suite covers crash-consistency behaviors (no temp leftovers, corrupted
manifest recovery), concurrency (parallel stores to one commit), collision
detection, sanitization round-trips, integrity misses, and the full
strict/lenient lookup matrix.
