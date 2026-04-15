//! GitHub token resolution.
//!
//! Resolves GitHub tokens from multiple sources with a defined priority:
//!
//! 1. **Explicit config** - Token set in APVM config file
//! 2. **Environment variables** - `GITHUB_TOKEN` or `GH_TOKEN`
//! 3. **gh CLI config** - Reads from `~/.config/gh/hosts.yml`
//!
//! # Example
//!
//! ```rust,ignore
//! use apvm_core::git::token::resolve_github_token;
//!
//! // Will try all sources in priority order
//! let token = resolve_github_token(config.github_token.as_deref()).await;
//!
//! if let Some(token) = token {
//!     println!("Found token from: {:?}", token.source);
//! }
//! ```

use std::path::PathBuf;

use directories::BaseDirs;
use tokio::process::Command;
use tracing::{debug, trace, warn};

/// Source of a resolved GitHub token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// Token from APVM config file.
    Config,
    /// Token from `GITHUB_TOKEN` environment variable.
    EnvGithubToken,
    /// Token from `GH_TOKEN` environment variable.
    EnvGhToken,
    /// Token from `gh auth token` command (gh >= 2.17.0).
    GhAuthToken,
    /// Token from gh CLI config file (`~/.config/gh/hosts.yml`).
    GhConfigFile,
}

impl std::fmt::Display for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config => write!(f, "config file"),
            Self::EnvGithubToken => write!(f, "GITHUB_TOKEN env"),
            Self::EnvGhToken => write!(f, "GH_TOKEN env"),
            Self::GhAuthToken => write!(f, "gh auth token"),
            Self::GhConfigFile => write!(f, "gh config file"),
        }
    }
}

/// A resolved GitHub token with its source.
#[derive(Debug, Clone)]
pub struct ResolvedToken {
    /// The actual token value.
    pub token: String,
    /// Where the token was found.
    pub source: TokenSource,
}

impl ResolvedToken {
    /// Create a new resolved token.
    fn new(token: String, source: TokenSource) -> Self {
        Self { token, source }
    }
}

/// Resolve a GitHub token from multiple sources.
///
/// # Priority Order
///
/// 1. Explicit config token (if provided and non-empty)
/// 2. `GITHUB_TOKEN` environment variable
/// 3. `GH_TOKEN` environment variable  
/// 4. `gh auth token` command (gh CLI >= 2.17.0)
/// 5. gh CLI config file (`~/.config/gh/hosts.yml`)
///
/// # Arguments
///
/// * `config_token` - Optional token from config file
///
/// # Returns
///
/// Returns `Some(ResolvedToken)` if a token was found, `None` otherwise.
///
/// # Example
///
/// ```rust,ignore
/// // With explicit config token
/// let token = resolve_github_token(Some("ghp_xxx")).await;
///
/// // Without config token (will try env and gh CLI)
/// let token = resolve_github_token(None).await;
/// ```
pub async fn resolve_github_token(config_token: Option<&str>) -> Option<ResolvedToken> {
    // 1. Explicit config token
    if let Some(token) = config_token
        && !token.is_empty()
    {
        debug!("Using GitHub token from config file");
        return Some(ResolvedToken::new(token.to_string(), TokenSource::Config));
    }

    // 2. GITHUB_TOKEN environment variable
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        debug!("Using GitHub token from GITHUB_TOKEN env");
        return Some(ResolvedToken::new(token, TokenSource::EnvGithubToken));
    }

    // 3. GH_TOKEN environment variable
    if let Ok(token) = std::env::var("GH_TOKEN")
        && !token.is_empty()
    {
        debug!("Using GitHub token from GH_TOKEN env");
        return Some(ResolvedToken::new(token, TokenSource::EnvGhToken));
    }

    // 4. Try `gh auth token` command (gh >= 2.17.0)
    if let Some(token) = try_gh_auth_token().await {
        debug!("Using GitHub token from 'gh auth token' command");
        return Some(ResolvedToken::new(token, TokenSource::GhAuthToken));
    }

    // 5. Read from gh CLI config file
    if let Some(token) = read_gh_config_token() {
        debug!("Using GitHub token from gh config file");
        return Some(ResolvedToken::new(token, TokenSource::GhConfigFile));
    }

    debug!("No GitHub token found from any source");
    None
}

/// Try to get token using `gh auth token` command.
///
/// This command is available in gh CLI >= 2.17.0 (December 2022).
async fn try_gh_auth_token() -> Option<String> {
    trace!("Trying 'gh auth token' command");

    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .ok()?;

    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() && token.starts_with("gh") {
            return Some(token);
        }
    }

    trace!("'gh auth token' not available (older gh version?)");
    None
}

