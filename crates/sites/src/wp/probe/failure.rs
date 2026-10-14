//! Why a probe failed, and what it warns about (plan §6.2, §4.5 codes).

use crate::error::Code;
use crate::problem::quoted;
use crate::remote::{Identity, IdentityRefusal, SudoRefusal};

/// Why probing a site failed, with its stable code ([`ProbeFailure::code`]).
/// Values from the server are quoted and escaped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeFailure {
    /// The identity table refused the site (`owner_is_root`,
    /// `plugins_dir_mismatch`, `sudo_missing`, `invalid_input`).
    #[error("{0}")]
    Identity(IdentityRefusal),
    /// sudo refused a command the probe needs (`sudo_requiretty`,
    /// `sudo_password_required`, `sudo_not_allowed`).
    #[error("{0}")]
    Sudo(SudoRefusal),
    /// The login user cannot read the plugins directory, and reading it as
    /// root needs sudo, which the server lacks (`sudo_missing`).
    #[error(
        "the login user cannot read the plugins directory {}, and reading it as root needs \
         sudo, which the server lacks",
        quoted(dir)
    )]
    RootStatNeedsSudo {
        /// The directory.
        dir: String,
    },
    /// The plugins directory does not exist: no WordPress at the site's
    /// path (`not_wordpress`), or a pinned `plugins_dir` that is wrong
    /// (`plugins_dir_mismatch`).
    #[error(
        "the plugins directory {} does not exist{}",
        quoted(dir),
        if *pinned {
            ""
        } else {
            " (if this WordPress keeps its plugins elsewhere, WP_PLUGIN_DIR, pin the site's \
             plugins_dir to that directory)"
        }
    )]
    PluginsDirMissing {
        /// The directory.
        dir: String,
        /// Whether the site pins it (`plugins_dir`).
        pinned: bool,
    },
    /// The plugins directory is not a directory: root's `stat`, which never
    /// follows links, found a symbolic link or another file
    /// (`plugins_dir_mismatch`).
    #[error(
        "the plugins directory {} is a {}, not a directory: pin the site's plugins_dir to the \
         real directory",
        quoted(dir),
        quoted(found)
    )]
    PluginsDirNotDirectory {
        /// The directory.
        dir: String,
        /// What `stat` called it.
        found: String,
    },
    /// The walk to the plugins directory met a symbolic link the site
    /// could have planted or replaced, or landed elsewhere than the path
    /// says (`plugins_dir_mismatch`). The identity is derived only from a
    /// directory reached through links that root or the login user own,
    /// in directories only they may write (a release layout's `current ->
    /// releases/<n>` qualifies).
    #[error(
        "the plugins directory {} is reached through a symbolic link the site could have \
         planted or replaced{}: apvm follows a link only when it and the directory holding it \
         belong to root or the login user and neither group nor others may write that \
         directory (as for a release layout's current -> releases/<n>); fix the link's \
         ownership, or check who owns the real directory and pin the site's plugins_dir \
         (and path) to it",
        quoted(dir),
        detail.as_ref().map(|d| format!(" ('{d}')")).unwrap_or_default()
    )]
    PluginsDirLinked {
        /// The configured directory.
        dir: String,
        /// The walk's message, escaped.
        detail: Option<String>,
    },
    /// The identity's own walk found another owner or group than root's
    /// `stat` read: the directory changed between them
    /// (`plugins_dir_mismatch`).
    #[error(
        "the plugins directory {} changed while it was probed: root's stat read uid {} gid {}, \
         the identity found uid {} gid {}",
        quoted(dir),
        expected.0,
        expected.1,
        found.0,
        found.1
    )]
    PluginsDirChanged {
        /// The configured directory.
        dir: String,
        /// Owner and group root's `stat` read.
        expected: (u32, u32),
        /// Owner and group the identity found.
        found: (u32, u32),
    },
    /// WordPress's plugins directory (`WP_PLUGIN_DIR`, resolved) is not the
    /// one whose owner decided the identity (`plugins_dir_mismatch`). The
    /// identity is never switched to what WordPress reports: wp-config.php
    /// and must-use plugins are under the site's control.
    #[error(
        "WordPress uses the plugins directory {}, not {} whose owner decided the identity ({}): \
         check who owns it, then pin the site's plugins_dir to it",
        found.as_deref().map_or_else(|| "(unresolved)".to_string(), quoted),
        quoted(expected),
        identity_text(identity)
    )]
    PluginsDirMismatch {
        /// The directory step H measured.
        expected: String,
        /// `realpath(WP_PLUGIN_DIR)`, when PHP could resolve it.
        found: Option<String>,
        /// The identity the probe used (unchanged by the mismatch).
        identity: Identity,
    },
    /// The site's `wp_cli` cannot run as the identity, or is not WP-CLI
    /// (`wp_cli_missing`).
    #[error("the site's wp_cli {} {reason}", quoted(&program.join(" ")))]
    WpCliMissing {
        /// The `wp_cli` tokens.
        program: Vec<String>,
        /// What went wrong, with WP-CLI's or the shell's message.
        reason: String,
    },
    /// The site's path holds no WordPress (`not_wordpress`).
    #[error("{} holds no WordPress{}", quoted(path), message_suffix(message))]
    NotWordpress {
        /// The site's path.
        path: String,
        /// WP-CLI's message.
        message: Option<String>,
    },
    /// WordPress is there but not installed (`wp_not_installed`).
    #[error(
        "WordPress at {} is not installed{}",
        quoted(path),
        message_suffix(message)
    )]
    WpNotInstalled {
        /// The site's path.
        path: String,
        /// WP-CLI's message.
        message: Option<String>,
    },
    /// The identity could not be switched to (`runuser` failed)
    /// (`probe_failed`).
    #[error("{} failed{}", identity_text(identity), message_suffix(message))]
    SwitchFailed {
        /// The identity.
        identity: Identity,
        /// Its message.
        message: Option<String>,
    },
    /// A probe step printed something apvm cannot read with certainty
    /// (`probe_failed`).
    #[error("probe step {step} printed output apvm cannot read: {reason}")]
    Unreadable {
        /// The step (`H`, `I`, `W`, `C`) and command.
        step: &'static str,
        /// What is wrong, values escaped.
        reason: String,
    },
}

