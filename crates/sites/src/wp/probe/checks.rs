//! Probe step C (plan §6.2), pure: what the identity can write, and how
//! much space there is. Nothing here fails the probe: reads still work on
//! a site whose plugins directory is read-only; findings become
//! [`Capabilities`] and [`ProbeWarning`]s.

use crate::allowlist;
use crate::remote::inspect::Inspect;
use crate::remote::steps::StepOutput;

use super::failure::ProbeWarning;

/// What the site's identity can do there.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// `test -w` on the plugins directory.
    pub plugins_dir_writable: bool,
    /// WordPress's `upgrade` directory, where installs unpack.
    pub upgrade_dir: UpgradeDir,
    /// Free KiB in `remote_tmp_dir` (`df -Pk`), when known.
    pub tmp_free_kib: Option<u64>,
    /// Free KiB in the content directory, when known.
    pub content_free_kib: Option<u64>,
}

/// WordPress's `<content>/upgrade` directory, as the identity sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpgradeDir {
    /// It exists, and the identity can write it.
    Writable,
    /// It exists, and the identity cannot write it.
    NotWritable,
    /// It does not exist; WordPress creates it in the content directory,
    /// which the identity can (`true`) or cannot write.
    Missing {
        /// Whether the identity can write the content directory.
        content_writable: bool,
    },
    /// PHP could not resolve `WP_CONTENT_DIR`: not checked.
    Unknown,
}

/// The paths step C checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckPaths {
    /// The plugins directory.
    pub(super) plugins_dir: String,
    /// `remote_tmp_dir`.
    pub(super) tmp_dir: String,
    /// The resolved content directory and its `upgrade`, when they pass
    /// the remote path rule.
    pub(super) content: Option<(String, String)>,
}

impl CheckPaths {
    /// The paths for a site.
    ///
    /// # Arguments
    ///
    /// * `plugins_dir` - The plugins directory step H measured
    /// * `tmp_dir` - The site's `remote_tmp_dir`
    /// * `content_dir` - `realpath(WP_CONTENT_DIR)` from step W, if any
    pub(super) fn new(plugins_dir: &str, tmp_dir: &str, content_dir: Option<&str>) -> Self {
        let content = content_dir
            .filter(|dir| allowlist::remote_path(dir).is_ok())
            .map(|dir| (dir.to_string(), format!("{dir}/upgrade")))
            .filter(|(_, upgrade)| allowlist::remote_path(upgrade).is_ok());
        Self {
            plugins_dir: plugins_dir.to_string(),
            tmp_dir: tmp_dir.to_string(),
            content,
        }
    }