/// Read token from gh CLI config file.
///
/// gh CLI stores authentication in `~/.config/gh/hosts.yml` (or platform equivalent).
///
/// # File Format
///
/// ```yaml
/// github.com:
///     oauth_token: ghp_xxxxxxxxxxxxxxxxxxxx
///     user: username
///     git_protocol: https
/// ```
///
/// # Locations
///
/// | Platform | Path |
/// |----------|------|
/// | Linux | `~/.config/gh/hosts.yml` |
/// | macOS | `~/.config/gh/hosts.yml` |
/// | Windows | `%APPDATA%\gh\hosts.yml` |
fn read_gh_config_token() -> Option<String> {
    let config_path = get_gh_config_path()?;

    trace!("Reading gh config from: {}", config_path.display());

    if !config_path.exists() {
        trace!("gh config file not found");
        return None;
    }

    let content = std::fs::read_to_string(&config_path).ok()?;

    // Parse the YAML manually (simple case - avoid adding yaml dependency)
    // We're looking for oauth_token under github.com
    parse_gh_hosts_yaml(&content)
}

/// Get the path to gh CLI hosts config file.
///
/// Uses the `directories` crate for cross-platform config paths.
///
/// # Resolution Order
///
/// 1. `GH_CONFIG_DIR` environment variable (if set)
/// 2. Platform config directory via `directories::BaseDirs`:
///    - Linux/macOS: `~/.config/gh/hosts.yml`
///    - Windows: `%APPDATA%\gh\hosts.yml`
///
/// # Sources
///
/// - [gh CLI config docs](https://cli.github.com/manual/gh_config)
/// - [directories crate](https://docs.rs/directories)
fn get_gh_config_path() -> Option<PathBuf> {
    // 1. Check GH_CONFIG_DIR environment variable first (gh CLI respects this)
    if let Ok(config_dir) = std::env::var("GH_CONFIG_DIR") {
        return Some(PathBuf::from(config_dir).join("hosts.yml"));
    }

    // 2. Use directories crate for platform-appropriate config path
    //    BaseDirs::config_dir() returns:
    //    - Linux: $XDG_CONFIG_HOME or ~/.config
    //    - macOS: ~/.config (gh uses XDG, not ~/Library)
    //    - Windows: %APPDATA% (e.g., C:\Users\<user>\AppData\Roaming)
    if let Some(base_dirs) = BaseDirs::new() {
        // gh CLI uses XDG on all Unix platforms, including macOS
        #[cfg(unix)]
        {
            // On Unix, gh uses $XDG_CONFIG_HOME/gh or ~/.config/gh
            let config_home = std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| base_dirs.home_dir().join(".config"));
            return Some(config_home.join("gh").join("hosts.yml"));
        }

        #[cfg(windows)]
        {
            // On Windows, gh uses %APPDATA%\gh
            return Some(base_dirs.config_dir().join("gh").join("hosts.yml"));
        }
    }

    warn!("Could not determine gh config path");
    None
}

/// Parse gh hosts.yml to extract oauth_token for github.com.
///
/// This is a simple parser that handles the common format without
/// requiring a full YAML parser dependency.
///
/// # Expected Format
///
/// ```yaml
/// github.com:
///     oauth_token: ghp_xxxxxxxxxxxxxxxxxxxx
///     user: username
///     git_protocol: https
/// ```
fn parse_gh_hosts_yaml(content: &str) -> Option<String> {
    let mut in_github_com_section = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Check if we're entering github.com section
        if trimmed == "github.com:" || trimmed.starts_with("github.com:") {
            in_github_com_section = true;
            continue;
        }

        // Check if we're entering a different host section (exit github.com)
        if !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !line.starts_with(' ')
            && !line.starts_with('\t')
            && in_github_com_section
            && !trimmed.starts_with("oauth_token")
        {
            // We hit another top-level key, exit github.com section
            in_github_com_section = false;
        }

        // Look for oauth_token within github.com section
        if in_github_com_section && let Some(token) = extract_oauth_token(trimmed) {
            return Some(token);
        }
    }

    None
}

