//! Simplified GitHub models for APVM.

/// Simplified Pull Request information.
#[derive(Debug, Clone)]
pub struct PullRequest {
    /// PR number.
    pub number: u64,
    /// PR title.
    pub title: String,
    /// Base branch (target branch for merge).
    pub base_branch: String,
    /// Head branch (source branch with changes).
    pub head_branch: String,
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// HTML URL of the pull request.
    pub html_url: Option<String>,
}

/// Simplified GitHub Release information.
#[derive(Debug, Clone)]
pub struct Release {
    /// Release ID (GitHub internal).
    pub id: u64,
    /// Tag name associated with the release.
    pub tag_name: String,
    /// Release title.
    pub name: String,
    /// Whether this is a pre-release.
    pub prerelease: bool,
    /// Whether this is a draft release.
    pub draft: bool,
    /// Downloadable assets attached to the release.
    pub assets: Vec<ReleaseAsset>,
    /// HTML URL of the release page.
    pub html_url: Option<String>,
}

/// A single downloadable asset attached to a GitHub Release.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    /// Asset ID (GitHub internal).
    pub id: u64,
    /// Filename of the asset (e.g., `"backwpup-free-5.6.8.zip"`).
    pub name: String,
    /// Size of the asset in bytes.
    pub size: u64,
    /// Browser download URL (public releases) or API URL (private repos).
    pub download_url: String,
    /// Content type (e.g., `"application/zip"`).
    pub content_type: String,
}
