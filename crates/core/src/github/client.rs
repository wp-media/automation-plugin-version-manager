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
        let html_url = pr.html_url.map(|url| url.to_string());
        Ok(PullRequest {
            number: pr_number,
            title,
            base_branch,
            head_branch,
            owner: owner.to_string(),
            repo: repo.to_string(),
            html_url,
        })
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
            Ok(release) => {
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

                Ok(Some(Release {
                    id: release.id.into_inner(),
                    tag_name: release.tag_name.clone(),
                    name: release.name.clone().unwrap_or_default(),
                    prerelease: release.prerelease,
                    draft: release.draft,
                    assets,
                    html_url: Some(release.html_url.to_string()),
                }))
            }
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch the latest published release (non-draft, non-prerelease).
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
    pub async fn get_latest_release(&self, owner: &str, repo: &str) -> Result<Option<Release>> {
        let result = self.inner.repos(owner, repo).releases().get_latest().await;

        match result {
            Ok(release) => {
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

                Ok(Some(Release {
                    id: release.id.into_inner(),
                    tag_name: release.tag_name.clone(),
                    name: release.name.clone().unwrap_or_default(),
                    prerelease: release.prerelease,
                    draft: release.draft,
                    assets,
                    html_url: Some(release.html_url.to_string()),
                }))
            }
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code.as_u16() == 404 => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
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
