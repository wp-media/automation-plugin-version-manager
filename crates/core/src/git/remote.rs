//! Remote git queries that need **no local clone**.
//!
//! Powers the build pipeline's pre-clone resolution phase: refs are checked
//! directly against the remote so a bad reference fails fast — before any
//! repository is cloned — and a good one resolves to a concrete commit SHA
//! usable for cache lookups.
//!
//! Two primitives:
//!
//! - [`RemoteGit::ls_remote`] — one `git ls-remote` round-trip retrieving
//!   every advertised branch and tag with its SHA (annotated tags are peeled
//!   to their commit).
//! - [`RemoteGit::tags_by_creatordate`] — tag names newest-first, exactly
//!   like the post-clone `git tag --sort=-creatordate`. `ls-remote` cannot
//!   sort by creator date (no object data before a fetch — verified: git
//!   fails with *"the field 'creatordate' requires access to object data"*),
//!   so this performs a tiny tags-only fetch into a throwaway bare repo:
//!   `--depth=1` (tip commits only) plus `--filter=tree:0` (no trees/blobs;
//!   servers without filter support just ignore it with a warning). For a
//!   typical plugin repo this transfers a few KB instead of a full clone.
//!
//! Authentication mirrors [`super::Repository`]: an optional GitHub token is
//! embedded in HTTPS URLs for the single command invocation and never
//! stored; error output is redacted before it can leak the token.
//! `GIT_TERMINAL_PROMPT=0` is set on every command so a missing credential
//! fails immediately instead of hanging on an interactive prompt.

use std::collections::HashMap;
use std::path::Path;

use tokio::process::Command;

use crate::error::{Error, Result};

/// Snapshot of the refs advertised by a remote repository.
///
/// Produced by [`RemoteGit::ls_remote`]; lookups are cheap map reads, so a
/// single network round-trip serves any number of branch/tag checks.
#[derive(Debug, Default)]
pub struct RemoteRefs {
    /// Branch name → commit SHA (from `refs/heads/*`).
    branches: HashMap<String, String>,
    /// Tag name → commit SHA (from `refs/tags/*`; annotated tags use the
    /// peeled `^{}` target so the SHA is always the *commit*, matching what
    /// `git checkout <tag>` lands on).
    tags: HashMap<String, String>,
    /// SHA of the remote's default branch head, if advertised.
    head: Option<String>,
}

impl RemoteRefs {
    /// Parse `git ls-remote` output (`<sha>\t<refname>` per line).
    ///
    /// Annotated tags appear twice: `refs/tags/X` (tag object SHA) and
    /// `refs/tags/X^{}` (peeled commit SHA). The peeled entry wins so
    /// [`Self::tag_sha`] always yields the commit.
    fn parse(output: &str) -> Self {
        let mut refs = Self::default();

        for line in output.lines() {
            let Some((sha, name)) = line.split_once('\t') else {
                continue;
            };
            let (sha, name) = (sha.trim(), name.trim());
            if sha.is_empty() || name.is_empty() {
                continue;
            }

            if name == "HEAD" {
                refs.head = Some(sha.to_string());
            } else if let Some(branch) = name.strip_prefix("refs/heads/") {
                refs.branches.insert(branch.to_string(), sha.to_string());
            } else if let Some(tag) = name.strip_prefix("refs/tags/") {
                if let Some(peeled) = tag.strip_suffix("^{}") {
                    // Peeled commit SHA: authoritative for annotated tags.
                    refs.tags.insert(peeled.to_string(), sha.to_string());
                } else {
                    // Lightweight tag (or annotated tag object SHA seen
                    // before its peeled line): only insert if nothing better
                    // is recorded yet.
                    refs.tags
                        .entry(tag.to_string())
                        .or_insert_with(|| sha.to_string());
                }
            }
            // Other namespaces (refs/pull/*, refs/merge-requests/*, …) are
            // intentionally ignored: PRs resolve through the GitHub API.
        }

        refs
    }

    /// Commit SHA of a branch, if the remote advertises it.
    pub fn branch_sha(&self, name: &str) -> Option<&str> {
        self.branches.get(name).map(String::as_str)
    }

