//! `git` commands isolated from the caller's repository.
//!
//! Git exports variables such as `GIT_DIR` to the processes it starts: hooks,
//! `git rebase --exec`, aliases, and every program they run. A `git` spawned
//! by apvm under such a parent would act on the parent's repository instead of
//! the one apvm passes in its working directory. In a linked worktree the
//! exported `GIT_DIR` is absolute, so a clone or commit meant for a temporary
//! repository lands in the caller's repository.
//!
//! Every `git` apvm runs is therefore built here, with the repository-location
//! variables removed. The list is the location subset of
//! `git rev-parse --local-env-vars`: the variables Git itself drops when it
//! switches to another repository. `GIT_CONFIG_PARAMETERS` and
//! `GIT_CONFIG_COUNT` are kept on purpose: they carry configuration a caller
//! sets deliberately (e.g. CI injecting an `insteadOf` URL rewrite), not a
//! repository location.

/// The variables through which a parent `git` points its children at its own
/// repository; [`command`] removes all of them.
pub const REPOSITORY_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
];

/// Removes [`REPOSITORY_ENV_VARS`] from `cmd`'s environment, whether inherited
/// or set on the command.
///
/// # Arguments
/// * `cmd` - The command to isolate.
///
/// # Returns
/// `cmd`, for chaining.
pub fn isolate(cmd: &mut std::process::Command) -> &mut std::process::Command {
    for var in REPOSITORY_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// A `git` command isolated from the caller's repository (see the module
/// docs). Callers set the repository through `current_dir` or arguments.
///
/// # Returns
/// A `std::process::Command` for `git` with [`REPOSITORY_ENV_VARS`] removed.
pub fn command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    isolate(&mut cmd);
    cmd
}

/// The async (tokio) form of [`command`], used by apvm's own git operations.
///
/// # Returns
/// A `tokio::process::Command` for `git` with [`REPOSITORY_ENV_VARS`] removed.
pub(crate) fn tokio_command() -> tokio::process::Command {
    command().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::Path;
    use tempfile::TempDir;

    /// Initializes an empty repository in a fresh temp dir.
    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        let out = command()
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .output()
            .expect("git must be runnable in tests");
        assert!(
            out.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir
    }

    /// Runs `git rev-parse --absolute-git-dir` in `dir` with `GIT_DIR` set to
    /// `decoy` on the command, isolated or not; returns the reported git dir.
    fn git_dir_seen(dir: &Path, decoy: &Path, isolated: bool) -> String {
        let mut cmd = std::process::Command::new("git");
        cmd.env("GIT_DIR", decoy.join(".git"))
            .env("GIT_INDEX_FILE", decoy.join(".git/index"))
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(dir);
        if isolated {
            isolate(&mut cmd);
        }
        let out = cmd.output().expect("git must be runnable in tests");
        assert!(
            out.status.success(),
            "rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn command_removes_every_repository_variable() {
        let cmd = command();
        let removed: Vec<&OsStr> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key)
            .collect();
        for var in REPOSITORY_ENV_VARS {
            assert!(removed.contains(&OsStr::new(var)), "{var} is not removed");
        }
        assert_eq!(cmd.get_program(), "git");
    }

    #[test]
    fn deliberate_config_injection_is_kept() {
        assert!(!REPOSITORY_ENV_VARS.contains(&"GIT_CONFIG_PARAMETERS"));
        assert!(!REPOSITORY_ENV_VARS.contains(&"GIT_CONFIG_COUNT"));
    }

    #[test]
    fn isolated_git_works_on_its_own_repository_despite_a_foreign_git_dir() {
        let target = init_repo();
        let decoy = init_repo();
        let target_git_dir = std::fs::canonicalize(target.path().join(".git")).expect("canonical");
        let decoy_git_dir = std::fs::canonicalize(decoy.path().join(".git")).expect("canonical");

        // Control: without isolation, the foreign GIT_DIR wins (the bug).
        assert_eq!(
            git_dir_seen(target.path(), decoy.path(), false),
            decoy_git_dir.to_string_lossy()
        );
        // With isolation, git uses the repository of its working directory.
        assert_eq!(
            git_dir_seen(target.path(), decoy.path(), true),
            target_git_dir.to_string_lossy()
        );
    }

    #[tokio::test]
    async fn tokio_command_is_isolated_too() {
        let cmd = tokio_command();
        let removed: Vec<&OsStr> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key)
            .collect();
        for var in REPOSITORY_ENV_VARS {
            assert!(removed.contains(&OsStr::new(var)), "{var} is not removed");
        }
    }
}
