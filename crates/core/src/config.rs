//! Configuration loading and saving.

use std::path::PathBuf;
use std::sync::LazyLock;

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::error::Result;

static APVM_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".apvm"))
        .unwrap_or_else(|| PathBuf::from(".apvm"))
});

/// Main application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// GitHub Personal Access Token.
    pub github_token: Option<String>,
    /// Cache directory for cloned repositories.
    #[serde(default = "Config::default_cache_dir")]
    pub cache_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            github_token: None,
            cache_dir: Self::default_cache_dir(),
        }
    }
}

impl Config {
    /// Load configuration from the default location.
    pub fn load() -> Result<Self> {
        let config_path = Self::config_file_path();

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&config_path)?;
        let config: Config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Save configuration to the default location.
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_file_path();

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&config_path, content)?;
        Ok(())
    }

    /// Get the configuration file path (~/.apvm/config.json).
    pub fn config_file_path() -> PathBuf {
        APVM_DIR.join("config.json")
    }

    /// Get the default cache directory (~/.apvm/cache).
    pub fn default_cache_dir() -> PathBuf {
        APVM_DIR.join("cache")
    }

    /// Get the APVM directory (~/.apvm).
    pub fn apvm_dir() -> &'static PathBuf {
        &APVM_DIR
    }
}
