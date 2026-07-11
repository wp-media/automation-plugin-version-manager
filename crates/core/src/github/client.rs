//! GitHub API client wrapper.

use octocrab::Octocrab;

use crate::error::Result;

use super::models::{PullRequest, Release, ReleaseAsset};

/// GitHub API client.
pub struct GitHubClient {
    inner: Octocrab,
    /// Stored token for authenticated downloads.
    token: Option<String>,
}

impl GitHubClient {
    /// Create a new GitHub client with the given token.
    pub fn new(token: &str) -> Result<Self> {
        let inner = Octocrab::builder()
            .personal_token(token.to_string())
            .build()?;

        Ok(Self {
            inner,
            token: Some(token.to_string()),
        })
    }

    /// Create a new GitHub client without authentication (rate-limited).
    pub fn anonymous() -> Result<Self> {
        let inner = Octocrab::builder().build()?;
        Ok(Self { inner, token: None })
    }

    /// Returns a clone of the stored auth token, if any.
    ///
    /// Used to pass owned token data into spawned tasks that require `'static`.
    pub fn token(&self) -> Option<String> {
        self.token.clone()
    }

    /// Fetch a pull request by number.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<PullRequest> {
        let pr = self.inner.pulls(owner, repo).get(pr_number).await?;

        let title = pr.title.clone().unwrap_or_default();

        let base_branch = pr.base.ref_field.clone();