    /// Commit SHA of a tag (peeled for annotated tags), if advertised.
    pub fn tag_sha(&self, name: &str) -> Option<&str> {
        self.tags.get(name).map(String::as_str)
    }

    /// SHA of the remote HEAD (default branch), if advertised.
    pub fn head_sha(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Number of advertised branches (diagnostics).
    pub fn branch_count(&self) -> usize {
        self.branches.len()
    }

    /// Number of advertised tags (diagnostics).
    pub fn tag_count(&self) -> usize {
        self.tags.len()
    }
}

/// Client for querying a git remote without a local repository.
#[derive(Debug, Clone)]
pub struct RemoteGit {
    /// Remote repository URL (HTTPS, SSH, or local path — anything the
    /// system git accepts).
    url: String,
    /// Optional token embedded into HTTPS URLs per invocation.
    token: Option<String>,
}

impl RemoteGit {
    /// Create a remote client for `url`, optionally authenticating HTTPS
    /// operations with a GitHub token.
    pub fn new(url: impl Into<String>, token: Option<&str>) -> Self {
        Self {
            url: url.into(),
            token: token.map(String::from),
        }
    }

    /// The remote URL this client queries (token-free).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Fetch the remote's advertised refs in one round-trip.
    ///
    /// # Errors
    ///
    /// Fails if the remote is unreachable or authentication is rejected —
    /// conditions under which a subsequent clone would fail too, so callers
    /// can safely treat this as fatal for the whole operation.
    pub async fn ls_remote(&self) -> Result<RemoteRefs> {
        tracing::debug!("Listing remote refs for {}", self.url);

        let output = self
            .git_command()
            .args(["ls-remote", &self.effective_url()])
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git ls-remote: {e}")))?;

        if !output.status.success() {
            let stderr = self.redact(&String::from_utf8_lossy(&output.stderr));
            return Err(Error::Git(format!(
                "git ls-remote failed for {}: {stderr}",
                self.url
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let refs = RemoteRefs::parse(&stdout);
        tracing::debug!(
            "Remote {} advertises {} branches and {} tags",
            self.url,
            refs.branch_count(),
            refs.tag_count()
        );
        Ok(refs)
    }

    /// List the remote's tag names sorted by creation date, newest first —
    /// the same ordering as local `git tag --sort=-creatordate`.
    ///
    /// Implemented as a minimal tags-only fetch (`--depth=1`, plus
    /// `--filter=tree:0` where the server supports partial clone) into a
    /// temporary bare repository that is deleted before returning.
    ///
    /// Returns an empty vector when the remote has no tags.
    pub async fn tags_by_creatordate(&self) -> Result<Vec<String>> {
        tracing::debug!("Fetching tag metadata from {}", self.url);

        // `git fetch` fails outright when a refspec matches nothing, so a
        // tag-less remote must be detected first (it is an empty result,
        // not an error).
        if !self.has_any_tags().await? {
            tracing::debug!("Remote {} has no tags", self.url);
            return Ok(Vec::new());
        }

        // Throwaway bare repo; auto-removed on drop (even on error).
        let probe = tempfile::Builder::new()
            .prefix("apvm-ref-probe-")
            .tempdir()
            .map_err(Error::Io)?;

        self.run_git_in(
            probe.path(),
            &["init", "--quiet", "--bare"],
            "initializing ref probe",
        )
        .await?;

        // Fetch only tag refs. Two attempts:
        //   1. shallow + treeless (`--filter=tree:0`): tiny transfer; servers
        //      without partial-clone support ignore the filter with a warning.
        //   2. shallow only: for transports that reject the filter option
        //      outright instead of ignoring it.
        let url = self.effective_url();
        let refspec = "+refs/tags/*:refs/tags/*";
        let with_filter = [
            "fetch",
            "--quiet",
            "--depth=1",
            "--filter=tree:0",
            url.as_str(),
            refspec,
        ];
        let without_filter = ["fetch", "--quiet", "--depth=1", url.as_str(), refspec];

        if let Err(first) = self
            .run_git_in(probe.path(), &with_filter, "fetching tags (filtered)")
            .await
        {
            tracing::debug!("Filtered tag fetch failed ({first}); retrying without filter");
            self.run_git_in(probe.path(), &without_filter, "fetching tags")
                .await?;
        }

        let output = self
            .run_git_in(
                probe.path(),
                &["tag", "--sort=-creatordate"],
                "sorting tags by creation date",
            )
            .await?;

        Ok(output
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    /// Whether the remote advertises at least one tag (`ls-remote --tags`).
    async fn has_any_tags(&self) -> Result<bool> {
        let output = self
            .git_command()
            .args(["ls-remote", "--tags", &self.effective_url()])
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git ls-remote: {e}")))?;

        if !output.status.success() {
            let stderr = self.redact(&String::from_utf8_lossy(&output.stderr));
            return Err(Error::Git(format!(
                "git ls-remote --tags failed for {}: {stderr}",
                self.url
            )));
        }

        Ok(!output.stdout.is_empty())
    }

    /// Run a git subcommand inside `dir`, returning stdout on success.
    async fn run_git_in(&self, dir: &Path, args: &[&str], what: &str) -> Result<String> {
        let output = self
            .git_command()
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .map_err(|e| Error::Git(format!("Failed to execute git while {what}: {e}")))?;

        if !output.status.success() {
            let stderr = self.redact(&String::from_utf8_lossy(&output.stderr));
            return Err(Error::Git(format!("git failed while {what}: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Base git command with interactive prompts disabled: a missing or
    /// rejected credential must fail immediately, not hang a CI pipeline
    /// waiting for terminal input.
    fn git_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    /// The URL to hand to git: token-authenticated for HTTPS when a token is
    /// configured (same scheme as [`super::Repository::clone_with_token`]),
    /// verbatim otherwise.
    fn effective_url(&self) -> String {
        match &self.token {
            Some(token) if self.url.starts_with("https://") => {
                self.url
                    .replacen("https://", &format!("https://x-access-token:{token}@"), 1)
            }
            _ => self.url.clone(),
        }
    }

    /// Redact the token from text destined for error messages or logs.
    fn redact(&self, text: &str) -> String {
        match &self.token {
            Some(token) if !token.is_empty() => text.replace(token, "[REDACTED]"),
            _ => text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    // =========================================================================
    // RemoteRefs::parse
    // =========================================================================

    const SAMPLE: &str = "\
9054c60ecd697daf5504aa5d6e9a022f19154732\tHEAD
9054c60ecd697daf5504aa5d6e9a022f19154732\trefs/heads/develop
1111111111111111111111111111111111111111\trefs/heads/feature/x
2970b4cf7f6557cd31ee70279921f7588f2002dc\trefs/tags/v1.0.0
d28a355392e82c73d7c5a0623822df0ac9cc939d\trefs/tags/v2.0.0
9054c60ecd697daf5504aa5d6e9a022f19154732\trefs/tags/v2.0.0^{}
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\trefs/pull/12/head
";

    #[test]
    fn parse_extracts_head_branches_and_tags() {
        let refs = RemoteRefs::parse(SAMPLE);

        assert_eq!(
            refs.head_sha(),
            Some("9054c60ecd697daf5504aa5d6e9a022f19154732")
        );
        assert_eq!(
            refs.branch_sha("develop"),
            Some("9054c60ecd697daf5504aa5d6e9a022f19154732")
        );
        assert_eq!(
            refs.branch_sha("feature/x"),
            Some("1111111111111111111111111111111111111111")
        );
        assert!(refs.branch_sha("missing").is_none());
        assert_eq!(refs.branch_count(), 2);
    }

    #[test]
    fn parse_peels_annotated_tags_to_commit_sha() {
        let refs = RemoteRefs::parse(SAMPLE);

        // Lightweight tag: ref SHA is the commit.
        assert_eq!(
            refs.tag_sha("v1.0.0"),
            Some("2970b4cf7f6557cd31ee70279921f7588f2002dc")
        );
        // Annotated tag: the peeled ^{} commit wins over the tag object SHA.
        assert_eq!(
            refs.tag_sha("v2.0.0"),
            Some("9054c60ecd697daf5504aa5d6e9a022f19154732")
        );
        assert_eq!(refs.tag_count(), 2);
    }

    #[test]
    fn parse_ignores_pull_refs_and_garbage() {
        let refs = RemoteRefs::parse("garbage line without tab\n\n");
        assert_eq!(refs.branch_count(), 0);
        assert_eq!(refs.tag_count(), 0);
        assert!(refs.head_sha().is_none());

        let refs = RemoteRefs::parse(SAMPLE);
        // refs/pull/* is deliberately not exposed anywhere.
        assert!(refs.branch_sha("12/head").is_none());
        assert!(refs.tag_sha("12/head").is_none());
    }

    // =========================================================================
    // Live behavior against a local repository (no network)
    // =========================================================================

    use crate::git::testutil::{file_url, make_remote_repo};

    #[tokio::test]
    async fn ls_remote_resolves_branches_and_tags_without_clone() {
        let repo = make_remote_repo();
        let remote = RemoteGit::new(file_url(&repo), None);

        let refs = remote.ls_remote().await.unwrap();

        // Branches resolve to 40-hex SHAs.
        let main_sha = refs.branch_sha("main").expect("main must exist");
        assert_eq!(main_sha.len(), 40);
        assert!(refs.branch_sha("feature/x").is_some());
        assert!(refs.branch_sha("nope").is_none());

        // Tags resolve to *commit* SHAs (annotated tags peeled): v2.0.0
        // tags c3 which is main's tip.
        assert_eq!(refs.tag_sha("v2.0.0"), Some(main_sha));
        assert!(refs.tag_sha("v1.0.0").is_some());
        assert!(refs.tag_sha("v9.9.9").is_none());

        assert_eq!(refs.head_sha(), Some(main_sha));
    }

    #[tokio::test]
    async fn tags_by_creatordate_orders_newest_first() {
        let repo = make_remote_repo();
        let remote = RemoteGit::new(file_url(&repo), None);

        let tags = remote.tags_by_creatordate().await.unwrap();

        // Same ordering the post-clone `git tag --sort=-creatordate` gives:
        // v2.0.0 (2025-01) > v2.0.0-beta1 (2024-06) > v1.0.0 (2024-01).
        assert_eq!(tags, vec!["v2.0.0", "v2.0.0-beta1", "v1.0.0"]);
    }

    #[tokio::test]
    async fn tags_by_creatordate_empty_repo() {
        let dir = TempDir::new().unwrap();
        let out = StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());

        let remote = RemoteGit::new(file_url(&dir), None);
        let tags = remote.tags_by_creatordate().await.unwrap();
        assert!(tags.is_empty());
    }

    #[tokio::test]
    async fn ls_remote_unreachable_remote_fails_fast() {
        let missing = TempDir::new().unwrap();
        let url = format!("file://{}/does-not-exist", missing.path().display());
        let remote = RemoteGit::new(url, None);

        let err = remote.ls_remote().await.unwrap_err();
        assert!(err.to_string().contains("ls-remote failed"));
    }

    // =========================================================================
    // Auth plumbing
    // =========================================================================

    #[test]
    fn effective_url_embeds_token_for_https_only() {
        let with = RemoteGit::new("https://github.com/o/r.git", Some("tok123"));
        assert_eq!(
            with.effective_url(),
            "https://x-access-token:tok123@github.com/o/r.git"
        );

        // Non-HTTPS transports never get the token embedded.
        let ssh = RemoteGit::new("git@github.com:o/r.git", Some("tok123"));
        assert_eq!(ssh.effective_url(), "git@github.com:o/r.git");

        let no_token = RemoteGit::new("https://github.com/o/r.git", None);
        assert_eq!(no_token.effective_url(), "https://github.com/o/r.git");
    }

    #[test]
    fn redact_scrubs_token_from_errors() {
        let remote = RemoteGit::new("https://github.com/o/r.git", Some("sekrit"));
        assert_eq!(
            remote.redact("fatal: auth failed for sekrit@github.com"),
            "fatal: auth failed for [REDACTED]@github.com"
        );
        // No token configured → text passes through.
        let open = RemoteGit::new("https://github.com/o/r.git", None);
        assert_eq!(open.redact("plain"), "plain");
    }
}
