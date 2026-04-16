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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::BuildSource;
    use crate::store::{BuildMetadata, SourceArtifact};
    use tempfile::TempDir;

    /// Helper: create a store backed by a temp directory.
    fn temp_store() -> (TempDir, ArtifactStore) {
        let dir = TempDir::new().unwrap();
        let store = ArtifactStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    /// Helper: create a dummy source artifact and store it.
    fn store_build(
        dir: &std::path::Path,
        store: &ArtifactStore,
        project: &str,
        version: &str,
        commit: &str,
    ) {
        let name = format!("{project}-{version}-{commit}.zip");
        let path = dir.join(&name);
        std::fs::write(&path, format!("content {commit}")).unwrap();
        let artifact = SourceArtifact {
            variant_id: None,
            path,
            target_name: name,
        };
        let meta = BuildMetadata::new(
            project.to_string(),
            version.to_string(),
            BuildSource::Branch("main".into()),
            commit.to_string(),
            "main".to_string(),
        );
        store.store(&[artifact], &meta).unwrap();
    }

    // =========================================================================
    // 3.1 – Builder Chaining
    // =========================================================================

    #[test]
    fn test_query_builder_chaining() {
        let (_dir, store) = temp_store();
        let q = BuildQuery::new(&store)
            .project("wp-rocket")
            .version("3.17.4")
            .limit(5);
        assert_eq!(q.project, Some("wp-rocket".to_string()));
        assert_eq!(q.version, Some("3.17.4".to_string()));
        assert_eq!(q.limit, Some(5));
    }

    // =========================================================================
    // 3.2 – Execute on Empty Store
    // =========================================================================

    #[test]
    fn test_query_empty_store() {
        let (_dir, store) = temp_store();
        let results = store.query().project("wp-rocket").execute().unwrap();
        assert!(results.is_empty());
    }

    // =========================================================================
    // 3.3 – Filter by Project
    // =========================================================================

    #[test]
    fn test_query_filter_by_project() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "backwpup", "5.1.0", "bbb2222222222");

        let results = store.query().project("wp-rocket").execute().unwrap();
        // Query finds each build twice (real commit dir + source symlink)
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.manifest.project == "wp-rocket"));
    }

    // =========================================================================
    // 3.4 – Filter by Version
    // =========================================================================

    #[test]
    fn test_query_filter_by_version() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let results = store
            .query()
            .project("wp-rocket")
            .version("3.17.4")
            .execute()
            .unwrap();
        // Query finds each build twice (real commit dir + source symlink)
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.manifest.version == "3.17.4"));
    }

    // =========================================================================
    // 3.5 – Filter by Major.Minor
    // =========================================================================

    #[test]
    fn test_query_filter_by_major_minor() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.0", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "bbb2222222222");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "ccc3333333333");

        let results = store
            .query()
            .project("wp-rocket")
            .major_minor("3.17")
            .execute()
            .unwrap();
        // 2 builds × 2 (real + symlink) = 4
        assert_eq!(results.len(), 4);
        for r in &results {
            assert!(r.manifest.version.starts_with("3.17"));
        }
    }

    // =========================================================================
    // 3.6 – Limit
    // =========================================================================

    #[test]
    fn test_query_limit() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let results = store
            .query()
            .project("wp-rocket")
            .limit(1)
            .execute()
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    // =========================================================================
    // 3.7 – Latest
    // =========================================================================

    #[test]
    fn test_query_latest() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let latest = store.query().project("wp-rocket").latest().unwrap();
        assert!(latest.is_some());
    }

    // =========================================================================
    // 3.8 – Count
    // =========================================================================

    #[test]
    fn test_query_count() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let count = store.query().project("wp-rocket").count().unwrap();
        // 2 builds × 2 (real + symlink) = 4
        assert_eq!(count, 4);
    }

    // =========================================================================
    // 3.9 – Sorted Newest First
    // =========================================================================

    #[test]
    fn test_query_sorted_newest_first() {
        let (dir, store) = temp_store();
        // First build is older
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        // Small delay to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(10));
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let results = store.query().project("wp-rocket").execute().unwrap();
        // 2 builds × 2 (real + symlink) = 4
        assert_eq!(results.len(), 4);
        // Results sorted by built_at descending
        for w in results.windows(2) {
            assert!(w[0].manifest.built_at >= w[1].manifest.built_at);
        }
    }

    // =========================================================================
    // 3.10 – Latest on Empty
    // =========================================================================

    #[test]
    fn test_query_latest_on_empty() {
        let (_dir, store) = temp_store();
        let latest = store.query().project("nope").latest().unwrap();
        assert!(latest.is_none());
    }

    // =========================================================================
    // 3.11 – No Filters Returns All
    // =========================================================================

    #[test]
    fn test_query_no_filters() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "backwpup", "5.1.0", "bbb2222222222");

        let results = store.query().execute().unwrap();
        // 2 builds × 2 (real + symlink) = 4
        assert_eq!(results.len(), 4);
    }
}