        let head_branch = pr.head.ref_field.clone();
        // `head.sha` is a required field on GitHub's PR payload (octocrab types
        // it as a non-optional String); wrapped in `Some` to keep the model
        // tolerant of a future absent value.
        let head_sha = Some(pr.head.sha.clone());
        let html_url = pr.html_url.map(|url| url.to_string());
        Ok(PullRequest {
            number: pr_number,
            title,
            base_branch,
            head_branch,
            head_sha,
            owner: owner.to_string(),
            repo: repo.to_string(),
            html_url,
        })
    }

    /// Resolve a commit reference (full or abbreviated SHA) to its full SHA.
    ///
    /// Used by pre-clone ref resolution: `git ls-remote` can list branches
    /// and tags but cannot confirm an arbitrary commit exists, so commits
    /// are validated (and short SHAs expanded) through the GitHub commits
    /// API instead.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(full_sha))` — the commit exists
    /// - `Ok(None)` — no such commit (404) or unresolvable/ambiguous
    ///   reference (422)
    /// - `Err(_)` — transport/auth/rate-limit failure (existence unknown)
    ///
    /// # Sources
    ///
    /// - GitHub REST API — Get a commit:
    ///   <https://docs.github.com/en/rest/commits/commits#get-a-commit>
    /// - octocrab `commits().get()`:
    ///   <https://docs.rs/octocrab/0.49/octocrab/commits/struct.CommitHandler.html#method.get>
    pub async fn get_commit_sha(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
    ) -> Result<Option<String>> {
        let result = self.inner.commits(owner, repo).get(reference).await;

        match result {
            Ok(commit) => Ok(Some(commit.sha)),
            Err(octocrab::Error::GitHub { source, .. })
                if matches!(source.status_code.as_u16(), 404 | 422) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch the raw bytes of a single file at a git ref (commit SHA, branch,
    /// or tag) via the GitHub "Get repository content" API.
    ///
    /// Authenticated when a token is present, so it works for private repos and
    /// uses the higher authenticated rate limit. The `application/vnd.github.raw`
    /// media type returns the file bytes directly (no base64) and serves files up
    /// to 100 MB (the ~1 MB cap applies only to the base64 JSON form) — ample for
    /// the plugin header files this is used for.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(bytes))` — the file exists at `git_ref`
    /// - `Ok(None)` — the file or ref does not exist (HTTP 404), so callers can
    ///   treat "not found" as "cannot fetch" without special-casing errors
    /// - `Err(_)` — transport/auth/rate-limit failure
    ///
    /// # Sources
    ///
    /// - GitHub REST API — Get repository content:
    ///   <https://docs.github.com/en/rest/repos/contents#get-repository-content>
    pub async fn get_file_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<Vec<u8>>> {
        // `path` and `git_ref` come from trusted builder constants and resolved
        // commit SHAs (no spaces or reserved characters), so they are safe to
        // interpolate directly into the URL.
        let url =
            format!("https://api.github.com/repos/{owner}/{repo}/contents/{path}?ref={git_ref}");

        // `Client::builder().build()` surfaces a TLS-init failure as an error
        // rather than panicking (as `Client::new()` would), keeping this library
        // path panic-free.
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| crate::error::Error::Build(format!("Failed to build HTTP client: {e}")))?;
        let mut request = client
            .get(&url)
            .header("Accept", "application/vnd.github.raw")
            .header("User-Agent", "apvm");

        if let Some(token) = self.token.as_deref() {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request.send().await.map_err(|e| {
            crate::error::Error::Build(format!("Failed to request '{path}' at {git_ref}: {e}"))
        })?;

        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(crate::error::Error::Build(format!(
                "Failed to fetch '{path}' at {git_ref}: HTTP {}",
                response.status()
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| {
                crate::error::Error::Build(format!("Failed to read '{path}' at {git_ref}: {e}"))
            })?
            .to_vec();

        Ok(Some(bytes))
    }

    /// Fetch a release by tag name.
    ///
    /// Returns `None` if no release exists for the given tag.
    pub async fn get_release_by_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Option<Release>> {
        let result = self
            .inner
            .repos(owner, repo)
            .releases()
            .get_by_tag(tag)
            .await;

        match result {
            Ok(release) => Ok(Some(Self::convert_release(&release))),
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch the latest **stable** release (non-draft, non-prerelease).
    ///
    /// Uses GitHub's "Get the latest release" API endpoint which returns the
    /// most recent release that is not a draft and not a prerelease.
    ///
    /// # Sources
    ///
    /// - GitHub REST API:
    ///   <https://docs.github.com/en/rest/releases/releases#get-the-latest-release>
    /// - octocrab `get_latest()`:
    ///   <https://docs.rs/octocrab/0.49/octocrab/repos/struct.ReleasesHandler.html#method.get_latest>
    pub async fn get_latest_stable_release(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Release>> {
        let result = self.inner.repos(owner, repo).releases().get_latest().await;

        match result {
            Ok(release) => Ok(Some(Self::convert_release(&release))),
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch the previous **stable** release (the second most recent
    /// non-draft, non-prerelease release).
    ///
    /// Lists releases ordered by `created_at` descending (GitHub's default),
    /// filters out drafts and prereleases, and returns the **second** match.
    /// Returns `Ok(None)` if fewer than 2 stable releases exist.
    ///
    /// # Sources
    ///
    /// - GitHub REST API — List releases:
    ///   <https://docs.github.com/en/rest/releases/releases#list-releases>
    /// - octocrab `list().per_page().send()`:
    ///   <https://docs.rs/octocrab/0.49/octocrab/repos/releases/struct.ReleasesHandler.html#method.list>
    /// - octocrab `Page<T>` (field `items: Vec<T>`):
    ///   <https://docs.rs/octocrab/0.49/octocrab/struct.Page.html>
    pub async fn get_previous_stable_release(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Release>> {
        self.get_nth_release(owner, repo, 1, true).await
    }

    /// Fetch the latest release of **any** kind (including prereleases,
    /// but never drafts).
    ///
    /// Lists releases ordered by `created_at` descending, skips drafts, and
    /// returns the first non-draft release (which may be a prerelease).
    /// Returns `Ok(None)` if no non-draft releases exist.
    ///
    /// # Sources
    ///
    /// - GitHub REST API — List releases:
    ///   <https://docs.github.com/en/rest/releases/releases#list-releases>
    pub async fn get_latest_release_any(&self, owner: &str, repo: &str) -> Result<Option<Release>> {
        self.get_nth_release(owner, repo, 0, false).await
    }

    /// Fetch the previous release of **any** kind (including prereleases,
    /// but never drafts).
    ///
    /// Lists releases ordered by `created_at` descending, skips drafts, and
    /// returns the **second** non-draft release.
    /// Returns `Ok(None)` if fewer than 2 non-draft releases exist.
    ///
    /// # Sources
    ///
    /// - GitHub REST API — List releases:
    ///   <https://docs.github.com/en/rest/releases/releases#list-releases>
    pub async fn get_previous_release_any(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Release>> {
        self.get_nth_release(owner, repo, 1, false).await
    }

    /// Fetch the Nth non-draft release from the list, optionally filtering
    /// to stable-only (non-prerelease).
    ///
    /// # Arguments
    ///
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `index` - Zero-based index into the filtered list (0 = first, 1 = second)
    /// * `stable_only` - If `true`, skip prereleases in addition to drafts
    ///
    /// # Sources
    ///
    /// - GitHub REST API — List releases (ordered by `created_at` desc):
    ///   <https://docs.github.com/en/rest/releases/releases#list-releases>
    async fn get_nth_release(
        &self,
        owner: &str,
        repo: &str,
        index: usize,
        stable_only: bool,
    ) -> Result<Option<Release>> {
        let result = self
            .inner
            .repos(owner, repo)
            .releases()
            .list()
            .per_page(30)
            .send()
            .await;

        match result {
            Ok(page) => {
                let found = page
                    .items
                    .iter()
                    .filter(|r| !r.draft)
                    .filter(|r| !stable_only || !r.prerelease)
                    .nth(index)
                    .map(Self::convert_release);

                Ok(found)
            }
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Convert an octocrab `Release` model into our simplified `Release`.
    ///
    /// # Sources
    ///
    /// - octocrab `Release` model:
    ///   <https://docs.rs/octocrab/0.49/octocrab/models/repos/struct.Release.html>
    fn convert_release(release: &octocrab::models::repos::Release) -> Release {
        let assets = release
            .assets
            .iter()
            .map(|a| ReleaseAsset {
                id: a.id.into_inner(),
                name: a.name.clone(),
                size: a.size as u64,
                download_url: a.browser_download_url.to_string(),
                content_type: a.content_type.clone(),
            })
            .collect();

        Release {
            id: release.id.into_inner(),
            tag_name: release.tag_name.clone(),
            name: release.name.clone().unwrap_or_default(),
            prerelease: release.prerelease,
            draft: release.draft,
            assets,
            html_url: Some(release.html_url.to_string()),
        }
    }

    /// Download a release asset's bytes.
    ///
    /// Uses the browser download URL for public repos. For private repos,
    /// sends an authenticated request to the API asset endpoint with the
    /// `application/octet-stream` accept header, which GitHub redirects
    /// to a signed download URL.
    pub async fn download_release_asset(
        &self,
        owner: &str,
        repo: &str,
        asset: &ReleaseAsset,
    ) -> Result<Vec<u8>> {
        // Use the API endpoint which works for both public and private repos
        // when authenticated. GitHub returns a 302 redirect to the actual download.
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/assets/{asset_id}",
            asset_id = asset.id
        );

        let client = reqwest::Client::new();
        let mut request = client
            .get(&url)
            .header("Accept", "application/octet-stream")
            .header("User-Agent", "apvm");

        // Add auth token if the octocrab client was created with one
        if let Some(token) = self.token.as_deref() {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request.send().await.map_err(|e| {
            crate::error::Error::Build(format!("Failed to request asset '{}': {e}", asset.name))
        })?;

        if !response.status().is_success() {
            return Err(crate::error::Error::Build(format!(
                "Failed to download asset '{}': HTTP {}",
                asset.name,
                response.status()
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| {
                crate::error::Error::Build(format!("Failed to read asset '{}': {e}", asset.name))
            })?
            .to_vec();

        Ok(bytes)
    }
}

/// Download a single release asset, taking all data by ownership.
///
/// This standalone function satisfies the `Send + 'static` bound required
/// by [`tokio::task::JoinSet::spawn`], enabling true parallel downloads
/// across tokio worker threads.
///
/// Returns a tuple of `(asset_name, bytes)` on success.
///
/// # Sources
/// - [`tokio::task::JoinSet::spawn`](https://docs.rs/tokio/1.49.0/tokio/task/struct.JoinSet.html#method.spawn)
///   requires `F: Future<Output = T> + Send + 'static`.
/// - [`reqwest::Client::get`](https://docs.rs/reqwest/0.13.2/reqwest/struct.Client.html#method.get)
///   for building the HTTP request.
pub async fn download_asset_owned(
    token: Option<String>,
    owner: String,
    repo: String,
    asset: ReleaseAsset,
) -> Result<(String, Vec<u8>)> {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/releases/assets/{asset_id}",
        asset_id = asset.id
    );

    let client = reqwest::Client::new();
    let mut request = client
        .get(&url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", "apvm");

    if let Some(ref token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let response = request.send().await.map_err(|e| {
        crate::error::Error::Build(format!("Failed to request asset '{}': {e}", asset.name))
    })?;

    if !response.status().is_success() {
        return Err(crate::error::Error::Build(format!(
            "Failed to download asset '{}': HTTP {}",
            asset.name,
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| {
            crate::error::Error::Build(format!("Failed to read asset '{}': {e}", asset.name))
        })?
        .to_vec();

    Ok((asset.name, bytes))
}
