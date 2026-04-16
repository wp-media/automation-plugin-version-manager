//! Query builder for finding builds.
//!
//! Provides a fluent interface for querying stored builds with filters.

use walkdir::WalkDir;

use crate::error::Result;
use crate::manifest::BuildManifest;
use crate::store::{ArtifactStore, StoredBuild};

/// Query builder for finding builds.
///
/// # Example
///
/// ```ignore
/// let builds = store.query()
///     .project("backwpup")
///     .version("5.6.0")
///     .execute()?;
///
/// let latest = store.query()
///     .project("backwpup")
///     .latest()?;
/// ```
pub struct BuildQuery<'a> {
    store: &'a ArtifactStore,
    project: Option<String>,
    version: Option<String>,
    major_minor: Option<String>,
    limit: Option<usize>,
}

impl<'a> BuildQuery<'a> {
    /// Create a new query builder.
    pub fn new(store: &'a ArtifactStore) -> Self {
        Self {
            store,
            project: None,
            version: None,
            major_minor: None,
            limit: None,
        }
    }

    /// Filter by project name.
    pub fn project(mut self, project: &str) -> Self {
        self.project = Some(project.to_string());
        self
    }

    /// Filter by exact version.
    pub fn version(mut self, version: &str) -> Self {
        self.version = Some(version.to_string());
        self
    }

    /// Filter by major.minor version.
    pub fn major_minor(mut self, major_minor: &str) -> Self {
        self.major_minor = Some(major_minor.to_string());
        self
    }

    /// Limit the number of results.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Execute the query and return matching builds.
    pub fn execute(self) -> Result<Vec<StoredBuild>> {
        let mut results = Vec::new();
        let base = self.store.base_dir();

        if !base.exists() {
            return Ok(results);
        }

        // Determine the starting directory based on filters (optimize search)
        let start_dir = match (&self.project, &self.version, &self.major_minor) {
            // Most specific: project + version
            (Some(project), Some(version), _) => self.store.paths().version_dir(project, version),
            // Project + major.minor (walk versions within major.minor)
            (Some(project), None, Some(major_minor)) => {
                self.store.paths().major_minor_dir(project, major_minor)
            }
            // Project only
            (Some(project), None, None) => self.store.paths().project_dir(project),
            // No filters, walk everything
            _ => base.to_path_buf(),
        };

        if !start_dir.exists() {
            return Ok(results);
        }

        // Walk the directory tree looking for manifests
        for entry in WalkDir::new(&start_dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            // Check if this directory contains a manifest
            let manifest_path = entry.path().join(BuildManifest::FILENAME);

            if !manifest_path.exists() {
                continue;
            }

            // Load manifest
            let manifest = match BuildManifest::load(entry.path()) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Apply filters
            if let Some(ref project) = self.project
                && &manifest.project != project
            {
                continue;
            }

            if let Some(ref version) = self.version
                && &manifest.version != version
            {
                continue;
            }

            if let Some(ref major_minor) = self.major_minor {
                let build_mm = crate::path::major_minor(&manifest.version);
                if &build_mm != major_minor {
                    continue;
                }
            }

            // Build file list
            let files = manifest
                .artifacts
                .iter()
                .map(|a| entry.path().join(&a.filename))
                .collect();

            results.push(StoredBuild {
                manifest,
                commit_dir: entry.path().to_path_buf(),
                files,
            });

            // Check limit
            if let Some(limit) = self.limit
                && results.len() >= limit
            {
                break;
            }
        }

        // Sort by build date (newest first)
        results.sort_by_key(|b| std::cmp::Reverse(b.manifest.built_at));

        // Apply limit after sorting
        if let Some(limit) = self.limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    /// Get the latest build matching the query.
    pub fn latest(self) -> Result<Option<StoredBuild>> {
        Ok(self.limit(1).execute()?.into_iter().next())
    }

    /// Count builds matching the query.
    pub fn count(self) -> Result<usize> {
        Ok(self.execute()?.len())
    }
}
