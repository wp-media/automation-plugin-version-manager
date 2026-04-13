//! GitHub API integration.

pub mod client;
mod models;

pub use client::GitHubClient;
pub use models::{PullRequest, Release, ReleaseAsset};
