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
    // 1-3. Config, then GITHUB_TOKEN, then GH_TOKEN (cheap, no I/O)
    if let Some(resolved) = token_from_explicit_sources(
        config_token,
        std::env::var("GITHUB_TOKEN").ok(),
        std::env::var("GH_TOKEN").ok(),
    ) {
        return Some(resolved);
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

/// Pick a token from the explicit sources, in priority order: config file,
/// then `GITHUB_TOKEN`, then `GH_TOKEN`. Empty values are skipped.
///
/// Pure (the caller reads the environment) so the precedence contract is
/// unit-testable without mutating process-global env vars.
///
/// # Arguments
///
/// * `config_token` - Token from the APVM config file, if any
/// * `github_token_env` - Value of `GITHUB_TOKEN`, if set and valid UTF-8
/// * `gh_token_env` - Value of `GH_TOKEN`, if set and valid UTF-8
///
/// # Returns
///
/// The first non-empty token with its source, or `None`.
fn token_from_explicit_sources(
    config_token: Option<&str>,
    github_token_env: Option<String>,
    gh_token_env: Option<String>,
) -> Option<ResolvedToken> {
    if let Some(token) = config_token
        && !token.is_empty()
    {
        debug!("Using GitHub token from config file");
        return Some(ResolvedToken::new(token.to_string(), TokenSource::Config));
    }

    if let Some(token) = github_token_env
        && !token.is_empty()
    {
        debug!("Using GitHub token from GITHUB_TOKEN env");
        return Some(ResolvedToken::new(token, TokenSource::EnvGithubToken));
    }

    if let Some(token) = gh_token_env
        && !token.is_empty()
    {
        debug!("Using GitHub token from GH_TOKEN env");
        return Some(ResolvedToken::new(token, TokenSource::EnvGhToken));
    }

    None
}

/// Try to get token using `gh auth token` command.
///
/// This command is available in gh CLI >= 2.17.0 (December 2022).
async fn try_gh_auth_token() -> Option<String> {
    trace!("Trying 'gh auth token' command");

    let output = crate::process::output(crate::process::command("gh").args(["auth", "token"]))
        .await
        .ok()?;

    if output.status.success()
        && let Some(token) = parse_gh_auth_token_stdout(&output.stdout)
    {
        return Some(token);
    }

    trace!("'gh auth token' not available (older gh version?)");
    None
}

/// Extract a token from successful `gh auth token` stdout.
///
/// Accepts only a single value in the GitHub token alphabet
/// ([`has_token_alphabet`]) with a known prefix ([`is_valid_token_format`]) —
/// including fine-grained `github_pat_` tokens, which `gh auth login
/// --with-token` stores as-is — so unrelated or decorated output from an old
/// or wrapped `gh` is never mistaken for a credential.
///
/// # Arguments
///
/// * `stdout` - Raw stdout of `gh auth token`
///
/// # Returns
///
/// The trimmed token, or `None` when the output is not exactly one token.
fn parse_gh_auth_token_stdout(stdout: &[u8]) -> Option<String> {
    let token = String::from_utf8_lossy(stdout).trim().to_string();
    (has_token_alphabet(&token) && is_valid_token_format(&token)).then_some(token)
}

/// Whether `token` is non-empty and uses only the GitHub token alphabet
/// (`[A-Za-z0-9_]`), for tokens apvm discovers on its own (`gh auth token`,
/// `hosts.yml`).
///
/// Such a token is embedded in `https://x-access-token:<token>@…` URLs, so
/// characters like `@`, `/`, `:` or `#` could redirect the credential to
/// another host or path; whitespace, control or escape characters only mean
/// the value is not a bare token.
fn has_token_alphabet(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    read_gh_hosts_file(&config_path)
}

/// Read and parse a gh `hosts.yml` at `config_path`.
///
/// Returns `None` when the file is missing, unreadable, or has no
/// `github.com` `oauth_token` in the GitHub token alphabet
/// ([`has_token_alphabet`]) — never an error, since this is only a fallback
/// source. No prefix is required: tokens issued before GitHub's 2021 format
/// change have none and still work.
fn read_gh_hosts_file(config_path: &std::path::Path) -> Option<String> {
    trace!("Reading gh config from: {}", config_path.display());

    if !config_path.exists() {
        trace!("gh config file not found");
        return None;
    }

    let content = std::fs::read_to_string(config_path).ok()?;
    // A leading BOM (Windows editors) is not whitespace to `trim`, so it
    // would hide the first section.
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);

    // Parse the YAML manually (simple case - avoid adding yaml dependency)
    // We're looking for oauth_token under github.com
    parse_gh_hosts_yaml(content).filter(|token| has_token_alphabet(token))
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
    gh_hosts_path(
        std::env::var("GH_CONFIG_DIR").ok(),
        std::env::var("XDG_CONFIG_HOME").ok(),
        BaseDirs::new().as_ref(),
    )
}

