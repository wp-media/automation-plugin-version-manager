//! Project registry.

use std::collections::HashMap;

use crate::build::plugins::Builder;
use crate::error::{Error, Result};

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