/// Extract oauth_token value from a YAML line.
fn extract_oauth_token(line: &str) -> Option<String> {
    // Handle both formats:
    // - oauth_token: ghp_xxx
    // - oauth_token: "ghp_xxx"
    // - oauth_token: 'ghp_xxx'

    let prefix = "oauth_token:";
    if !line.starts_with(prefix) {
        return None;
    }

    let value = line[prefix.len()..].trim();

    // Remove quotes if present
    let token = if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };

    if token.is_empty() {
        return None;
    }

    Some(token.to_string())
}

/// Check if a GitHub token appears to be valid format.
///
/// Does NOT validate against GitHub API, just format check.
///
/// # Token Formats
///
/// | Prefix | Type |
/// |--------|------|
/// | `ghp_` | Personal Access Token (classic) |
/// | `github_pat_` | Personal Access Token (fine-grained) |
/// | `gho_` | OAuth token |
/// | `ghu_` | User-to-server token |
/// | `ghs_` | Server-to-server token |
/// | `ghr_` | Refresh token |
pub fn is_valid_token_format(token: &str) -> bool {
    let valid_prefixes = ["ghp_", "github_pat_", "gho_", "ghu_", "ghs_", "ghr_"];

    valid_prefixes
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gh_hosts_yaml_simple() {
        let yaml = r#"
github.com:
    oauth_token: ghp_test123456789
    user: testuser
    git_protocol: https
"#;
        let token = parse_gh_hosts_yaml(yaml);
        assert_eq!(token, Some("ghp_test123456789".to_string()));
    }

    #[test]
    fn test_parse_gh_hosts_yaml_quoted() {
        let yaml = r#"
github.com:
    oauth_token: "ghp_quoted_token"
    user: testuser
"#;
        let token = parse_gh_hosts_yaml(yaml);
        assert_eq!(token, Some("ghp_quoted_token".to_string()));
    }

    #[test]
    fn test_parse_gh_hosts_yaml_single_quoted() {
        let yaml = r#"
github.com:
    oauth_token: 'ghp_single_quoted'
    user: testuser
"#;
        let token = parse_gh_hosts_yaml(yaml);
        assert_eq!(token, Some("ghp_single_quoted".to_string()));
    }

    #[test]
    fn test_parse_gh_hosts_yaml_multiple_hosts() {
        let yaml = r#"
github.com:
    oauth_token: ghp_github_token
    user: user1
enterprise.example.com:
    oauth_token: ghp_enterprise_token
    user: user2
"#;
        let token = parse_gh_hosts_yaml(yaml);
        assert_eq!(token, Some("ghp_github_token".to_string()));
    }

    #[test]
    fn test_parse_gh_hosts_yaml_no_github_com() {
        let yaml = r#"
enterprise.example.com:
    oauth_token: ghp_enterprise_token
    user: user1
"#;
        let token = parse_gh_hosts_yaml(yaml);
        assert_eq!(token, None);
    }

    #[test]
    fn test_parse_gh_hosts_yaml_empty() {
        let yaml = "";
        let token = parse_gh_hosts_yaml(yaml);
        assert_eq!(token, None);
    }

    #[test]
    fn test_is_valid_token_format() {
        // Valid formats
        assert!(is_valid_token_format("ghp_abc123"));
        assert!(is_valid_token_format("github_pat_abc123"));
        assert!(is_valid_token_format("gho_oauth_token"));
        assert!(is_valid_token_format("ghu_user_token"));
        assert!(is_valid_token_format("ghs_server_token"));
        assert!(is_valid_token_format("ghr_refresh_token"));

        // Invalid formats
        assert!(!is_valid_token_format("invalid_token"));
        assert!(!is_valid_token_format(""));
        assert!(!is_valid_token_format("gh_not_valid"));
    }

    #[test]
    fn test_extract_oauth_token() {
        assert_eq!(
            extract_oauth_token("oauth_token: ghp_abc123"),
            Some("ghp_abc123".to_string())
        );
        assert_eq!(
            extract_oauth_token("oauth_token: \"ghp_quoted\""),
            Some("ghp_quoted".to_string())
        );
        assert_eq!(
            extract_oauth_token("oauth_token: 'ghp_single'"),
            Some("ghp_single".to_string())
        );
        assert_eq!(extract_oauth_token("oauth_token:"), None);
        assert_eq!(extract_oauth_token("user: testuser"), None);
    }

    #[test]
    fn test_token_source_display() {
        assert_eq!(TokenSource::Config.to_string(), "config file");
        assert_eq!(TokenSource::EnvGithubToken.to_string(), "GITHUB_TOKEN env");
        assert_eq!(TokenSource::GhConfigFile.to_string(), "gh config file");
    }
}
