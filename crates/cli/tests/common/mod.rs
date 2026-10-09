//! Hermetic sandbox for running the real `apvm` binary in integration tests.
//!
//! Every spawned process gets a **cleared environment** plus a small
//! allow-list, so nothing from the developer's machine leaks in:
//!
//! - `HOME`/`USERPROFILE` → a temp dir (redirects `~/.apvm` and `~/.claude`;
//!   verified empirically — the `directories` crate honors `HOME` on Unix);
//! - the working directory → a temp dir (project-local skill installs);
//! - `PATH=/usr/bin:/bin` (no Homebrew `gh` handing out a real token);
//! - `APVM_NO_UPDATE_CHECK=1` and `NO_COLOR=1`; no `GITHUB_TOKEN`/`GH_TOKEN`.
//!
//! Only `LLVM_PROFILE_FILE` and `TMPDIR` pass through: the first lets
//! coverage tools collect the child's profile, the second keeps the child's
//! own temp files in the system temp location.
//!
//! The harness is Unix-only by design: on Windows the home directory comes
//! from the Known Folder API and ignores `USERPROFILE`, so these tests could
//! touch the real profile there.
#![cfg(unix)]
// Each integration-test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// Path of the `apvm` binary Cargo built for this test run.
pub const APVM: &str = env!("CARGO_BIN_EXE_apvm");

/// Isolated home, working directory and cache location for one test.
pub struct Sandbox {
    /// Fake `$HOME` (holds `.apvm/` and `.claude/`).
    home: TempDir,
    /// Working directory for every spawned command.
    cwd: TempDir,
    /// Parent of the (initially missing) `APVM_CACHE_DIR`.
    scratch: TempDir,
    /// When `true`, `APVM_CACHE_DIR` is not exported (exercises the config
    /// file / default cache location instead).
    without_cache_env: bool,
    /// Allow-listed variables to drop again (see [`Sandbox::without_env`]).
    removed_env: Vec<&'static str>,
}

impl Sandbox {
    /// Create a sandbox whose `APVM_CACHE_DIR` points at a missing directory
    /// under a private temp dir.
    pub fn new() -> Self {
        Self {
            home: TempDir::new().expect("temp home"),
            cwd: TempDir::new().expect("temp cwd"),
            scratch: TempDir::new().expect("temp scratch"),
            without_cache_env: false,
            removed_env: Vec::new(),
        }
    }

    /// Create a sandbox that does **not** export `APVM_CACHE_DIR`, so the
    /// cache location comes from the config file or the `~/.apvm/cache`
    /// default (both inside the fake home).
    pub fn without_cache_env() -> Self {
        Self {
            without_cache_env: true,
            ..Self::new()
        }
    }

    /// Do not export `key` (one of the allow-listed defaults, e.g.
    /// `NO_COLOR`) to commands from this sandbox.
    pub fn without_env(mut self, key: &'static str) -> Self {
        self.removed_env.push(key);
        self
    }

    /// The fake home directory.
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// The working directory every command runs in.
    pub fn cwd(&self) -> &Path {
        self.cwd.path()
    }

    /// The value exported as `APVM_CACHE_DIR` (missing until something
    /// creates it).
    pub fn cache_env_dir(&self) -> PathBuf {
        self.scratch.path().join("cache")
    }

    /// A private scratch directory for test fixtures.
    pub fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    /// `~/.apvm/config.json` inside the fake home.
    pub fn config_file(&self) -> PathBuf {
        self.home().join(".apvm").join("config.json")
    }

    /// `~/.claude/skills/apvm-cli` inside the fake home.
    pub fn global_skill_dir(&self) -> PathBuf {
        self.home().join(".claude").join("skills").join("apvm-cli")
    }

    /// `./.claude/skills/apvm-cli` inside the working directory.
    pub fn local_skill_dir(&self) -> PathBuf {
        self.cwd().join(".claude").join("skills").join("apvm-cli")
    }

    /// Build a hermetic command for `binary` (see the module docs).
    pub fn command_for(&self, binary: &Path) -> Command {
        let mut cmd = Command::new(binary);
        cmd.env_clear()
            .current_dir(self.cwd())
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("PATH", "/usr/bin:/bin")
            .env("APVM_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1");
        for passthrough in ["LLVM_PROFILE_FILE", "TMPDIR"] {
            if let Some(value) = std::env::var_os(passthrough) {
                cmd.env(passthrough, value);
            }
        }
        if !self.without_cache_env {
            cmd.env("APVM_CACHE_DIR", self.cache_env_dir());
        }
        for key in &self.removed_env {
            cmd.env_remove(key);
        }
        cmd
    }

    /// Run the test's `apvm` binary with `args` and empty stdin.
    pub fn run(&self, args: &[&str]) -> Output {
        self.run_binary(Path::new(APVM), args, "")
    }

    /// Run `binary` with `args`, feeding `stdin` and then closing it.
    pub fn run_binary(&self, binary: &Path, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command_for(binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("apvm must be spawnable");
        // Dropping the handle closes stdin, so a prompt sees EOF after `stdin`.
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("apvm must finish")
    }

    /// Copy the test's `apvm` binary to `<home>/.apvm/bin/apvm` and return
    /// that path — the installer's layout. Self-destructive commands
    /// (`uninstall`) run against this throwaway copy, never the build output.
    pub fn installed_copy(&self) -> PathBuf {
        let bin_dir = self.home().join(".apvm").join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        let copy = bin_dir.join("apvm");
        std::fs::copy(APVM, &copy).expect("copy apvm binary");
        copy
    }
}

/// Captured stdout as UTF-8 (lossy).
pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Captured stderr as UTF-8 (lossy).
pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Assert the process exited with `code`, printing both streams otherwise.
#[track_caller]
pub fn assert_exit(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "unexpected exit status\n--- stdout ---\n{}\n--- stderr ---\n{}",
        stdout(out),
        stderr(out)
    );
}

/// The repository copy of a skill file, e.g. `SKILL.md` — the source the
/// binary embeds at compile time.
pub fn repo_skill_file(rel_path: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.claude/skills/apvm-cli")
        .join(rel_path);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