    /// Step C's commands: the plugins directory and the temporary
    /// directory, then (when the content directory is known) `upgrade`'s
    /// existence and access, the content directory's access and space.
    pub(super) fn checks(&self) -> Vec<Inspect<'_>> {
        let mut checks = vec![
            Inspect::Writable(&self.plugins_dir),
            Inspect::FreeSpace(&self.tmp_dir),
        ];
        if let Some((content, upgrade)) = &self.content {
            checks.extend([
                Inspect::IsDir(upgrade),
                Inspect::Writable(upgrade),
                Inspect::Writable(content),
                Inspect::FreeSpace(content),
            ]);
        }
        checks
    }

    /// Read step C's outputs, in [`CheckPaths::checks`]' order.
    ///
    /// # Arguments
    ///
    /// * `steps` - The outputs
    ///
    /// # Returns
    ///
    /// The capabilities, and the warnings they raise.
    pub(super) fn read(&self, steps: &[StepOutput]) -> (Capabilities, Vec<ProbeWarning>) {
        let capabilities = self.capabilities(steps);
        let warnings = self.warnings(&capabilities);
        (capabilities, warnings)
    }

    /// Capabilities when step C's output could not be read: nothing is
    /// assumed writable, and one warning says why.
    ///
    /// # Arguments
    ///
    /// * `reason` - Why the output was refused
    pub(super) fn unknown(&self, reason: &str) -> (Capabilities, Vec<ProbeWarning>) {
        let capabilities = Capabilities {
            plugins_dir_writable: false,
            upgrade_dir: UpgradeDir::Unknown,
            tmp_free_kib: None,
            content_free_kib: None,
        };
        let warning = ProbeWarning::ChecksUnreadable {
            reason: reason.to_string(),
        };
        (capabilities, vec![warning])
    }

    /// What step C's outputs say the identity can do.
    ///
    /// # Arguments
    ///
    /// * `steps` - The outputs, in [`CheckPaths::checks`]' order
    fn capabilities(&self, steps: &[StepOutput]) -> Capabilities {
        let ok = |index: usize| steps.get(index).is_some_and(StepOutput::success);
        let space = |index: usize| steps.get(index).and_then(free_kib);
        let known = self.content.is_some();
        let upgrade_dir = match (known, ok(2), ok(3)) {
            (false, _, _) => UpgradeDir::Unknown,
            (true, true, true) => UpgradeDir::Writable,
            (true, true, false) => UpgradeDir::NotWritable,
            (true, false, _) => UpgradeDir::Missing {
                content_writable: ok(4),
            },
        };
        Capabilities {
            plugins_dir_writable: ok(0),
            upgrade_dir,
            tmp_free_kib: space(1),
            content_free_kib: if known { space(5) } else { None },
        }
    }

    /// The warnings `capabilities` raise.
    ///
    /// # Arguments
    ///
    /// * `capabilities` - What step C found
    fn warnings(&self, capabilities: &Capabilities) -> Vec<ProbeWarning> {
        let mut warnings = Vec::new();
        if !capabilities.plugins_dir_writable {
            let dir = self.plugins_dir.clone();
            warnings.push(ProbeWarning::PluginsDirNotWritable { dir });
        }
        if capabilities.tmp_free_kib.is_none() {
            let dir = self.tmp_dir.clone();
            warnings.push(ProbeWarning::SpaceUnknown { dir });
        }
        let Some((content, upgrade)) = &self.content else {
            warnings.push(ProbeWarning::ContentDirUnresolved);
            return warnings;
        };
        match capabilities.upgrade_dir {
            UpgradeDir::NotWritable => warnings.push(ProbeWarning::UpgradeDirNotWritable {
                dir: upgrade.clone(),
            }),
            UpgradeDir::Missing {
                content_writable: false,
            } => warnings.push(ProbeWarning::ContentDirNotWritable {
                dir: content.clone(),
            }),
            _ => {}
        }
        if capabilities.content_free_kib.is_none() {
            let dir = content.clone();
            warnings.push(ProbeWarning::SpaceUnknown { dir });
        }
        warnings
    }
}