/// Compute the gh `hosts.yml` path from already-read inputs (see
/// [`get_gh_config_path`] for the resolution order).
///
/// Pure so the precedence is unit-testable without mutating process env.
///
/// # Arguments
///
/// * `gh_config_dir` - Value of `GH_CONFIG_DIR`, if set
/// * `xdg_config_home` - Value of `XDG_CONFIG_HOME`, if set (Unix only)
/// * `base_dirs` - Platform directories, if the home dir is known
#[cfg_attr(windows, allow(unused_variables))]
fn gh_hosts_path(
    gh_config_dir: Option<String>,
    xdg_config_home: Option<String>,
    base_dirs: Option<&BaseDirs>,
) -> Option<PathBuf> {
    // 1. Check GH_CONFIG_DIR environment variable first (gh CLI respects this)
    if let Some(config_dir) = gh_config_dir {
        return Some(PathBuf::from(config_dir).join("hosts.yml"));
    }

    // 2. Use directories crate for platform-appropriate config path
    //    BaseDirs::config_dir() returns:
    //    - Linux: $XDG_CONFIG_HOME or ~/.config
    //    - macOS: ~/.config (gh uses XDG, not ~/Library)
    //    - Windows: %APPDATA% (e.g., C:\Users\<user>\AppData\Roaming)
    if let Some(base_dirs) = base_dirs {
        // gh CLI uses XDG on all Unix platforms, including macOS
        #[cfg(unix)]
        {
            // On Unix, gh uses $XDG_CONFIG_HOME/gh or ~/.config/gh
            let config_home = xdg_config_home
                .map(PathBuf::from)
                .unwrap_or_else(|| base_dirs.home_dir().join(".config"));
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

/// Prefixes of every documented GitHub token kind — the single list both the
/// token lookup and the CLI's `config set token` check use.
///
/// | Prefix | Type |
/// |--------|------|
/// | `ghp_` | Personal Access Token (classic) |
/// | `gho_` | OAuth token |
/// | `ghu_` | User-to-server token |
/// | `ghs_` | Server-to-server token |
/// | `ghr_` | Refresh token |
/// | `github_pat_` | Personal Access Token (fine-grained) |
///
/// Source: <https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/about-authentication-to-github#githubs-token-formats>
pub const KNOWN_TOKEN_PREFIXES: &[&str] = &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];

/// Check if a GitHub token appears to be valid format: it starts with one of
/// the [`KNOWN_TOKEN_PREFIXES`].
///
/// Does NOT validate against GitHub API, just format check.
pub fn is_valid_token_format(token: &str) -> bool {
    KNOWN_TOKEN_PREFIXES
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

    #[test]
    fn test_token_source_display_all_variants() {
        // Shown to users (e.g. "GitHub token from: …"); keep wording stable.
        assert_eq!(TokenSource::EnvGhToken.to_string(), "GH_TOKEN env");
        assert_eq!(TokenSource::GhAuthToken.to_string(), "gh auth token");
    }

    // =========================================================================
    // Explicit-source precedence: config → GITHUB_TOKEN → GH_TOKEN
    // =========================================================================

    /// Shorthand: resolve from explicit sources and return `(token, source)`.
    fn explicit(
        config: Option<&str>,
        github: Option<&str>,
        gh: Option<&str>,
    ) -> Option<(String, TokenSource)> {
        token_from_explicit_sources(config, github.map(String::from), gh.map(String::from))
            .map(|r| (r.token, r.source))
    }

    #[test]
    fn explicit_sources_follow_documented_precedence() {
        assert_eq!(
            explicit(Some("cfg"), Some("gh_env"), Some("gh2")),
            Some(("cfg".into(), TokenSource::Config))
        );
        assert_eq!(
            explicit(None, Some("gh_env"), Some("gh2")),
            Some(("gh_env".into(), TokenSource::EnvGithubToken))
        );
        assert_eq!(
            explicit(None, None, Some("gh2")),
            Some(("gh2".into(), TokenSource::EnvGhToken))
        );
        assert_eq!(explicit(None, None, None), None);
    }

    #[test]
    fn explicit_sources_skip_empty_values() {
        // An empty config value or `GITHUB_TOKEN=` must not shadow a real
        // token further down the chain (nor yield an empty credential).
        assert_eq!(
            explicit(Some(""), Some(""), Some("gh2")),
            Some(("gh2".into(), TokenSource::EnvGhToken))
        );
        assert_eq!(
            explicit(Some(""), Some("env"), None),
            Some(("env".into(), TokenSource::EnvGithubToken))
        );
        assert_eq!(explicit(Some(""), Some(""), Some("")), None);
    }

    #[tokio::test]
    async fn resolve_prefers_non_empty_config_token_without_other_lookups() {
        let resolved = resolve_github_token(Some("ghp_from_config")).await.unwrap();
        assert_eq!(resolved.token, "ghp_from_config");
        assert_eq!(resolved.source, TokenSource::Config);
    }

    // =========================================================================
    // `gh auth token` output parsing
    // =========================================================================

    #[test]
    fn gh_auth_token_stdout_accepts_every_known_token_kind() {
        // `gh auth login --with-token` stores whatever token it is given, so
        // `gh auth token` can print a fine-grained PAT, not just `gho_`.
        for token in [
            "gho_oauth",
            "ghp_classic",
            "ghu_user",
            "ghs_server",
            "ghr_refresh",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz",
        ] {
            let stdout = format!("{token}\n");
            assert_eq!(
                parse_gh_auth_token_stdout(stdout.as_bytes()).as_deref(),
                Some(token)
            );
        }
    }

    #[test]
    fn gh_auth_token_stdout_rejects_anything_but_a_single_token() {
        // Extra lines or words mean the output is not (only) a token — e.g. a
        // wrapper script printing a banner — and must not be sent to GitHub.
        for stdout in [
            "gho_abc\nnotice: something else\n",
            "ghp_abc def",
            "gh version 2.0.0",
            "ghost",
        ] {
            assert_eq!(
                parse_gh_auth_token_stdout(stdout.as_bytes()),
                None,
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn discovered_tokens_must_use_the_github_token_alphabet() {
        // Auto-discovered tokens are embedded in `https://x-access-token:<t>@`
        // URLs: `@`, `/`, `:` or `#` would move the credential to another
        // host or path, and escape codes from a wrapped `gh` only cause
        // confusing 401s. GitHub tokens use `[A-Za-z0-9_]` only.
        for stdout in [
            "gho_x@evil.example/",
            "gho_x:y",
            "gho_x#frag",
            "gho_x\u{1b}[0m",
            "gho_x\0",
            "gho_\u{fffd}",
        ] {
            assert_eq!(
                parse_gh_auth_token_stdout(stdout.as_bytes()),
                None,
                "{stdout:?}"
            );
        }

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hosts.yml");
        std::fs::write(&path, "github.com:\n    oauth_token: ghp_x@evil.example/\n").unwrap();
        assert_eq!(read_gh_hosts_file(&path), None);
    }

    #[test]
    fn hosts_file_with_a_byte_order_mark_still_yields_its_token() {
        // A leading BOM (from a Windows editor) is not whitespace to `trim`,
        // so the `github.com:` section was never found.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hosts.yml");
        std::fs::write(&path, "\u{feff}github.com:\n    oauth_token: gho_bom\n").unwrap();
        assert_eq!(read_gh_hosts_file(&path).as_deref(), Some("gho_bom"));
    }

    #[test]
    fn hosts_file_keeps_legacy_tokens_without_a_prefix() {
        // Tokens issued before GitHub's 2021 format change have no prefix but
        // still work; the hosts.yml fallback has always accepted them.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hosts.yml");
        let legacy = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(&path, format!("github.com:\n    oauth_token: {legacy}\n")).unwrap();
        assert_eq!(read_gh_hosts_file(&path).as_deref(), Some(legacy));
    }

    #[test]
    fn known_prefixes_are_listed_in_documentation_order() {
        // The CLI prints this list in its unknown-prefix warning; keep the
        // order the READMEs and the skill use.
        assert_eq!(
            KNOWN_TOKEN_PREFIXES,
            ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"]
        );
    }

    #[test]
    fn known_prefixes_drive_the_format_check() {
        for prefix in KNOWN_TOKEN_PREFIXES {
            assert!(is_valid_token_format(&format!("{prefix}abc")), "{prefix}");
        }
        assert!(!is_valid_token_format("gh_abc"));
        assert!(!is_valid_token_format("token"));
    }

    #[test]
    fn gh_auth_token_stdout_accepts_trimmed_gh_tokens_only() {
        assert_eq!(
            parse_gh_auth_token_stdout(b"gho_abc123\n"),
            Some("gho_abc123".into())
        );
        assert_eq!(
            parse_gh_auth_token_stdout(b"  ghp_x  \r\n"),
            Some("ghp_x".into())
        );
        assert_eq!(parse_gh_auth_token_stdout(b""), None);
        assert_eq!(parse_gh_auth_token_stdout(b"  \n"), None);
        // Unrelated text (e.g. an old gh printing help) is not a token.
        assert_eq!(
            parse_gh_auth_token_stdout(b"unknown command \"token\""),
            None
        );
    }

    // =========================================================================
    // gh hosts.yml location and reading
    // =========================================================================

    #[test]
    fn gh_hosts_path_prefers_gh_config_dir() {
        let base = BaseDirs::new();
        assert_eq!(
            gh_hosts_path(Some("/gh/dir".into()), Some("/xdg".into()), base.as_ref()),
            Some(PathBuf::from("/gh/dir/hosts.yml"))
        );
        // GH_CONFIG_DIR alone is enough, even without a home directory.
        assert_eq!(
            gh_hosts_path(Some("/gh/dir".into()), None, None),
            Some(PathBuf::from("/gh/dir/hosts.yml"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn gh_hosts_path_uses_xdg_then_home_config_on_unix() {
        let base = BaseDirs::new().expect("tests need a home directory");
        assert_eq!(
            gh_hosts_path(None, Some("/xdg".into()), Some(&base)),
            Some(PathBuf::from("/xdg/gh/hosts.yml"))
        );
        // gh uses ~/.config on macOS too — never ~/Library/Application Support.
        assert_eq!(
            gh_hosts_path(None, None, Some(&base)),
            Some(base.home_dir().join(".config/gh/hosts.yml"))
        );
    }

    #[test]
    fn gh_hosts_path_is_none_without_any_base() {
        assert_eq!(gh_hosts_path(None, Some("/xdg".into()), None), None);
    }

    #[test]
    fn read_gh_hosts_file_reads_token_or_degrades_to_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hosts.yml");

        // Missing file → None (fallback source, never an error).
        assert_eq!(read_gh_hosts_file(&path), None);

        std::fs::write(&path, "github.com:\n    oauth_token: gho_file\n").unwrap();
        assert_eq!(read_gh_hosts_file(&path), Some("gho_file".into()));

        // Unreadable as text (a directory) → None.
        assert_eq!(read_gh_hosts_file(dir.path()), None);
    }

    #[test]
    fn parse_gh_hosts_yaml_ignores_tokens_of_hosts_listed_before_github() {
        let yaml = "\
enterprise.example.com:
    oauth_token: ghp_enterprise
github.com:
    user: me
    oauth_token: ghp_public
";
        assert_eq!(parse_gh_hosts_yaml(yaml), Some("ghp_public".into()));
    }

    #[test]
    fn parse_gh_hosts_yaml_without_token_for_github_is_none() {
        // Newer gh keeps the token in the OS keyring; hosts.yml has no
        // oauth_token, and another host's token must not be picked up.
        let yaml = "\
github.com:
    user: me
    git_protocol: https
other.example.com:
    oauth_token: ghp_other
";
        assert_eq!(parse_gh_hosts_yaml(yaml), None);
    }

    #[test]
    fn parse_gh_hosts_yaml_skips_comments_and_tab_indentation() {
        let yaml = "# managed by gh\ngithub.com:\n# comment\n\toauth_token: ghp_tabbed\n";
        assert_eq!(parse_gh_hosts_yaml(yaml), Some("ghp_tabbed".into()));
    }

    #[test]
    fn extract_oauth_token_handles_quoted_empty_values() {
        assert_eq!(extract_oauth_token("oauth_token: \"\""), None);
        assert_eq!(extract_oauth_token("oauth_token: ''"), None);
        assert_eq!(
            extract_oauth_token("oauth_token:   ghp_spaced   "),
            Some("ghp_spaced".into())
        );
    }
}
