# Git reference formats for `apvm build`

The `<GIT_REF>` argument accepts many formats. APVM **auto-detects** the type
or accepts an **explicit prefix** for disambiguation. This file mirrors the
`apvm build --help` long-help text (`crates/cli/src/commands/build.rs`,
`after_long_help`).

## Automatic detection (no prefix)

| Input     | Resolved as                                                |
|-----------|------------------------------------------------------------|
| `123`     | PR #123 (or branch if PR doesn't exist)                    |
| `#123`    | PR #123 (`#` stripped by the CLI)                          |
| `develop` | Branch name                                                |
| `v1.0.0`  | Tag (if exists) or branch                                  |
| `abc1234` | Commit SHA (7–40 hex characters)                           |
| `5.6.8`   | Version → tries GitHub Release first, then tag/branch     |

The CLI normalizes `#123` → `123` before resolution (`#` is only stripped
when followed by digits, e.g. `#abc` is **not** treated as a PR).

## Explicit prefixes

Use these when the auto-detected type isn't what you want (e.g., you have a
branch named `v1.0.0` and want the tag instead).

| Prefix      | Example              | Description                        |
|-------------|----------------------|------------------------------------|
| `pr:`       | `pr:123`             | Force PR interpretation            |
| `branch:`   | `branch:main`        | Force branch interpretation        |
| `tag:`      | `tag:v1.0.0`         | Force tag interpretation           |
| `commit:`   | `commit:abc1234`     | Force commit interpretation        |
| `release:`  | `release:v5.6.8`     | Download pre-built GitHub Release assets (skip the build) |

## Special keyword refs — tags

Tags are sorted by **creation date** (`git tag --sort=-creatordate`).

### Stable (excludes `-alpha`, `-beta`, `-rc`)

| Keyword                | Resolves to                                |
|------------------------|--------------------------------------------|
| `tag:latest-stable`    | Latest stable tag                          |
| `tag:previous-stable`  | Previous stable tag                        |

### Any (includes prereleases)

| Keyword                | Resolves to                                |
|------------------------|--------------------------------------------|
| `tag:latest`           | Very latest tag (any kind)                 |
| `tag:previous-latest`  | Tag right before the latest                |

## Special keyword refs — releases

Fetched from the **GitHub Releases API**. **Drafts are always excluded.**

### Stable (excludes prereleases)

| Keyword                    | Resolves to                                |
|----------------------------|--------------------------------------------|
| `release:latest-stable`    | Latest stable release (non-prerelease, non-draft) |
| `release:previous-stable`  | Previous stable release                    |

### Any (includes prereleases)

| Keyword                    | Resolves to                                |
|----------------------------|--------------------------------------------|
| `release:latest`           | Very latest non-draft release              |
| `release:previous-latest`  | Previous non-draft release                 |

## Practical examples

```sh
# Auto-detected types
apvm build backwpup 123 -v 5.1.0             # PR
apvm build backwpup "#456" -v 5.1.0          # PR via '#'
apvm build backwpup develop -v 5.1.0         # branch
apvm build backwpup v1.0.0 -v 5.1.0          # tag (or branch if tag missing)
apvm build backwpup abc1234 -v 5.1.0         # commit
apvm build backwpup 5.6.8                    # version → release first, then tag/branch

# Explicit prefixes (disambiguation)
apvm build backwpup pr:789 -v 5.1.0
apvm build backwpup branch:main -v 5.1.0
apvm build backwpup tag:v2.0.0 -v 5.1.0
apvm build backwpup commit:3fb4102 -v 5.1.0
apvm build backwpup release:5.6.8            # download pre-built zip (no build tools)

# Tag keyword refs
apvm build backwpup tag:latest-stable -v 5.1.0
apvm build backwpup tag:previous-stable -v 5.1.0
apvm build backwpup tag:latest -v 5.1.0
apvm build backwpup tag:previous-latest -v 5.1.0

# Release keyword refs (BackWPup supports releases)
apvm build backwpup release:latest-stable
apvm build backwpup release:previous-stable
apvm build backwpup release:latest
apvm build backwpup release:previous-latest
```

## Plugin support matrix

| Plugin     | `tag:*` | `release:*` |
|------------|---------|-------------|
| `backwpup` | ✅      | ✅ (downloads pre-built assets) |
| `wp-rocket`| ✅      | ❌ (no pre-built assets published) |
| `imagify`  | ✅      | ❌ (publishes tags but no downloadable assets — use `tag:` instead) |

## How resolution is reported

After the reference is resolved, the CLI prints a `Reference:` line in the
header:

```
Building backwpup from develop
  Version: 5.1.0
  Output: /current/dir
  Reference: branch 'develop' @ 3fb4102
```

For PRs it adds the source branch:

```
  Reference: PR #123 (branch: feature/x) @ abc1234
```

For releases (no commit yet):

```
  Reference: release 'v2.3.0'
```

For commits, the SHA is already in the description, so no `@ <short>` suffix
is appended:

```
  Reference: commit a1b2c3d
```