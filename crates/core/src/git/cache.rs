//! Repository cache management.

use std::path::{Path, PathBuf};

use crate::error::Result;

use super::repository::Repository;

/// Manages a cache of cloned repositories.
///
/// # Directory Structure
///
/// ```text
/// {cache_dir}/
/// └── repos/
///     └── {project_name}/
///         ├── {repo_name}/       ← cloned repository
///         └── artifact.zip       ← artifacts generated via ../
/// ```
///
/// This structure ensures that artifacts generated outside the repo
/// (via `../` in build scripts) stay within the project directory,
/// keeping `repos/` clean and organized.
pub struct RepoCache {
    cache_dir: PathBuf,
}

impl RepoCache {
    /// Create a new repository cache.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Get or clone a repository.
    ///
    /// If the repository already exists in the cache, it will be opened.
    /// Otherwise, it will be cloned from the URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The repository URL to clone from
    /// * `project_name` - The project identifier (e.g., "backwpup")
    /// * `repo_name` - The repository name (e.g., "backwpup-pro")
    ///
    /// # Path Structure
    ///
    /// ```text
    /// {cache_dir}/repos/{project_name}/{repo_name}/
    /// ```
    pub fn get_or_clone(
        &self,
        url: &str,
        project_name: &str,
        repo_name: &str,
    ) -> Result<Repository> {
        let repo_path = self.repo_path(project_name, repo_name);

        if repo_path.exists() {
            Repository::open(&repo_path)
        } else {
            std::fs::create_dir_all(&repo_path)?;
            Repository::clone(url, &repo_path)
        }
    }

    /// Get the path where a repository would be cached.
    ///
    /// # Path Structure
    ///
    /// ```text
    /// {cache_dir}/repos/{project_name}/{repo_name}
    /// ```
    pub fn repo_path(&self, project_name: &str, repo_name: &str) -> PathBuf {
        self.cache_dir
            .join("repos")
            .join(project_name)
            .join(repo_name)
    }

    /// Get the project directory path (parent of repo).
    ///
    /// # Path Structure
    ///
    /// ```text
    /// {cache_dir}/repos/{project_name}
    /// ```
    ///
    /// This is where artifacts generated with `../` will land.
    pub fn project_path(&self, project_name: &str) -> PathBuf {
        self.cache_dir.join("repos").join(project_name)
    }

    /// Clear the entire repository cache.
    pub fn clear(&self) -> Result<()> {
        let repos_dir = self.cache_dir.join("repos");
        if repos_dir.exists() {
            std::fs::remove_dir_all(&repos_dir)?;
        }
        Ok(())
    }

    /// Clear cache for a specific project (repo + artifacts).
    pub fn clear_project(&self, project_name: &str) -> Result<()> {
        let project_dir = self.project_path(project_name);
        if project_dir.exists() {
            std::fs::remove_dir_all(&project_dir)?;
        }
        Ok(())
    }

    /// Get the cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}
