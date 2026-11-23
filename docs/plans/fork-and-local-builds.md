# Fork PR builds and local-repository builds

Status: **planned** (2026-11-10; revised after adversarial audit, §8) · Target: **3.3.1** (Track A, bug fix) and **3.4.0** (Track B, feature; shares 3.4.0 with [site-deployment](site-deployment.md)) · Author: Sandy Figueroa

---

## TL;DR

| | |
|---|---|
| **Problem** | PRs from forks cannot be built: apvm checks out the PR's *branch name* on the base repo, and a fork's branch is not there. Worse, when a fork's branch has the same name as a base branch (Imagify PR #1177, fork branch `develop`), apvm **silently builds the base branch** and would cache it as that PR. Merged PRs whose branch was deleted fail too. |
| **Fix (Track A)** | Build every PR from GitHub's `refs/pull/<N>/head`, which exists on the base repo for every PR (fork or not, open, closed or merged), pinned to the PR's head SHA and checked after checkout. No new option; `apvm build wp-rocket pr:9172` just works. |
| **Feature (Track B)** | `apvm build <PLUGIN> <GIT_REF> [OUTPUT] --repo <DIR> [--uncommitted]`: build from a local clone. `GIT_REF` is resolved in that clone (`HEAD`, a branch, a tag, a SHA, `origin/x`). `--uncommitted` adds staged, unstaged and untracked (not ignored) changes on top of `HEAD`. The user's repo is only read; the build runs in a temp clone. Local builds are never cached. NAPI: `repoPath`, `includeUncommitted`. |
| **Deps** | None new. Rust 1.97 / edition 2024, existing workspace crates (tokio 1, tempfile 3, clap 4, octocrab 0.54, napi 3.14.2). System `git` (tested on 2.56). |
| **Fork safety** | Building runs the repo's npm/composer scripts on your machine. Registry repos are trusted; a **fork PR** asks `[y/N]` first (default No), showing the fork and commit. Approval is tied to that exact commit. Non-interactive: refuse unless `--trust-fork`. NAPI rejects with `ForkNotApproved` unless `approvedForkCommit`/`trustFork` is set. GitHub tokens are removed from the environment of fork builds. |
| **Phases** | A: P1.1, P1.2 → P1.3 (trust gate) → P1.4, P1.5 → P1.6 (checkout fix) → P1.7 (release 3.3.1). B: P2.1, P2.2, P2.3 (parallel) → P2.4 → P2.5 → P2.6, P2.7 (parallel) → P3 (release 3.4.0). P1.x and P2.1–P2.3 can run in parallel; P2.4 starts after P1.6 (both edit `run_clone_build`). |

---

## 1. Verified facts

