//! Project registry.

use std::collections::HashMap;

use crate::build::plugins::{BackWPupBuilder, Builder, ImagifyBuilder, WpRocketBuilder};
use crate::error::{Error, Result};

pub const DEFAULT_BRANCH_NAME: &str = "develop";

/// A known project.
pub struct Project {
    /// Project name (short identifier).
    pub name: String,
    /// Repository URL.
    pub repo_url: String,
    /// Repository owner (GitHub).
    pub owner: String,
    /// Repository name (GitHub).
    pub repo: String,
    /// Default branch name.
    pub default_branch: String,
    /// Whether the repository is private (requires authentication).
    ///
    /// When `true`, build operations will fail early if no GitHub token
    /// is available, providing a clear error message instead of failing
    /// later during clone/fetch with a cryptic git error.
    pub is_private: bool,
    /// Whether this project publishes GitHub Releases with downloadable assets.
    ///
    /// When `true`, the `release:` prefix and automatic release detection
    /// are enabled for this project. When `false`, `release:` returns a
    /// clear error message suggesting git-based alternatives.
    pub has_releases: bool,
    /// Project-specific builder.
    pub builder: Box<dyn Builder>,
}

/// Registry of known projects.
pub struct ProjectRegistry {
    projects: HashMap<String, Project>,
}

impl ProjectRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            projects: HashMap::new(),
        }
    }

    /// Create registry with all known projects.
    pub fn with_known_projects() -> Self {
        let mut registry = Self::new();

        // Register all supported projects
        registry.register(Project {
            name: "backwpup".to_string(),
            repo_url: "https://github.com/wp-media/backwpup-pro.git".to_string(),
            owner: "wp-media".to_string(),
            repo: "backwpup-pro".to_string(),
            default_branch: DEFAULT_BRANCH_NAME.to_string(),
            is_private: true,
            has_releases: true,
            builder: Box::new(BackWPupBuilder),
        });

        registry.register(Project {
            name: "wp-rocket".to_string(),
            repo_url: "https://github.com/wp-media/wp-rocket.git".to_string(),
            owner: "wp-media".to_string(),
            repo: "wp-rocket".to_string(),
            default_branch: DEFAULT_BRANCH_NAME.to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(WpRocketBuilder),
        });

        // Imagify is a public repository whose version is embedded in the
        // `imagify.php` plugin header. Its GitHub Releases carry no downloadable
        // assets (distribution goes to WordPress.org SVN), so `has_releases` is
        // false: a specific released version is built from its tag
        // (e.g. `tag:v2.3.0`) rather than downloaded.
        registry.register(Project {
            name: "imagify".to_string(),
            repo_url: "https://github.com/wp-media/imagify-plugin.git".to_string(),
            owner: "wp-media".to_string(),
            repo: "imagify-plugin".to_string(),
            default_branch: DEFAULT_BRANCH_NAME.to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(ImagifyBuilder),
        });

        registry
    }

    /// Register a project.
    pub fn register(&mut self, project: Project) {
        self.projects.insert(project.name.clone(), project);
    }

    /// Get a project by name.
    pub fn get(&self, name: &str) -> Result<&Project> {
        self.projects
            .get(name)
            .ok_or_else(|| Error::ProjectNotFound(name.to_string()))
    }

    /// List all registered projects.
    pub fn list(&self) -> impl Iterator<Item = &Project> {
        self.projects.values()
    }
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::plugins::{BuildArtifact, Builder, VersionRequirement};
    use crate::build::progress::BuildStep;

    /// Minimal test builder.
    struct TestBuilder;

    impl Builder for TestBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            VersionRequirement::Optional
        }

        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }

        fn build_commands(
            &self,
            _context: &BuildContext,
            _version: &str,
            _variants: &[&str],
        ) -> Vec<BuildStep> {
            vec![]
        }

        fn artifacts(
            &self,
            _context: &BuildContext,
            _version: &str,
            _variants: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }
    }

    #[test]
    fn test_registry_new_is_empty() {
        let registry = ProjectRegistry::new();
        assert_eq!(registry.list().count(), 0);
    }

    #[test]
    fn test_registry_default_is_empty() {
        let registry = ProjectRegistry::default();
        assert_eq!(registry.list().count(), 0);
    }

    #[test]
    fn test_registry_with_known_projects_has_backwpup() {
        let registry = ProjectRegistry::with_known_projects();
        let project = registry.get("backwpup");
        assert!(project.is_ok());
        assert_eq!(project.unwrap().name, "backwpup");
    }

    #[test]
    fn test_registry_with_known_projects_has_imagify() {
        let registry = ProjectRegistry::with_known_projects();
        let project = registry.get("imagify").expect("imagify must be registered");
        assert_eq!(project.name, "imagify");
        assert_eq!(project.repo, "imagify-plugin");
        assert_eq!(project.owner, "wp-media");
        assert_eq!(project.default_branch, DEFAULT_BRANCH_NAME);
        // Public repo with an embedded version and no downloadable release assets.
        assert!(!project.is_private);
        assert!(!project.has_releases);
        assert!(
            project.builder.version_requirement().is_embedded(),
            "imagify version is embedded in imagify.php"
        );
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = ProjectRegistry::new();
        registry.register(Project {
            name: "test-plugin".to_string(),
            repo_url: "https://github.com/test/repo.git".to_string(),
            owner: "test".to_string(),
            repo: "repo".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        let project = registry.get("test-plugin").unwrap();
        assert_eq!(project.owner, "test");
        assert_eq!(project.repo, "repo");
        assert!(!project.is_private);
    }

    #[test]
    fn test_registry_get_not_found() {
        let registry = ProjectRegistry::new();
        let result = registry.get("nonexistent");
        assert!(result.is_err());

        let err_string = result.err().unwrap().to_string();
        assert!(err_string.contains("nonexistent"));
    }

    #[test]
    fn test_registry_list() {
        let mut registry = ProjectRegistry::new();

        registry.register(Project {
            name: "plugin-a".to_string(),
            repo_url: "https://github.com/test/a.git".to_string(),
            owner: "test".to_string(),
            repo: "a".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        registry.register(Project {
            name: "plugin-b".to_string(),
            repo_url: "https://github.com/test/b.git".to_string(),
            owner: "test".to_string(),
            repo: "b".to_string(),
            default_branch: "develop".to_string(),
            is_private: true,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        let names: Vec<_> = registry.list().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"plugin-a"));
        assert!(names.contains(&"plugin-b"));
    }

    #[test]
    fn test_registry_register_overwrites() {
        let mut registry = ProjectRegistry::new();

        registry.register(Project {
            name: "plugin".to_string(),
            repo_url: "https://github.com/test/old.git".to_string(),
            owner: "test".to_string(),
            repo: "old".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        registry.register(Project {
            name: "plugin".to_string(),
            repo_url: "https://github.com/test/new.git".to_string(),
            owner: "test".to_string(),
            repo: "new".to_string(),
            default_branch: "develop".to_string(),
            is_private: true,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        // Should only have 1 project (overwritten)
        assert_eq!(registry.list().count(), 1);

        let project = registry.get("plugin").unwrap();
        assert_eq!(project.repo, "new");
        assert!(project.is_private);
    }
}