impl ProbeFailure {
    /// The stable code (plan §4.5).
    pub fn code(&self) -> Code {
        match self {
            Self::Identity(refusal) => refusal.code(),
            Self::Sudo(refusal) => refusal.code(),
            Self::RootStatNeedsSudo { .. } => Code::SudoMissing,
            Self::PluginsDirMissing { pinned: false, .. } => Code::NotWordpress,
            Self::PluginsDirMissing { pinned: true, .. }
            | Self::PluginsDirNotDirectory { .. }
            | Self::PluginsDirLinked { .. }
            | Self::PluginsDirChanged { .. }
            | Self::PluginsDirMismatch { .. } => Code::PluginsDirMismatch,
            Self::WpCliMissing { .. } => Code::WpCliMissing,
            Self::NotWordpress { .. } => Code::NotWordpress,
            Self::WpNotInstalled { .. } => Code::WpNotInstalled,
            Self::SwitchFailed { .. } | Self::Unreadable { .. } => Code::ProbeFailed,
        }
    }
}

/// `: '<message>'`, or nothing.
///
/// # Arguments
///
/// * `message` - An escaped message, if any
fn message_suffix(message: &Option<String>) -> String {
    message
        .as_ref()
        .map(|message| format!(": '{message}'"))
        .unwrap_or_default()
}

/// An identity in messages: `the login user`, or its prefix.
///
/// # Arguments
///
/// * `identity` - The identity
pub(crate) fn identity_text(identity: &Identity) -> String {
    match identity {
        Identity::Login => "the login user".to_string(),
        Identity::Sudo { user, group } => match group {
            Some(group) => format!("sudo -u {} -g {}", user.as_str(), group.as_str()),
            None => format!("sudo -u {}", user.as_str()),
        },
        Identity::Runuser { user, group } => {
            format!("runuser -u {} -g {}", user.as_str(), group.as_str())
        }
    }
}

