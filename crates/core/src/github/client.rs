//! GitHub API client wrapper.

use octocrab::Octocrab;

use crate::error::Result;

use super::models::PullRequest;

/// GitHub API client.
pub struct GitHubClient {
    inner: Octocrab,
}

impl GitHubClient {
    /// Create a new GitHub client with the given token.
    pub fn new(token: &str) -> Result<Self> {
        let inner = Octocrab::builder()
            .personal_token(token.to_string())
            .build()?;

        Ok(Self { inner })
    }

    /// Create a new GitHub client without authentication (rate-limited).
    pub fn anonymous() -> Result<Self> {
        let inner = Octocrab::builder().build()?;
        Ok(Self { inner })
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
        let html_url = pr.html_url.map(|url| {
            url.to_string()
        });
        Ok(PullRequest {
            number: pr_number,
            title,
            base_branch,
            head_branch,
            owner: owner.to_string(),
            repo: repo.to_string(),
            html_url
        })
    }
}
