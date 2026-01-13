//! Project registry.

use std::collections::HashMap;

use crate::build::plugins::{BackWPupBuilder, Builder};
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
            builder: Box::new(BackWPupBuilder),
        });

        // Add more projects here as you implement their builders
        // registry.register(Project { ... });

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