/// Something the probe found that does not stop reads, but that front
/// ends show and later phases act on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeWarning {
    /// The identity cannot write the plugins directory: installs and
    /// removals are blocked (`plugins_dir_not_writable`).
    PluginsDirNotWritable {
        /// The directory.
        dir: String,
    },
    /// WordPress's `upgrade` directory exists, but the identity cannot
    /// write it: installs will fail.
    UpgradeDirNotWritable {
        /// The directory.
        dir: String,
    },
    /// No `upgrade` directory, and the identity cannot write the content
    /// directory to create it: installs will fail.
    ContentDirNotWritable {
        /// The content directory.
        dir: String,
    },
    /// PHP could not resolve `WP_CONTENT_DIR`: the content checks were
    /// skipped.
    ContentDirUnresolved,
    /// `df` could not tell the free space of a directory.
    SpaceUnknown {
        /// The directory.
        dir: String,
    },
    /// No `sha256sum`, `shasum` or `openssl`: uploads are checked by size
    /// only (plan §6.6).
    NoChecksumTool,
    /// WordPress's `siteurl` is not the site's `url`.
    SiteurlDiffers {
        /// The `siteurl` option.
        siteurl: String,
        /// The site's `url`.
        url: String,
    },
    /// Files the identity creates may not get the reference owner and
    /// group: the post-install check repairs them (plan §6.8).
    CheckedAfterCreation,
    /// sudo refused the preferred identity, and a later one is used.
    IdentityFallback {
        /// The identity sudo refused.
        refused: Identity,
    },
    /// Step C's output could not be read: its capabilities are unknown.
    ChecksUnreadable {
        /// Why, escaped.
        reason: String,
    },
}

