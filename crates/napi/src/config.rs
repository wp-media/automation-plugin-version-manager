//! Configuration bindings for Node.js.
//!
//! Provides a JavaScript-friendly configuration object that maps to the
//! internal [`apvm_config::Config`] type. All paths are represented as
//! strings since JavaScript doesn't have a native `Path` type.
//!
//! # Optional `buildsDir`
//!
//! When `buildsDir` is omitted, a unique temporary directory is created
//! automatically (e.g., `/tmp/apvm-<pid>-<hex>/builds`). This is useful
//! for one-off builds where persistent storage is not needed — the consumer
//! only wants build artifacts without managing a storage directory.

use std::path::PathBuf;

use napi_derive::napi;

/// Configuration options for creating an APVM instance.
///
/// This is a plain JavaScript object (not a class) that you pass
/// to [`Apvm.create()`] or [`Apvm.createWithTokenResolution()`].
///
/// All fields are optional — when `buildsDir` is omitted, a temporary
/// directory is created automatically.
///
/// # TypeScript
///
/// ```typescript
/// // Minimal — temp builds dir, no token
/// const apvm = Apvm.create({});
///
/// // With explicit builds directory
/// const apvm = Apvm.create({ buildsDir: '/var/lib/apvm/builds' });
///
/// // Full configuration
/// const config: ApvmConfig = {
///   buildsDir: '/var/lib/apvm/builds',
///   githubToken: 'ghp_xxxxxxxxxxxx',
/// };
/// ```
#[derive(Default)]
#[napi(object)]
pub struct ApvmConfig {
    /// Directory where built artifacts will be stored.
    ///
    /// When omitted (`null` or `undefined`), a temporary directory is created
    /// automatically under the system temp path (e.g., `/tmp/apvm-<pid>-<hex>/builds`).
    /// This is ideal for one-off builds where persistent storage is not needed.
    ///
    /// When provided, must be an absolute path to an existing (or creatable) directory.
    pub builds_dir: Option<String>,

    /// GitHub Personal Access Token for API requests.
    ///
    /// Required for private repositories (e.g., BackWPup).
    /// Optional for public repositories (e.g., WP Rocket), but recommended
    /// for higher API rate limits (5000 vs 60 requests/hour).
    ///
    /// The token needs the `repo` scope for private repositories.
    pub github_token: Option<String>,
}

/// Generate a unique temporary directory path for builds.
///
/// Creates a path like `/tmp/apvm-<pid>-<hex-timestamp>/builds` that is
/// unique per process invocation. The directory is NOT automatically cleaned
/// up — consumers are responsible for cleanup if needed.
fn generate_temp_builds_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    std::env::temp_dir()
        .join(format!("apvm-{pid}-{nanos:x}"))
        .join("builds")
}

impl From<ApvmConfig> for apvm_config::Config {
    fn from(js_config: ApvmConfig) -> Self {
        let builds_dir = js_config
            .builds_dir
            .map(PathBuf::from)
            .unwrap_or_else(generate_temp_builds_dir);
        let config = apvm_config::Config::new(builds_dir);
        match js_config.github_token {
            Some(token) => config.set_token(token),
            None => config,
        }
    }
}
