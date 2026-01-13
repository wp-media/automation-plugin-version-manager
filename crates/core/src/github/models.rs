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
