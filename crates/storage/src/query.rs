//! Query builder for finding builds.
//!
//! Provides a fluent interface for querying stored builds with filters.
//!
//! The query enumerates the store *structurally* (projects → major.minor →
//! versions → `by-commit` directories) instead of walking the whole tree:
//! `by-source` link directories are never traversed, so every build is seen
//! exactly once, and the `releases/` cache subtree is naturally excluded.
//!
//! Queries are an inventory tool: they list what manifests record and do
//! not integrity-check files. Use [`crate::store::StoredBuild::verify`] or
//! the cache-oriented [`crate::lookup`] API when health matters.

use crate::error::Result;
use crate::manifest::short_commit;
use crate::path::{BuildSource, major_minor};
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
    source: Option<BuildSource>,
    commit: Option<String>,
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
            source: None,
            commit: None,
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

    /// Filter by build source (matches by source identity, including
    /// lossy round-tripped values — see [`BuildSource::from_dir_name`]).
    pub fn source(mut self, source: &BuildSource) -> Self {
        self.source = Some(source.clone());
        self
    }

    /// Filter by commit hash prefix (short or full, any case).
    pub fn commit(mut self, commit: &str) -> Self {
        self.commit = Some(commit.to_ascii_lowercase());
        self
    }

    /// Limit the number of results (applied *after* sorting, so
    /// `.limit(1)` returns the newest match).
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Execute the query and return matching builds, newest first.
    pub fn execute(self) -> Result<Vec<StoredBuild>> {
        let mut results = Vec::new();

        // Project scope: explicit filter or every project in the store.
        let projects = match &self.project {
            Some(p) => vec![p.clone()],
            None => self.store.list_projects()?,
        };

        for project in &projects {
            // Version scope: exact filter, or all versions (optionally
            // narrowed to one major.minor). Invalid filter values match
            // nothing; real IO errors propagate.
            let versions: Vec<String> = match &self.version {
                Some(v) => vec![v.clone()],
                None => match self.store.list_versions(project) {
                    Ok(versions) => versions
                        .into_iter()
                        .filter(|v| {
                            self.major_minor
                                .as_ref()
                                .is_none_or(|mm| &major_minor(v) == mm)
                        })
                        .collect(),
                    Err(crate::error::Error::InvalidInput(_)) => continue,
                    Err(e) => return Err(e),
                },
            };

            for version in &versions {
                let commits = match self.store.list_commits(project, version) {
                    Ok(commits) => commits,
                    Err(crate::error::Error::InvalidInput(_)) => continue,
                    Err(e) => return Err(e),
                };
                for commit_name in commits {
                    if let Some(build) = self.load_if_matching(project, version, &commit_name)? {
                        results.push(build);
                    }
                }
            }
        }

        // Sort by build date (newest first) BEFORE applying the limit, so a
        // limited query returns the newest matches rather than arbitrary ones.
        results.sort_by_key(|b| std::cmp::Reverse(b.manifest.built_at));
        if let Some(limit) = self.limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    /// Load one commit directory and apply the per-build filters.
    fn load_if_matching(
        &self,
        project: &str,
        version: &str,
        commit_name: &str,
    ) -> Result<Option<StoredBuild>> {
        // Cheap directory-name prefix check before touching the manifest.
        if let Some(filter) = &self.commit {
            let short = short_commit(filter);
            if !commit_name.starts_with(short.as_str()) && !short.starts_with(commit_name) {
                return Ok(None);
            }
        }

        let commit_dir = self.store.paths().commit_dir(project, version, commit_name);
        let Some(build) = self.store.load_stored_build(&commit_dir, None)? else {
            return Ok(None);
        };

        // Full-precision commit check against the manifest's full hash.
        if let Some(filter) = &self.commit
            && !build.manifest.commit.starts_with(filter.as_str())
            && !filter.starts_with(build.manifest.commit.as_str())
        {
            return Ok(None);
        }

        // Source filter: identity or directory-name match (tolerates lossy
        // round-tripped sources).
        if let Some(filter) = &self.source {
            let dir_name = filter.to_dir_name();
            let matches = build
                .manifest
                .sources
                .iter()
                .any(|s| s.source == *filter || s.source.to_dir_name() == dir_name);
            if !matches {
                return Ok(None);
            }
        }

        Ok(Some(build))
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
    use crate::store::{BuildMetadata, SourceArtifact};
    use tempfile::TempDir;

    /// Helper: create a store backed by a temp directory.
    fn temp_store() -> (TempDir, ArtifactStore) {
        let dir = TempDir::new().unwrap();
        let store = ArtifactStore::new(dir.path().join("store"));
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
        store_build_with_source(
            dir,
            store,
            project,
            version,
            commit,
            BuildSource::Branch("main".into()),
        );
    }

    /// Helper: store a build with a specific source.
    fn store_build_with_source(
        dir: &std::path::Path,
        store: &ArtifactStore,
        project: &str,
        version: &str,
        commit: &str,
        source: BuildSource,
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
            source,
            commit.to_string(),
            "main".to_string(),
        );
        store.store(&[artifact], &meta).unwrap();
    }

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

    #[test]
    fn test_query_empty_store() {
        let (_dir, store) = temp_store();
        let results = store.query().project("wp-rocket").execute().unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_query_finds_each_build_exactly_once() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");

        // One stored build = one result: source links must NOT double-count.
        let results = store.query().execute().unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_query_filter_by_project() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "backwpup", "5.1.0", "bbb2222222222");

        let results = store.query().project("wp-rocket").execute().unwrap();
        assert_eq!(results.len(), 1);
        assert!(results.iter().all(|r| r.manifest.project == "wp-rocket"));
    }

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
        assert_eq!(results.len(), 1);
        assert!(results.iter().all(|r| r.manifest.version == "3.17.4"));
    }

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
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.manifest.version.starts_with("3.17"));
        }
    }

    #[test]
    fn test_query_filter_by_source() {
        let (dir, store) = temp_store();
        store_build_with_source(
            dir.path(),
            &store,
            "wp-rocket",
            "3.17.4",
            "aaa1111111111",
            BuildSource::PullRequest(42),
        );
        store_build_with_source(
            dir.path(),
            &store,
            "wp-rocket",
            "3.17.4",
            "bbb2222222222",
            BuildSource::Branch("develop".into()),
        );

        let results = store
            .query()
            .project("wp-rocket")
            .source(&BuildSource::PullRequest(42))
            .execute()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].manifest.commit_short, "aaa1111");
    }

    #[test]
    fn test_query_filter_by_commit_prefix() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        // Short prefix (< 7 chars) matches.
        let results = store.query().commit("aaa11").execute().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].manifest.commit_short, "aaa1111");

        // Full hash matches.
        let results = store.query().commit("bbb2222222222").execute().unwrap();
        assert_eq!(results.len(), 1);

        // Full hash that agrees on the short prefix but not the rest: no match.
        let results = store.query().commit("aaa1111999999").execute().unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_query_limit_returns_newest() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        std::thread::sleep(std::time::Duration::from_millis(10));
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let results = store
            .query()
            .project("wp-rocket")
            .limit(1)
            .execute()
            .unwrap();
        assert_eq!(results.len(), 1);
        // Limit is applied after sorting: newest build wins.
        assert_eq!(results[0].manifest.commit_short, "bbb2222");
    }

    #[test]
    fn test_query_latest() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        std::thread::sleep(std::time::Duration::from_millis(10));
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let latest = store.query().project("wp-rocket").latest().unwrap();
        assert_eq!(latest.unwrap().manifest.commit_short, "bbb2222");
    }

    #[test]
    fn test_query_count() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let count = store.query().project("wp-rocket").count().unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_query_sorted_newest_first() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        std::thread::sleep(std::time::Duration::from_millis(10));
        store_build(dir.path(), &store, "wp-rocket", "3.18.0", "bbb2222222222");

        let results = store.query().project("wp-rocket").execute().unwrap();
        assert_eq!(results.len(), 2);
        for w in results.windows(2) {
            assert!(w[0].manifest.built_at >= w[1].manifest.built_at);
        }
    }

    #[test]
    fn test_query_latest_on_empty() {
        let (_dir, store) = temp_store();
        let latest = store.query().project("nope").latest().unwrap();
        assert!(latest.is_none());
    }

    #[test]
    fn test_query_no_filters_returns_all() {
        let (dir, store) = temp_store();
        store_build(dir.path(), &store, "wp-rocket", "3.17.4", "aaa1111111111");
        store_build(dir.path(), &store, "backwpup", "5.1.0", "bbb2222222222");

        let results = store.query().execute().unwrap();
        assert_eq!(results.len(), 2);
    }
}
