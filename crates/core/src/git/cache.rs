//! Repository cache management.

use std::path::{Path, PathBuf};

use crate::error::Result;

use super::repository::Repository;

/// Manages a cache of cloned repositories.
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
    pub fn get_or_clone(&self, url: &str, name: &str) -> Result<Repository> {
        let repo_path = self.repo_path(name);

        if repo_path.exists() {
            Repository::open(&repo_path)
        } else {
            std::fs::create_dir_all(&repo_path)?;
            Repository::clone(url, &repo_path)
        }
    }

    /// Get the path where a repository would be cached.
    pub fn repo_path(&self, name: &str) -> PathBuf {
        self.cache_dir.join("repos").join(name)
    }

    /// Clear the entire cache.
    pub fn clear(&self) -> Result<()> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }

    /// Get the cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}