/// The free KiB `df -Pk` reports: its second line's "Available" column,
/// found as the number before the `NN%` capacity that follows three
/// numbers (a file system or mount point name may hold spaces).
///
/// # Arguments
///
/// * `step` - `df -Pk`'s output
pub(super) fn free_kib(step: &StepOutput) -> Option<u64> {
    if !step.success() {
        return None;
    }
    let text = std::str::from_utf8(&step.stdout).ok()?;
    let line = text.lines().nth(1)?;
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let numeric = |token: &&str| !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit());
    tokens.windows(4).find_map(|window| {
        let [total, used, available, capacity] = window else {
            return None;
        };
        let is_capacity = capacity
            .strip_suffix('%')
            .is_some_and(|digits| numeric(&digits));
        let numbers = [total, used, available].iter().all(|token| numeric(token));
        (is_capacity && numbers)
            .then(|| available.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step that exited `status` with `stdout`.
    ///
    /// # Arguments
    ///
    /// * `status` - Its exit status
    /// * `stdout` - Its output
    fn step(status: i32, stdout: &str) -> StepOutput {
        StepOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            stderr_truncated: false,
        }
    }

    /// A `df -Pk` answer with `available` KiB free.
    ///
    /// # Arguments
    ///
    /// * `available` - The free KiB
    fn df(available: u64) -> StepOutput {
        step(
            0,
            &format!(
                "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                 overlay           90243936 10630268  {available}      13% /\n"
            ),
        )
    }

    /// Paths with a content directory.
    fn paths() -> CheckPaths {
        CheckPaths::new("/srv/p", "/tmp", Some("/srv/c"))
    }

    #[test]
    fn free_space_is_the_available_column() {
        // Captured on the harness (Debian 13 coreutils).
        assert_eq!(free_kib(&df(74_983_604)), Some(74_983_604));
        // A file system and mount point with spaces.
        let spaced = step(
            0,
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
             map auto home 0 0 0 100% /System/Volumes/Data/home dir\n",
        );
        assert_eq!(free_kib(&spaced), Some(0));
        for bad in [
            step(1, ""),
            step(0, "Filesystem\n"),
            step(0, "header\nx y z w\n"),
        ] {
            assert_eq!(free_kib(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn checks_follow_what_is_known() {
        let paths = paths();
        assert_eq!(
            paths.checks(),
            [
                Inspect::Writable("/srv/p"),
                Inspect::FreeSpace("/tmp"),
                Inspect::IsDir("/srv/c/upgrade"),
                Inspect::Writable("/srv/c/upgrade"),
                Inspect::Writable("/srv/c"),
                Inspect::FreeSpace("/srv/c"),
            ]
        );
        // An unresolved or unusable content directory: two checks only.
        for content in [None, Some("relative"), Some("/a\nb")] {
            assert_eq!(CheckPaths::new("/srv/p", "/tmp", content).checks().len(), 2);
        }
    }

    #[test]
    fn a_writable_site_raises_no_warning() {
        let steps = [
            step(0, ""),
            df(10),
            step(0, ""),
            step(0, ""),
            step(0, ""),
            df(20),
        ];
        let (capabilities, warnings) = paths().read(&steps);
        assert_eq!(
            capabilities,
            Capabilities {
                plugins_dir_writable: true,
                upgrade_dir: UpgradeDir::Writable,
                tmp_free_kib: Some(10),
                content_free_kib: Some(20),
            }
        );
        assert_eq!(warnings, []);
    }

    #[test]
    fn each_finding_becomes_a_warning() {
        let steps = [
            step(1, ""),
            step(1, ""),
            step(0, ""),
            step(1, ""),
            step(0, ""),
            step(1, ""),
        ];
        let (capabilities, warnings) = paths().read(&steps);
        assert!(!capabilities.plugins_dir_writable);
        assert_eq!(capabilities.upgrade_dir, UpgradeDir::NotWritable);
        assert_eq!(
            warnings,
            [
                ProbeWarning::PluginsDirNotWritable {
                    dir: "/srv/p".into()
                },
                ProbeWarning::SpaceUnknown { dir: "/tmp".into() },
                ProbeWarning::UpgradeDirNotWritable {
                    dir: "/srv/c/upgrade".into()
                },
                ProbeWarning::SpaceUnknown {
                    dir: "/srv/c".into()
                },
            ]
        );
        // No upgrade directory: the content directory decides.
        let steps = [
            step(0, ""),
            df(1),
            step(1, ""),
            step(1, ""),
            step(1, ""),
            df(1),
        ];
        let (capabilities, warnings) = paths().read(&steps);
        assert_eq!(
            capabilities.upgrade_dir,
            UpgradeDir::Missing {
                content_writable: false
            }
        );
        assert_eq!(
            warnings,
            [ProbeWarning::ContentDirNotWritable {
                dir: "/srv/c".into()
            }]
        );
        let steps = [
            step(0, ""),
            df(1),
            step(1, ""),
            step(1, ""),
            step(0, ""),
            df(1),
        ];
        let (capabilities, warnings) = paths().read(&steps);
        assert_eq!(
            capabilities.upgrade_dir,
            UpgradeDir::Missing {
                content_writable: true
            }
        );
        assert_eq!(warnings, []);
    }

    #[test]
    fn an_unresolved_content_dir_is_reported() {
        let (capabilities, warnings) =
            CheckPaths::new("/srv/p", "/tmp", None).read(&[step(0, ""), df(5)]);
        assert_eq!(capabilities.upgrade_dir, UpgradeDir::Unknown);
        assert_eq!(capabilities.content_free_kib, None);
        assert_eq!(warnings, [ProbeWarning::ContentDirUnresolved]);
    }
}