impl ProbeWarning {
    /// The stable code, for the warnings that have one.
    pub fn code(&self) -> Option<Code> {
        match self {
            Self::PluginsDirNotWritable { .. } => Some(Code::PluginsDirNotWritable),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProbeWarning {
    /// The warning, for humans.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PluginsDirNotWritable { dir } => write!(
                f,
                "the site's identity cannot write the plugins directory {}",
                quoted(dir)
            ),
            Self::UpgradeDirNotWritable { dir } => write!(
                f,
                "the site's identity cannot write WordPress's upgrade directory {}",
                quoted(dir)
            ),
            Self::ContentDirNotWritable { dir } => write!(
                f,
                "the site's identity cannot write the content directory {}, where WordPress \
                 creates its upgrade directory",
                quoted(dir)
            ),
            Self::ContentDirUnresolved => {
                f.write_str("PHP could not resolve WP_CONTENT_DIR; its checks were skipped")
            }
            Self::SpaceUnknown { dir } => {
                write!(f, "the free space of {} is unknown", quoted(dir))
            }
            Self::NoChecksumTool => f.write_str(
                "the server has no sha256sum, shasum or openssl: uploads are checked by size only",
            ),
            Self::SiteurlDiffers { siteurl, url } => write!(
                f,
                "WordPress's siteurl {} is not the site's url {}",
                quoted(siteurl),
                quoted(url)
            ),
            Self::CheckedAfterCreation => f.write_str(
                "new plugin files may not get the plugins directory's owner and group at \
                 creation; the post-install check repairs them",
            ),
            Self::ChecksUnreadable { reason } => write!(
                f,
                "the write and space checks printed output apvm cannot read ({reason}); \
                 the identity's capabilities are unknown"
            ),
            Self::IdentityFallback { refused } => write!(
                f,
                "sudo refused {}; a fallback identity is used",
                identity_text(refused)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{Account, SudoRefusalKind};

    #[test]
    fn every_failure_has_its_code() {
        let www = Identity::sudo("www-data", Some("www-data")).unwrap();
        let cases = [
            (
                ProbeFailure::Identity(IdentityRefusal::SudoMissing {
                    user: Account::new(33, None),
                }),
                Code::SudoMissing,
            ),
            (
                ProbeFailure::Sudo(SudoRefusal {
                    kind: SudoRefusalKind::Requiretty,
                    login: None,
                    user: "www-data".into(),
                    group: None,
                    command: "ALL",
                }),
                Code::SudoRequiretty,
            ),
            (
                ProbeFailure::RootStatNeedsSudo { dir: "/p".into() },
                Code::SudoMissing,
            ),
            (
                ProbeFailure::PluginsDirMissing {
                    dir: "/p".into(),
                    pinned: false,
                },
                Code::NotWordpress,
            ),
            (
                ProbeFailure::PluginsDirMissing {
                    dir: "/p".into(),
                    pinned: true,
                },
                Code::PluginsDirMismatch,
            ),
            (
                ProbeFailure::PluginsDirNotDirectory {
                    dir: "/p".into(),
                    found: "symbolic link".into(),
                },
                Code::PluginsDirMismatch,
            ),
            (
                ProbeFailure::PluginsDirLinked {
                    dir: "/p".into(),
                    detail: Some("apvm: unprotected link plugins".into()),
                },
                Code::PluginsDirMismatch,
            ),
            (
                ProbeFailure::PluginsDirChanged {
                    dir: "/p".into(),
                    expected: (2010, 33),
                    found: (33, 33),
                },
                Code::PluginsDirMismatch,
            ),
            (
                ProbeFailure::PluginsDirMismatch {
                    expected: "/p".into(),
                    found: None,
                    identity: www.clone(),
                },
                Code::PluginsDirMismatch,
            ),
            (
                ProbeFailure::WpCliMissing {
                    program: vec!["wp".into()],
                    reason: "x".into(),
                },
                Code::WpCliMissing,
            ),
            (
                ProbeFailure::NotWordpress {
                    path: "/p".into(),
                    message: None,
                },
                Code::NotWordpress,
            ),
            (
                ProbeFailure::WpNotInstalled {
                    path: "/p".into(),
                    message: None,
                },
                Code::WpNotInstalled,
            ),
            (
                ProbeFailure::SwitchFailed {
                    identity: www,
                    message: None,
                },
                Code::ProbeFailed,
            ),
            (
                ProbeFailure::Unreadable {
                    step: "H",
                    reason: "x".into(),
                },
                Code::ProbeFailed,
            ),
        ];
        for (failure, code) in cases {
            assert_eq!(failure.code(), code, "{failure}");
        }
    }

    #[test]
    fn messages_quote_server_values() {
        let failure = ProbeFailure::PluginsDirMismatch {
            expected: "/var/www/a/wp-content/plugins".into(),
            found: Some("/srv/it's\u{1b}".into()),
            identity: Identity::sudo("www-data", Some("www-data")).unwrap(),
        };
        assert_eq!(
            failure.to_string(),
            "WordPress uses the plugins directory '/srv/it\\'s\\u{1b}', not \
             '/var/www/a/wp-content/plugins' whose owner decided the identity (sudo -u www-data \
             -g www-data): check who owns it, then pin the site's plugins_dir to it"
        );
        let failure = ProbeFailure::NotWordpress {
            path: "/var/www".into(),
            message: Some("Error: This does not seem to be a WordPress installation.".into()),
        };
        assert_eq!(
            failure.to_string(),
            "'/var/www' holds no WordPress: 'Error: This does not seem to be a WordPress \
             installation.'"
        );
        assert_eq!(identity_text(&Identity::Login), "the login user");
        assert_eq!(
            identity_text(&Identity::runuser("ftpuser", "www-data").unwrap()),
            "runuser -u ftpuser -g www-data"
        );
    }

    #[test]
    fn warnings_have_codes_only_where_the_plan_names_one() {
        let warning = ProbeWarning::PluginsDirNotWritable { dir: "/p".into() };
        assert_eq!(warning.code(), Some(Code::PluginsDirNotWritable));
        assert_eq!(
            warning.to_string(),
            "the site's identity cannot write the plugins directory '/p'"
        );
        assert_eq!(ProbeWarning::NoChecksumTool.code(), None);
        assert!(
            ProbeWarning::IdentityFallback {
                refused: Identity::sudo("deploy", Some("www-data")).unwrap()
            }
            .to_string()
            .contains("sudo -u deploy -g www-data")
        );
    }
}
