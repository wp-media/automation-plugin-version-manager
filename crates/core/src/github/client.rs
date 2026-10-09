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
    /// The API also resolves branch and tag names (to their tip commit), so
    /// its answer only counts when the returned SHA starts with `reference`
    /// (case-insensitively): a branch named like a SHA (`deadbeef`) is "no
    /// such commit", never its tip.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(full_sha))` — the commit exists
    /// - `Ok(None)` — no such commit (404), an unresolvable/ambiguous
    ///   reference (422), or a branch/tag name rather than a SHA
    /// - `Err(_)` — transport/auth/rate-limit failure (existence unknown)
    ///
    /// # Sources
    ///
    /// - GitHub REST API — Get a commit:
    ///   <https://docs.github.com/en/rest/commits/commits#get-a-commit>
    /// - octocrab `commits().get()`:
    ///   <https://docs.rs/octocrab/0.54/octocrab/commits/struct.CommitHandler.html#method.get>
    pub async fn get_commit_sha(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
    ) -> Result<Option<String>> {
        let result = self.inner.commits(owner, repo).get(reference).await;

        match result {
            Ok(commit) => Ok(names_commit(reference, &commit.sha).then_some(commit.sha)),
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
    ///   <https://docs.rs/octocrab/0.54/octocrab/repos/struct.ReleasesHandler.html#method.get_latest>
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
    ///   <https://docs.rs/octocrab/0.54/octocrab/repos/releases/struct.ReleasesHandler.html#method.list>
    /// - octocrab `Page<T>` (field `items: Vec<T>`):
    ///   <https://docs.rs/octocrab/0.54/octocrab/struct.Page.html>
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
            Ok(page) => Ok(Self::select_nth_release(&page.items, index, stable_only)),
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Pick the `index`-th (zero-based) non-draft release from `releases`,
    /// also skipping prereleases when `stable_only`, converted to our model.
    ///
    /// `releases` is expected newest first (GitHub's list order), so index 0
    /// is the latest and index 1 the previous. Split from
    /// [`get_nth_release`](Self::get_nth_release) so the selection rules are
    /// testable without the network.
    fn select_nth_release(
        releases: &[octocrab::models::repos::Release],
        index: usize,
        stable_only: bool,
    ) -> Option<Release> {
        releases
            .iter()
            .filter(|r| !r.draft)
            .filter(|r| !stable_only || !r.prerelease)
            .nth(index)
            .map(Self::convert_release)
    }

    /// Convert an octocrab `Release` model into our simplified `Release`.
    ///
    /// # Sources
    ///
    /// - octocrab `Release` model:
    ///   <https://docs.rs/octocrab/0.54/octocrab/models/repos/struct.Release.html>
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

/// Whether `sha` (as the commits API returned it) is the commit `reference`
/// abbreviates: a real match always starts with the (hex) input, compared
/// case-insensitively — the API answers an upper-case SHA in lower case.
///
/// # Arguments
///
/// * `reference` - What was looked up
/// * `sha` - The full SHA the API resolved it to
fn names_commit(reference: &str, sha: &str) -> bool {
    sha.to_ascii_lowercase()
        .starts_with(&reference.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_commit_rejects_a_branch_or_tag_the_api_resolved() {
        let tip = "0123456789abcdef0123456789abcdef01234567";
        // `deadbeef` named a branch whose tip is `0123…`: not a commit.
        assert!(!names_commit("deadbeef", tip));
        assert!(!names_commit("develop", tip));
        // A real (case-insensitive) abbreviation or the full SHA is kept.
        assert!(names_commit("0123456", tip));
        assert!(names_commit("0123456789ABCDEF", tip));
        assert!(names_commit(tip, tip));
    }

    /// An octocrab release as GitHub's REST API returns it (fields the model
    /// treats as optional are omitted), with one asset.
    fn api_release(id: u64, tag: &str, draft: bool, prerelease: bool) -> serde_json::Value {
        serde_json::json!({
            "url": format!("https://api.github.com/repos/o/r/releases/{id}"),
            "html_url": format!("https://github.com/o/r/releases/tag/{tag}"),
            "assets_url": format!("https://api.github.com/repos/o/r/releases/{id}/assets"),
            "upload_url": "https://uploads.github.com/repos/o/r/releases/1/assets{?name,label}",
            "id": id,
            "node_id": "RE_x",
            "tag_name": tag,
            "target_commitish": "main",
            "name": format!("Release {tag}"),
            "draft": draft,
            "prerelease": prerelease,
            "assets": [{
                "url": "https://api.github.com/repos/o/r/releases/assets/77",
                "browser_download_url": format!("https://github.com/o/r/releases/download/{tag}/plugin.zip"),
                "id": 77,
                "node_id": "RA_x",
                "name": "plugin.zip",
                "state": "uploaded",
                "content_type": "application/zip",
                "size": 2048,
                "download_count": 3,
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            }]
        })
    }

    /// Deserialize [`api_release`] into octocrab's model.
    fn release(
        id: u64,
        tag: &str,
        draft: bool,
        prerelease: bool,
    ) -> octocrab::models::repos::Release {
        serde_json::from_value(api_release(id, tag, draft, prerelease)).unwrap()
    }

    #[test]
    fn convert_release_maps_every_field() {
        let converted = GitHubClient::convert_release(&release(42, "v1.2.3", false, true));

        assert_eq!(converted.id, 42);
        assert_eq!(converted.tag_name, "v1.2.3");
        assert_eq!(converted.name, "Release v1.2.3");
        assert!(converted.prerelease);
        assert!(!converted.draft);
        assert_eq!(
            converted.html_url.as_deref(),
            Some("https://github.com/o/r/releases/tag/v1.2.3")
        );
        assert_eq!(converted.assets.len(), 1);
        let asset = &converted.assets[0];
        // The asset id drives the download URL, so it must survive conversion.
        assert_eq!(asset.id, 77);
        assert_eq!(asset.name, "plugin.zip");
        assert_eq!(asset.size, 2048);
        assert_eq!(asset.content_type, "application/zip");
        assert_eq!(
            asset.download_url,
            "https://github.com/o/r/releases/download/v1.2.3/plugin.zip"
        );
    }

    #[test]
    fn convert_release_defaults_a_missing_name_to_empty() {
        let mut json = api_release(1, "v1", false, false);
        json["name"] = serde_json::Value::Null;
        let release: octocrab::models::repos::Release = serde_json::from_value(json).unwrap();

        assert_eq!(GitHubClient::convert_release(&release).name, "");
    }

    #[test]
    fn select_nth_release_never_picks_drafts_and_honors_stable_only() {
        // Newest first, as GitHub lists them.
        let releases = [
            release(5, "v3.0.0-draft", true, false),
            release(4, "v3.0.0-rc1", false, true),
            release(3, "v2.1.0", false, false),
            release(2, "v2.1.0-beta", false, true),
            release(1, "v2.0.0", false, false),
        ];
        let tag = |index, stable_only| {
            GitHubClient::select_nth_release(&releases, index, stable_only).map(|r| r.tag_name)
        };

        // release:latest / release:previous-latest
        assert_eq!(tag(0, false).as_deref(), Some("v3.0.0-rc1"));
        assert_eq!(tag(1, false).as_deref(), Some("v2.1.0"));
        // release:latest-stable / release:previous-stable
        assert_eq!(tag(0, true).as_deref(), Some("v2.1.0"));
        assert_eq!(tag(1, true).as_deref(), Some("v2.0.0"));
        // Not enough matching releases.
        assert_eq!(tag(2, true), None);
        assert!(GitHubClient::select_nth_release(&[], 0, false).is_none());
    }

    #[tokio::test]
    async fn token_is_kept_only_for_authenticated_clients() {
        // Building an octocrab client needs a Tokio reactor but no network.
        let authed = GitHubClient::new("ghp_secret").unwrap();
        let anonymous = GitHubClient::anonymous().unwrap();

        assert_eq!(authed.token().as_deref(), Some("ghp_secret"));
        assert_eq!(anonymous.token(), None);
    }
}