| # | Fact | How verified |
|---|---|---|
| V1 | `checkout_resolved` checks out a PR with `git checkout -B <head_branch> refs/remotes/origin/<head_branch>` (`crates/core/src/commands/build.rs:590`). Only `git fetch --all --prune` runs before it, so `refs/pull/*` are never fetched. | read |
| V2 | Fork PR fails: `apvm build wp-rocket pr:9172` (fork `jay23606:pr-go/9171-…`) → `fatal: invalid reference: refs/remotes/origin/pr-go/9171-…`, after the pre-clone fast path already resolved head `c45bc8f`. | ran (isolated `HOME`/`APVM_CACHE_DIR`) |
| V3 | **Silent wrong build:** `apvm build imagify pr:1177` (fork `mauroTabsSpaces:develop`, head `a219de9`, v2.2.9) printed `Reference: PR #1177 (branch: develop) @ a219de9`, then checked out origin's `develop` (`f31d733`) and built **v2.3.5**. It only failed because of the known upstream Imagify build issue; otherwise the wrong zip would have been delivered and stored under source "PR #1177". Nothing compares HEAD after checkout with the resolved SHA. | ran + debug log |
| V4 | `refs/pull/<N>/head` exists on the base repo and equals the API's `head.sha`: open fork PR 9172 (`c45bc8f`), closed fork PR imagify 1177 (`a219de9`), merged fork PR 9155, merged same-repo PR 9187 whose branch `enhancement/9181-…` is **deleted** (so today it fails too). | `git ls-remote` + `gh pr view` |
| V5 | octocrab 0.54.3 `pulls::Head` has `label: Option<String>` (`owner:branch`), `ref_field`, `sha`, `repo: Option<Repository>` (`None` when the fork was deleted). | source in `~/.cargo/registry` |
| V6 | A local `git clone <path>` copies the whole object store: a commit reachable only from a detached HEAD, with or without `--no-hardlinks`, and from a linked worktree, is present in the clone. | ran (scratch repos) |
| V7 | `git rev-parse --show-toplevel` from a subdirectory or linked worktree returns the work-tree root; `git diff-index -p --binary … HEAD` + `git apply` in the clone reproduces staged + unstaged edits (porcelain `git diff` is **not** usable: it refreshes and rewrites the user's `.git/index` even with `GIT_OPTIONAL_LOCKS=0`, and honors `diff.noprefix`, which breaks `git apply`); `git ls-files -z --others --exclude-standard` lists untracked, non-ignored files. | ran |
| V8 | Builders run no `git` commands themselves (they only exclude `.git` when zipping), but the plugins' own build scripts are opaque, so the build tree must stay a real git checkout. | grep `crates/core/src/build/plugins` |
| V9 | `Repository::clone` runs `git clone <url> <path>` with no `--`. Safe for registry URLs; a local path must be passed absolute (starts with `/` or a drive letter) so it can never look like an option or an scp-style `host:path`. | read |
| V11 | `BuildWorkspace` names the clone dir after the last `/` segment of its URL (`git/workspace.rs` `repo_name_from_url`); WP Rocket splices that path **unquoted** into shell commands (`rsync -av {repo_dir}/ …`, `wpr.rs`). Safe today (registry names only); a user's dir name (`wp rocket`, `x;cmd`) would break or inject. A Windows `\` path isn't split at all. | read |
| V12 | WP Rocket zips with `zip -r` without `-y`, so a symlink in the tree is stored as its **target's content**. | read `wpr.rs` |
| V13 | `git rev-parse --is-bare-repository --show-toplevel` exits 128 in a bare repo; `--is-bare-repository --is-inside-work-tree` exits 0 and prints `true`/`false`. | ran |
| V14 | Build commands run as `sh -c <cmd>` (`cmd /C` on Windows) in the repo dir with apvm's full environment; nothing is removed (`build/runner.rs` `run`; no `.env*` calls in `build/`). A repo's `composer`/`npm` scripts therefore run as the user and see e.g. an exported `GITHUB_TOKEN`. Not reachable for forks today only because fork PRs fail (V2). | read |
| V15 | The CLI already has two identical y/N helpers (`confirm` + `is_yes`/`is_affirmative` in `commands/cache.rs` and `commands/uninstall.rs`; only `y`/`yes` mean yes) and injects the answer in tests (`Console::ask`). | read |
| V10 | `RefSource` is converted totally into `apvm_storage::BuildSource` (DB `kind` column); an unknown `kind` is read back as a corrupt row. Adding a `RefSource` variant therefore touches storage and older binaries. | read `storage/src/types.rs` |

---

## 2. Decisions (recommended; change before implementing if you disagree)

| # | Decision | Why | Alternatives rejected |
|---|---|---|---|
| D1 | Fork support is a **fix of `pr:`**, not a new option. | The PR number already identifies the code; `refs/pull/N/head` makes it fetchable. | `--fork owner/repo` (more typing, needs fork access, breaks for deleted forks). |
| D2 | Every PR (fork or same-repo) checks out `refs/pull/N/head`, detached at the API's head SHA; HEAD is verified after checkout. | One code path; also fixes deleted branches (V4) and the race where the branch moved after the API call. | Fork-only special case (keeps the deleted-branch bug, two paths to test). |
| D3 | Local flag is **`--repo <DIR>`** (NAPI `repoPath`). `GIT_REF` stays a required positional, resolved in that repo. | Clear next to `[OUTPUT]` (a "path"); no positional ambiguity; reuses ref syntax. | `--path` (confusable with output); `local:<dir>` ref prefix (no room for a ref, `@`/`:` parsing in paths); optional `GIT_REF` (clap can't disambiguate `GIT_REF` vs `OUTPUT`). |
| D4 | **Local builds never use the cache** (no read, no write). `--repo` + `--warm-cache` is an error; `--no-cache` is accepted and redundant. | A path is not a stable identity; uncommitted trees are not reproducible; no storage/schema change (V10). | Cache clean local commits under `commit:<sha>` (possible later, §7). |
| D5 | Local builds report the ref as `Branch`/`Tag`/`Commit` (existing `RefSource`) plus a new `BuildOutput::local: Option<LocalOrigin>`. | No new `RefSource` variant → no storage impact (V10). | `RefSource::Local` (storage kind, old-binary compat). |
| D6 | `--uncommitted` only on top of `HEAD`: `GIT_REF` must resolve to the repo's `HEAD` commit, else error. | The working-tree diff is relative to `HEAD`; applying it to another commit is guesswork. | Apply to any ref (conflicts, misleading results). |
| D7 | The user's repo is **read-only** to apvm: only plumbing that never writes: `rev-parse`, `diff-index`, `ls-files`, `cat-file`, `clone` (source side), all with `GIT_OPTIONAL_LOCKS=0`. Never porcelain `diff`/`status` (they refresh the index, V7). | A build tool must never touch a dev's checkout or index. | Building in place / `git worktree add` (writes to the user's repo). |
| D8 | Ship Track A as **3.3.1** before Track B. | It is a correctness bug (V3). It does add `--trust-fork` and two NAPI options; strict SemVer would call that a minor release (**your call**, see §9). | Bundle into 3.4.0 (delays the fix). |
| D9 | **Fork PRs need explicit approval** before anything runs; registry repos (incl. same-repo PRs) are trusted. Approval = full commit SHA, so a push after the prompt is never built unapproved. Gate lives in core, so CLI, NAPI, build and warm can't bypass it. | Build scripts run with the user's permissions (V14). | Prompt only in the CLI (NAPI/warm would bypass it); approve by PR number (a later push would ride on an old approval). |

---

## 3. Track A design — PRs via `refs/pull/N/head`

**Flow change** (inside `run_clone_build`, after the existing clone/fetch/reset):

```
PR:  fetch +refs/pull/N/head:refs/apvm/pull/N   (private namespace: refs/remotes/origin/pull/N collides with an origin branch named `pull`; token-aware)
     target = head_sha if present in the clone; else same-repo PR → refs/apvm/pull/N (+ Warning: "PR #N moved since it was resolved; building its current head <sha7>"), fork PR → error
     git checkout --detach <target>
     verify HEAD == target sha   (else Error::Git — never build a lookalike)
```

- **Model** (`github/models.rs`): `PullRequest` gains `head_label: Option<String>` (`owner:branch`) and `is_cross_repository: bool`, computed from repo **ids** (`head.repo.id` ≠ `base.repo.id`; names are case-sensitive and change on renames). `head.repo` or `base.repo` missing (deleted fork; both are `Option` in octocrab) → cross-repo.
- **Display**: `ResolvedRef::from_pull_request` keeps `git_ref = head_branch` for same-repo PRs; for cross-repo PRs `git_ref = head_label` (fallback `head_branch`). `detailed_description()` → `PR #1177 (fork: mauroTabsSpaces:develop)` vs `PR #9187 (branch: enhancement/…)`. `branch_label` (storage `branch` metadata) follows `git_ref`. `git_ref` is no longer used to check out a PR (doc comment updated).
- **Cache**: unchanged keys (commit + version). The fast path already keys on `head_sha` (V2/V3 logs); post-checkout, the verified HEAD equals it, so the stored commit is now always right.
- **Old cache rows:** a same-name fork PR built before the fix may be stored under the base branch's commit with source "PR #N". Lookups key on commit, so it is never served for the PR; mention it in the 3.3.1 notes (`apvm cache clean --project <p>` removes it).
- **Private repos** (BackWPup): same fetch through the token-aware path; forking is disabled there (`allow_forking: false`) but deleted-branch PRs benefit.

### 3.1 Fork trust gate

Building runs the repo's own scripts (`composer install`, `npm` scripts, `bin/build-zip.sh`) on the user's machine with their permissions (V14). Registry repos are trusted; code from a fork PR is not. So:

```
resolve ref → fork PR?  no  → continue as today
                        yes → approval == Commit(resolved sha) or Any ? continue (tokens stripped)
                                                                     : Error::ForkNotApproved{pr, fork, sha}   (nothing cloned or run)
```

CLI prompt (stderr, default **No**):

```
⚠  PR #9172 comes from a fork (jay23606/wp-rocket), not from wp-media/wp-rocket.
   Building it runs that fork's npm/composer scripts on this machine, with your
   permissions (files, SSH keys, tokens). Continue only if you trust this code.
   Commit: c45bc8f
Build it? [y/N]
```

- Checked before the cache, for build and `--warm-cache` alike (a cache hit of an earlier approved build still asks: simple and predictable).
- Fail closed: unknown fork status, non-interactive stdin, or a PR that moved after approval → no build.
- **Local builds (`--repo`) are not gated:** the user chose the folder, and apvm can't reliably tell whether its code came from a fork (e.g. after `gh pr checkout`). The `--repo` docs and help say that building runs that folder's scripts.

## 4. Track B design — `--repo <DIR>`

```
validate request (project, platform, version, --uncommitted needs --repo, --repo excludes warm)
LocalRepo::open(dir)            → absolute work-tree root (rev-parse, two calls: V13)
LocalRepo::resolve(git_ref)     → LocalRef { source: Branch|Tag|Commit, sha }
[--uncommitted] check sha == HEAD; LocalRepo::snapshot() → Overlay { patch, untracked }
BuildWorkspace::with_repo_name(name, <root as UTF-8>, project.repo, token=None) → clone (V6); clone dir named after the REGISTRY repo, never the user's dir (V11)
checkout --detach <sha>; verify HEAD
[--uncommitted] overlay.apply(clone)
identity check (builder's identity files exist)          → else "not a <project> repository"
resolve_version → optional version override → execute_build → collect to OUTPUT
BuildOutput { local: Some(LocalOrigin { repo_dir, uncommitted }), … }   (no cache I/O)
```

**Ref resolution in the local repo** (`LocalRepo::resolve`):

| Input | Result |
|---|---|
| `branch:X` | `refs/heads/X` must exist → `Branch(X)` |
| `tag:X` | `refs/tags/X` → `Tag(X)` |
| `commit:X` | `X^{commit}` → `Commit(full sha)` |
| bare `X` | tag `refs/tags/X` → `Tag`; else `refs/heads/X` → `Branch`; else `X^{commit}` (covers `HEAD`, `HEAD~2`, `origin/x`, SHAs) → `Commit(full)` |
| `pr:`, `release:`, tag/release keywords, empty, leading `-`, contains NUL | error with hint: "`--repo` builds a local ref; for a PR use `pr:N` without `--repo` (forks work), or `gh pr checkout N` then `--repo`" |

All lookups: `git rev-parse --verify --quiet <fully-qualified-ref>^{commit}`.

**Uncommitted overlay** (`Overlay`):
- `patch` = `git diff-index -p --binary --no-ext-diff --no-textconv --no-color HEAD` (plumbing: no index refresh, ignores `diff.noprefix`/`srcPrefix`; staged + unstaged, incl. deletes, mode changes, binaries). Stat-only changes give an empty hunk and are ignored. Applied with `git apply --whitespace=nowarn` in the clone; failure → error (no partial build).
- `untracked` = `git ls-files -z --others --exclude-standard`; each path must be relative, without `..`, and resolve inside the root. Regular files copied with permissions; directories created as needed. **Untracked symlinks are skipped** with a `Warning` (zipping follows them, V12: `auth.json -> ~/.composer/auth.json` would leak). Entries that are directories (an untracked nested git repo shows as `nested/`) are skipped with a `Warning`.
- Report: one `BuildEvent::Warning` "Building with uncommitted changes: N modified, M untracked files (X MB); this build is not reproducible and is not cached". Above 5,000 untracked files or 200 MB, the warning adds "check your .gitignore". (Not an error: the user asked for it.)

**Identity check**: new `Builder::identity_files(&self) -> Vec<&'static str>`, default = `version_source_files()`; BackWPup overrides with `backwpup.php` (root of `backwpup-pro`). An **empty list is an error** (`Error::LocalRepo`: "builder declares no identity files"), so a future builder can't silently skip the check. Checked in the clone after checkout/overlay. Prevents building `wp-rocket` from an Imagify clone, or from a parent repo that `--repo` resolved to (a plain dir inside a `$HOME` dotfiles repo resolves to that repo's root); the CLI also prints the resolved root.

**Partial/blobless user clones**: checkout in the temp clone can fail for missing blobs → map to `Error::LocalRepo` with "repository is a partial clone; run `git fetch --refetch` or use a full clone".

**Errors**: reuse `Error::Git` for git failures; add one variant `Error::LocalRepo { path: PathBuf, reason: String }` for validation (not a dir, not a git work tree, bare repo, non-UTF-8 path, identity mismatch, `--uncommitted` ref ≠ HEAD, partial clone). The napi crate's `core_error_status` (`crates/napi/src/error.rs`) matches every core error exhaustively, so the variant and its `Status::InvalidArg` mapping land together (P2.1). `crates/core/src/error.rs` has no generic invalid-input variant today (`RepositoryNotFound(PathBuf)` exists but means "no repo" only — reuse it for that case).

---

## 5. Phases

Effort: XS < 1 h · S ≈ half day · M ≈ 1 day · L ≈ 2–3 days. Every phase ends with the full validation suite from `CLAUDE.md` (fmt, clippy `-D warnings`, test, doc), run with `APVM_CACHE_DIR` and `HOME` pointed at temp dirs for any CLI exercise.

### Track A — fork / deleted-branch PRs

#### P1.1 — Fetch a PR head ref · S · easy · Depends: — · Parallel: P2.1–P2.3
- **Goal:** `Repository::fetch_pull_head(&self, number: u64, token: Option<&str>) -> Result<()>` fetching `+refs/pull/<N>/head:refs/apvm/pull/<N>`; `BuildWorkspace::fetch_pull_request(n)` passes the workspace token (HTTPS only, same rule as `fetch`).
- **How:** generalize the existing token swap in `fetch_with_token` into a private helper `with_token_remote(token, || git fetch origin <refspec>)` so the URL is **always** restored (also on error) and stderr is **always** redacted (`replace(token, "[REDACTED]")`, as `clone_with_token` does; `fetch_with_token` doesn't today — fix it through the helper). Do not duplicate the swap.
- **Edge cases:** PR number `0` → error; missing ref → `Error::Git("PR #N head not found on origin")`; token never appears in errors/logs (existing tests cover `fetch_with_token`; add the same for the new path).
- **Tests (`git/testutil.rs` + `repository.rs`):** bare `file://` remote with `git update-ref refs/pull/7/head <sha>` of a commit on no branch; fetch succeeds and `refs/apvm/pull/7` = sha; origin has a branch named `pull` → still succeeds (regression for the namespace choice); missing PR → error; re-fetch after the ref moved updates it (`+`); token path tested with the hermetic `insteadOf` pattern already used in `repository.rs` tests (a `file://` remote never takes the token path): token absent from error text and from the remote URL afterwards.
- **Done:** tests green; no behavior change elsewhere.

#### P1.2 — PR model: fork awareness · S · easy · Depends: — · Parallel: P1.1
- **Goal:** `PullRequest { head_label, head_repo_full_name, is_cross_repository }` (V5) filled in `GitHubClient::get_pull_request`; `ResolvedRef` gains `fork: Option<ForkInfo { head_repo: Option<String>, head_label: Option<String> }>` (set by `from_pull_request` for cross-repo PRs; the trust gate in P1.3 reads it); `detailed_description` per §3. `ResolvedRef` has ~41 struct literals (mostly tests): add `fork: None` mechanically (no assertion changes).
- **Edge cases:** deleted fork (`head.repo == None`) → cross-repo, label may still be present; missing label → fall back to `head_branch`.
- **Tests:** pure unit tests on a `cross_repository(head_repo_id: Option<u64>, base_repo_id: Option<u64>) -> bool` helper (extract it so it's testable without octocrab structs); `from_pull_request` for same-repo/fork/deleted-fork; description strings. Update existing `from_pull_request_*` tests' fixture builder for the new fields (fixture change only, not assertions).
- **Docs (same change, `CLAUDE.md` rule — the `Reference:` line changes):** `SKILL.md`, `crates/cli/README.md` (fork label), NAPI `JsResolvedRef.git_ref` doc (`owner:branch` for forks).
- **Done:** tests green; CLI shows the fork label in `Reference:` with no other change.

#### P1.3 — Fork trust gate (core) · M · medium · Depends: P1.2 · Parallel: P1.1
- **Goal:** core refuses to build or warm a fork PR unless the caller approved it (§3.1). Lands **before** P1.6, so fork code can never run on `develop` without the gate.
- **How:** `ForkApproval { Deny (default), Commit(String), Any }` on `BuildRequest` and `WarmRequest` (chainable setter `.fork_approval(..)`). Pure fn `ensure_fork_approved(resolved: &ResolvedRef, approval: &ForkApproval) -> Result<()>`, called in `BuildCommand::execute` right after `ReferenceResolved` is reported and before the cache fast path (so build and warm share it). New errors `Error::ForkNotApproved { pr, head_repo, head_label, commit }` and `Error::ForkApprovalStale { pr, approved, current }`, plus their `Status` arms in napi `core_error_status` (exhaustive match). `Commit(sha)` matches the full resolved SHA only; `head_sha` unknown + `Commit(_)` → stale (can't bind the approval). Fork builds also run with `GITHUB_TOKEN` and `GH_TOKEN` removed from the build commands' environment (`BuildRunner::without_env`), because `runner.rs` passes apvm's whole environment to `sh -c` today (V14).
- **Edge cases:** same-repo PR → no gate; deleted fork / unknown repo ids → fork (P1.2 fails closed); `Any` → never asks; branch/tag/commit refs → no gate (they're on the trusted repo).
- **Tests:** table test of `ensure_fork_approved` (same-repo × each approval, fork × Deny/Commit(match)/Commit(other)/Commit(short prefix)/Any, fork with no head SHA); runner test proving `GITHUB_TOKEN`/`GH_TOKEN` aren't visible to a command (`sh -c 'test -z "$GITHUB_TOKEN"'`) when stripped and are visible when not; napi status mapping tests.
- **Docs (same change):** `crates/core/README.md` (`ForkApproval`, errors).

#### P1.4 — CLI y/N prompt · S · easy · Depends: P1.3 · Parallel: P1.5
- **Goal:** `apvm build`/`--warm-cache` of a fork PR asks before building (§3.1 message); new flag `--trust-fork` skips the question (scripts/CI).
- **How:** run with `Deny`; on `ForkNotApproved` stop the spinner, print the warning, ask, and on **y** re-run with `Commit(sha)` without reprinting the header. Stdin not a terminal → no prompt, fail with "PR #N is from a fork; re-run with --trust-fork to build it" (exit ≠ 0). On `ForkApprovalStale` → error "PR #N changed since you approved it (abc1234 → def5678); run the command again". Move the two duplicated `confirm`/`is_yes` helpers (`commands/cache.rs`, `commands/uninstall.rs`) into one `crates/cli/src/prompt.rs` and reuse it (behavior unchanged; existing tests move with it).
- **Tests:** prompt text unit test; injected-answer tests (y / yes / n / empty / EOF → only y and yes build); non-TTY path; `--trust-fork` → no prompt; clap: `--trust-fork` accepted with `--warm-cache`.
- **Docs (same change):** `SKILL.md` (flag, prompt, non-interactive error in "Common error patterns"), `crates/cli/README.md`.

#### P1.5 — NAPI approval option · S · medium · Depends: P1.3 · Parallel: P1.4
- **Goal:** Node can't prompt, so a fork PR **rejects** by default with `err.code === 'ForkNotApproved'` and `err.prNumber`, `err.headRepo`, `err.commit`; the caller retries with `approvedForkCommit: '<sha>'` (bound to that commit) or `trustFork: true` (any commit). Both on `BuildOptions` and `WarmOptions`; both set → `InvalidArg`. Stale → `err.code === 'ForkApprovalStale'`.
- **How:** reuse the custom-code mechanism of `ApvmCache` (`crates/napi/src/cache.rs`, `spawn_future_with_callback` + `create_error`). `build`/`warmCache` currently use `env.spawn_future(call.run(..))`; start with a 30-min spike proving a custom code + extra properties reach JS on that path before writing the rest.
- **Tests:** Rust option-mapping tests; Node: both options set → `InvalidArg`; error shape documented in `crates/napi/README.md` with a retry example (a real fork build isn't run in the Node suite).

#### P1.6 — Check out PRs from `refs/pull` · M · medium · Depends: P1.1–P1.5
- **Goal:** `checkout_resolved` for `RefSource::PullRequest(n)`: `fetch_pull_request(n)` → pick target (`head_sha` if `git cat-file -e <sha>^{commit}` succeeds, else the fetched ref + `Warning`) → `checkout_detached(target)` → `verify_head(expected)`.
- **How:** `checkout_resolved` needs the workspace (for the token) — pass `&BuildWorkspace` instead of `&Repository`, or add a `pr_fetcher` argument; keep the function small by extracting `checkout_pull_request(workspace, n, head_sha, reporter)`. Add `Repository::has_commit(sha)` and `Repository::verify_head(expected_sha)` (returns `Error::Git("checked out <a> but expected <b>")`).
- **Fixtures:** the two existing tests calling `checkout_resolved(&repo, …)` (`commands/build.rs`, `unverified_commit` cases) are updated for the new signature (argument change only, assertions untouched).
- **Also:** apply `verify_head` to `Commit` checkouts (cheap, same guarantee). Branch/tag checkouts are not pinned (their SHA can legitimately move between resolve and fetch) — leave them.
- **Risk:** this changes every PR build. Mitigate with the tests below plus the manual checks in Done.
- **Tests:** unit tests of `checkout_pull_request` **and of the `checkout_resolved` PR arm** (constructed `ResolvedRef`; `execute` can't be tested hermetically because `BuildCommand` holds a concrete `GitHubClient`) against a `file://` remote: (a) fork-like PR whose ref points at a commit on no branch; (b) PR whose head branch name equals an origin branch (`develop`) → builds the PR commit, not origin's (regression for V3); (c) `head_sha` absent from remote (force-pushed) → same-repo PR: fetched tip + one `Warning`; **fork PR: error** (the approval is bound to that SHA; never build an unapproved commit); (d) `head_sha = None` (defensive; octocrab's `sha` is required) → fetched tip, no warning; (e) `verify_head` mismatch → error.
- **Docs (same change):** `SKILL.md` + `references/git-refs.md` PR row ("forks, closed and merged PRs supported; built from `refs/pull/N/head`"), `crates/cli/README.md`.
- **Done:** tests green; manual, with isolated `HOME`/`APVM_CACHE_DIR`: `apvm build wp-rocket pr:9172 <tmp>` succeeds and `Reference:`/commit = `c45bc8f`; `apvm build wp-rocket pr:9187 <tmp>` (deleted branch) succeeds; one same-repo open PR still builds; `apvm build imagify pr:1177` checks out `a219de9` (debug log `Checked out … at commit a219de9`; the build itself may fail upstream — see memory note on imagify#1289).

#### P1.7 — Release 3.3.1 · XS · easy · Depends: P1.6
- **Goal:** version bump to 3.3.1 per the established checklist (Cargo.toml, Cargo.lock workspace crates, package.json, package-lock.json root fields, SKILL.md version line; the skill version test enforces it).
- **Release notes** (hand-written, no co-author) must say: fork / deleted-branch PRs now build; `resolvedRef.gitRef` is `owner:branch` for fork PRs; old mislabeled cache rows (§3).
- **Done:** validation suite + `npm test` green.

### Track B — local repository builds

#### P2.1 — `LocalRepo::open` · S · easy · Depends: — · Parallel: P1.x, P2.2, P2.3
- **Goal:** new `crates/core/src/git/local.rs`: `LocalRepo { root: PathBuf }`, `open(path) -> Result<Self>`.
- **How:** `std::path::absolute` (not `canonicalize`: avoids Windows `\\?\` paths that git mishandles) → must be a directory → `git -C <abs> rev-parse --is-bare-repository --is-inside-work-tree` (non-zero exit → not a repo; `true …` → bare; `… false` → e.g. inside `.git`) → `git -C <abs> rev-parse --show-toplevel` → `root`; all with env `GIT_OPTIONAL_LOCKS=0`; root must be valid UTF-8 (it becomes the clone "URL"; error otherwise).
- **Tests:** dir OK; subdir → root; linked worktree → worktree root; file path, missing path, non-repo dir, bare repo → `Error::LocalRepo` with the right reason.
- **Also:** add `Error::LocalRepo` here, plus its `Status::InvalidArg` arm in `crates/napi/src/error.rs` `core_error_status` and a test there (the match is exhaustive: without it the workspace doesn't compile).
- **Done:** module compiled, `pub use` from `git/mod.rs`.

#### P2.2 — `LocalRepo::resolve` · S · easy · Depends: P2.1 (same file; can be developed in parallel against a stub) · Parallel: P2.3
- **Goal:** the table in §4 → `LocalRef { source: RefSource, sha: String }`.
- **How:** reuse the prefix parsing ideas from `RefResolver` but not its code paths (they hit GitHub). Keep a pure `classify_local_input(input) -> Result<LocalInput>` (prefix/keyword/validation, unit-testable without git) + an async `lookup(LocalInput)`.
- **Tests:** each table row; ambiguous name that is both tag and branch → `Tag` (documented); `HEAD` on detached HEAD; `origin/x`; short SHA expands; unknown ref → error naming the repo; `pr:1`, `release:x`, `tag:latest-stable`, `-x`, `""` → errors with the hint.

#### P2.3 — Uncommitted overlay · M · medium · Depends: P2.1 · Parallel: P2.2
- **Goal:** `LocalRepo::snapshot() -> Result<Overlay>`, `Overlay::apply(&self, clone_root) -> Result<OverlayStats>`, `OverlayStats { modified, untracked, bytes }`.
- **How:** §4. Keep path validation (`safe_relative(path) -> Option<&Path>`) and stats as pure functions. All git calls with `GIT_OPTIONAL_LOCKS=0`.
- **Tests:** modified, staged, deleted, renamed, mode change (Unix), binary file, untracked file in a new nested dir, ignored file excluded, untracked symlink skipped + warning, untracked nested git repo skipped + warning, `diff.noprefix=true` in an isolated `GIT_CONFIG_GLOBAL` still applies, clean tree → empty overlay; **source repo untouched**: `.git/index` bytes and mtime and `git status --porcelain` identical before/after `snapshot()`, including after `touch` on a tracked file (the case porcelain `git diff` would rewrite).

#### P2.4 — Core pipeline · L · medium-hard · Depends: P1.6, P2.1–P2.3
- **Goal:** `LocalSource { repo_dir: PathBuf, uncommitted: bool }`; `BuildRequest::local(Option<LocalSource>)` (new field defaulting to `None`); `BuildOutput::local: Option<LocalOrigin { repo_dir, uncommitted }>` (set `None` in every existing `commands::BuildOutput` literal); `Builder::identity_files` (+ BackWPup override); `BuildWorkspace::with_repo_name(namespace, source, repo_name, token)` (V11; `new` delegates to it with the URL-derived name); new `crates/core/src/commands/local_build.rs` with `run_local_build` per §4; `BuildCommand::execute` dispatches to it right after step 1's checks when `request.local.is_some()` (skips the private-repo token check, early resolve, fast path, store).
- **How:** `commands/build.rs` is 3.4k lines — do **not** grow it. Move the pieces both paths need (`resolve_version`, the version-override + `execute_build` + collect block of `run_clone_build`) into a `pub(crate)` helper `build_checked_out(…)` that `run_clone_build` and `run_local_build` both call. Behavior of `run_clone_build` must stay identical (its tests are the proof).
- **Docs:** `crates/core/README.md` (`BuildRequest`/`BuildOutput` sections). Adding public fields to these non-`#[non_exhaustive]` structs breaks external struct literals (3.4.0 is a minor bump; note it in release notes); fix the literal in `crates/napi/src/types.rs` tests.
- **Warm:** `Apvm::warm_cache` has no `local` field (`WarmRequest` unchanged), so the conflict only exists in front ends; core asserts nothing extra.
- **Tests:** new `crates/core/tests/local_build_e2e.rs` (`#![cfg(unix)]`, same `SourceBuilder` style as `build_e2e.rs`, artifact bytes reveal what was built): build `HEAD`, a branch, a tag, a SHA; `--uncommitted` includes an edit + untracked file; `--uncommitted` with a non-HEAD ref → error before cloning; identity mismatch → error; output dir gets the artifact; **cache untouched** (`APVM_CACHE_DIR` temp: no build rows/files after); **source untouched** (as P2.3); progress timeline order; private-project registry entry with no token still builds locally; a source dir named `my repo;x` builds (clone dir uses the registry name). Cross-platform unit tests (not `cfg(unix)`) for `with_repo_name` with a Windows-style source path, so Windows CI covers the naming. Existing `build_e2e.rs` / unit tests unchanged and green.

#### P2.5 — CLI `--repo` / `--uncommitted` · S · easy · Depends: P2.4
- **Goal:** `BuildArgs { repo: Option<PathBuf>, uncommitted: bool }` — `#[arg(long, value_name = "DIR", conflicts_with = "warm_cache")]`, `#[arg(long, requires = "repo")]`. Relative `--repo` resolved against the CWD. Help text: a "Local repository" section in `after_long_help` with examples. Output: `Reference: branch 'feature-x' @ abc1234 (local: /path[, + uncommitted changes])`; the `Source:`/cache summary says "not cached (local build)".
- **Tests (`crates/cli/tests/cli_commands.rs`, hermetic):** clap conflicts (`--uncommitted` without `--repo`; `--repo` + `--warm-cache`); non-repo dir → exit ≠ 0 with message; a full local build needs a registered project — if the CLI cannot register a test project, cover the display helpers with unit tests in `commands/build.rs` and rely on P2.4 for the pipeline.
- **Docs (same change, required by `CLAUDE.md`):** `crates/cli/README.md`, `SKILL.md` (build section, flags list, error patterns), `references/git-refs.md` (local ref table).

#### P2.6 — NAPI · S · easy · Depends: P2.4 · Parallel: P2.5
- **Goal:** `BuildOptions { repo_path: Option<String>, include_uncommitted: Option<bool> }` (`repoPath` is **checked** to be absolute → else `InvalidArg` — `outputDir` is only documented as absolute, so this is new validation; `includeUncommitted` without `repoPath` → `InvalidArg`); `JsBuildOutput.local: Option<JsLocalOrigin { repoPath, uncommitted }>`. `WarmOptions` unchanged.
- **Tests:** Rust unit tests for option mapping/validation in `types.rs`; Node `__tests__/apvm.test.ts`: relative path, `includeUncommitted` alone, non-repo dir → reject with `InvalidArg`/message (no real plugin build needed). `crates/napi/README.md` updated. Do not hand-edit or commit generated `index.d.ts`/`index.js`/`.node`.

#### P2.7 — Manual acceptance · XS · easy · Depends: P2.5
- With isolated `HOME`/`APVM_CACHE_DIR`: `gh pr checkout 9172` in a wp-rocket clone → `apvm build wp-rocket HEAD <tmp> --repo <clone>` builds `c45bc8f`; edit a file → `--uncommitted` zip contains the edit; the clone's `git status` is unchanged by apvm; cache dir has no new builds.

#### P3 — Release 3.4.0 · XS · easy · Depends: P2.5–P2.7 (+ coordinate with site-deployment, which also targets 3.4.0)
- Version bump checklist as P1.7; release notes.

---

## 6. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Fork PR runs untrusted npm/composer scripts as the user (files, SSH keys, tokens) | Trust gate (P1.3–P1.5): y/N per commit, fail closed when unsure (unknown repo, non-TTY, stale approval); tokens stripped from the build env. Not a sandbox: an approved fork can still read your files — the prompt says so. |
| P1.6 changes how **every** PR is checked out | Pinned SHA + `verify_head`; hermetic regression tests (incl. V3); manual builds of a fork PR, a deleted-branch PR and a normal PR before release. |
| `refs/pull/N/head` not served (non-GitHub remote, mirrors) | All registry repos are GitHub; missing ref → explicit error naming the ref. |
| Token leak in the new fetch | One shared token-swap helper with restore-on-error; tests assert the token is absent from errors and the remote URL afterwards. |
| apvm modifies the user's repo | Plumbing only (`diff-index`, not `diff`: V7), `GIT_OPTIONAL_LOCKS=0`, build in a temp clone; tests assert index bytes/mtime and status unchanged, incl. after `touch`. |
| Shell injection / broken builds via the user's dir name | Clone dir named after the registry repo (`with_repo_name`, V11); test with `my repo;x`. |
| Untracked secrets (`.env`, `auth.json` not in `.gitignore`) packaged into the zip with `--uncommitted` | Opt-in flag; warning lists counts; docs state that untracked, non-ignored files are included; untracked symlinks skipped (V12). Builders' zip exclusion lists still apply. |
| Huge untracked trees (missing `.gitignore` for `vendor/`, `node_modules/`) | Size/count warning; overlay is a copy into a temp dir that is always cleaned up. |
| Windows paths | `std::path::absolute` (no `\\?\`), UTF-8 requirement, registry-named clone dir; cross-platform unit tests for naming; NAPI Windows CI runs the Node validation tests. A full local build is only e2e-tested on Unix — P2.7 adds one manual Windows run if a Windows machine is available, else release notes say "Windows: lightly tested". |
| Silent misuse (`--repo` of another plugin) | `identity_files` check after checkout. |
| `commands/build.rs` grows further | Local pipeline in its own module; shared post-checkout helper extracted (P2.4). |
| Version-collision with site-deployment 3.4.0 | P3 is coordinated; Track A ships independently as 3.3.1. |

## 7. Out of scope / later

- Caching clean local commits (`commit:<sha>`), or `--warm-cache` with `--repo`.
- Building from an arbitrary remote URL / fork remote (`--remote`), now unnecessary for PRs.
- Submodules (not used by any registered project; remote builds don't init them either).
- Local builds in the site-deployment `build → install` flow (can reuse `LocalSource` later).

## 8. Audit log (2026-11-10)

Adversarial audit; every finding re-verified before fixing. Fixed in this revision: porcelain `git diff` rewrites the user's index → `diff-index` (V7, D7); user dir name used as clone dir → shell injection in WP Rocket commands and Windows path breakage → `with_repo_name` (V11); `diff.noprefix` broke the patch → `diff-index`; untracked symlinks followed by `zip` → skipped (V12); `refs/remotes/origin/pull/N` collides with a branch named `pull` → `refs/apvm/pull/N`; `Error::LocalRepo` breaks napi's exhaustive match → mapped in P2.1; skill/README updates moved into the phases that change output (`CLAUDE.md` rule); bare-repo detection needs two `rev-parse` calls (V13); cross-repo by repo id, both repos optional; P2.4 sequenced after P1.3; token redaction in the shared fetch helper; empty identity list is an error; nested repos and partial clones handled; napi version 3.14.2; release-note items. Checked correct by the audit: V1–V10, fork contents readable pre-clone, clap ids, error variants, test locations, 3.3.1 can be cut from `develop` without site-deployment code.

Revision 2026-11-23 (your review): added the fork trust gate (D9, §3.1, P1.3–P1.5); old P1.3/P1.4 renumbered to P1.6/P1.7; fork PRs whose approved SHA is gone now error instead of falling back to the current head.

## 9. Open questions

- **Version for Track A:** keep 3.3.1 (fast fix) or 3.4.0 (SemVer, since it adds `--trust-fork` and NAPI options)? 3.4.0 is already planned for site-deployment.
