//! The plugins directory as step H reads it (plan §6.2): the login user's
//! walk, a BSD `stat` retry, root's `stat` for a directory the login user
//! cannot reach, and the identity's own walk that must confirm root's
//! reading immediately before WordPress loads.

use super::host::{self, DirRead, HostFacts};
use super::{Host, PluginsDir, Prober, fail};
use crate::error::Result;
use crate::remote::identity::sudo;
use crate::remote::inspect::{self, Inspect, StatFlavor, walk_refused};
use crate::remote::steps::StepOutput;
use crate::remote::{AccountName, Identity, RootAccess, RootCommand};
use crate::wp::probe::ProbeFailure;

impl Prober<'_> {
    /// The plugins directory from step H's read, retrying with a BSD
    /// `stat` when `-c` was refused, or as root on `Permission denied`.
    ///
    /// # Arguments
    ///
    /// * `facts` - The login user and tools
    /// * `read` - How step H read it
    pub(super) async fn plugins_dir(&self, facts: &HostFacts, read: DirRead) -> Result<PluginsDir> {
        let read = match read {
            DirRead::BsdStat => self.bsd_read().await?,
            read => read,
        };
        match read {
            DirRead::Found { path, ownership } => Ok(PluginsDir {
                path,
                ownership,
                read_by_root: false,
            }),
            DirRead::Denied => self.root_stat(facts).await,
            other => Err(fail(self.unread(other))),
        }
    }

    /// Walk the plugins directory again with BSD `stat -f`.
    async fn bsd_read(&self) -> Result<DirRead> {
        let dir = &self.site.plugins_dir.value;
        let walk = Inspect::Walk {
            path: dir,
            login_uid: None,
            flavor: StatFlavor::Bsd,
        };
        let steps = inspect::steps(&Identity::Login, &[walk, Inspect::StatBsd(dir)])?;
        let outputs = self.steps(steps, "H").await?;
        let (Some(walk), Some(stat)) = (outputs.first(), outputs.get(1)) else {
            return Err(fail(unreadable("the BSD walk printed nothing")));
        };
        host::dir_read(walk, stat).map_err(fail)
    }

    /// The failure of a plugins directory that could not be read.
    ///
    /// # Arguments
    ///
    /// * `read` - How it read
    fn unread(&self, read: DirRead) -> ProbeFailure {
        let dir = self.site.plugins_dir.value.clone();
        match read {
            DirRead::Missing => ProbeFailure::PluginsDirMissing {
                dir,
                pinned: self.site.plugins_dir_is_pinned(),
            },
            DirRead::NotDirectory { found } => ProbeFailure::PluginsDirNotDirectory { dir, found },
            DirRead::Linked { detail } => ProbeFailure::PluginsDirLinked { dir, detail },
            DirRead::Other { message } => unreadable(&format!(
                "the plugins directory could not be read{}",
                message.map(|m| format!(": '{m}'")).unwrap_or_default()
            )),
            DirRead::BsdStat => unreadable("stat refused both -c and -f"),
            DirRead::Denied | DirRead::Found { .. } => {
                unreadable("root could not read the plugins directory")
            }
        }
    }

    /// Read the plugins directory as root (`sudo -n -u root -- stat`), for
    /// a login user that may not reach it.
    ///
    /// # Arguments
    ///
    /// * `facts` - The login user and tools
    async fn root_stat(&self, facts: &HostFacts) -> Result<PluginsDir> {
        let dir = &self.site.plugins_dir.value;
        if facts.login.user.id == 0 {
            return Err(fail(self.unread(DirRead::Denied)));
        }
        if facts.sudo.is_none() {
            return Err(fail(ProbeFailure::RootStatNeedsSudo { dir: dir.clone() }));
        }
        let command = RootCommand::stat_ownership(dir)?;
        let out = self
            .single(inspect::root_step(RootAccess::Sudo, &command), "H")
            .await?;
        if let Some(signal) = sudo::signal(&out) {
            let root = AccountName::new("root")?;
            let refusal = self
                .sudo_refusal(facts, &root, None, signal, ("stat", "/usr/bin/stat"))
                .await?;
            return Err(fail(ProbeFailure::Sudo(refusal)));
        }
        self.root_stat_dir(&out)
    }

    /// The plugins directory from root's `stat`, which describes a link
    /// instead of following it: anything but a directory is refused.
    ///
    /// # Arguments
    ///
    /// * `out` - Root's `stat` output
    fn root_stat_dir(&self, out: &StepOutput) -> Result<PluginsDir> {
        if !out.success() {
            let read = match host::failed_read(out) {
                DirRead::BsdStat => DirRead::Other {
                    message: crate::remote::call::last_line(&out.stderr),
                },
                read => read,
            };
            return Err(fail(self.unread(read)));
        }
        let lines = host::stat_lines(out).map_err(fail)?;
        if !lines.is_directory() {
            let found = crate::problem::printable(lines.kind());
            return Err(fail(self.unread(DirRead::NotDirectory { found })));
        }
        Ok(PluginsDir {
            path: self.site.plugins_dir.value.clone(),
            ownership: lines.ownership(),
            read_by_root: true,
        })
    }

    /// For a directory only root's `stat` read (it follows a linked
    /// `wp-content` or site path the site's code may have planted, then
    /// restored): the identity walks the path itself, in one process, and
    /// must find no link the site could plant and the owner and group root
    /// read. Run immediately before WordPress loads as the identity.
    ///
    /// # Arguments
    ///
    /// * `host` - Step H's results
    /// * `identity` - The chosen identity
    ///
    /// # Returns
    ///
    /// The directory's physical path.
    pub(super) async fn confirm_root_read(
        &self,
        host: &Host,
        identity: &Identity,
    ) -> Result<String> {
        let dir = &self.site.plugins_dir.value;
        let walk = Inspect::Walk {
            path: dir,
            login_uid: Some(host.facts.login.user.id),
            flavor: StatFlavor::Gnu,
        };
        let out = self.single(inspect::steps(identity, &[walk])?, "H").await?;
        if !out.success() {
            return Err(self.confirm_failure(host, identity, &out).await);
        }
        let DirRead::Found { path, ownership } = host::walked(&out).map_err(fail)? else {
            return Err(fail(unreadable("the identity's walk printed no directory")));
        };
        let expected = &host.plugins_dir.ownership;
        if (ownership.owner.id, ownership.group.id) != (expected.owner.id, expected.group.id) {
            return Err(fail(ProbeFailure::PluginsDirChanged {
                dir: dir.clone(),
                expected: (expected.owner.id, expected.group.id),
                found: (ownership.owner.id, ownership.group.id),
            }));
        }
        Ok(path)
    }

    /// Why the identity's confirming walk failed.
    ///
    /// # Arguments
    ///
    /// * `host` - Step H's results
    /// * `identity` - The identity
    /// * `out` - The walk's output
    async fn confirm_failure(
        &self,
        host: &Host,
        identity: &Identity,
        out: &StepOutput,
    ) -> crate::error::Error {
        if let Some(signal) =
            sudo::signal(out).filter(|_| matches!(identity, Identity::Sudo { .. }))
        {
            return match self.identity_refusal(&host.facts, identity, signal).await {
                Ok(refusal) => fail(ProbeFailure::Sudo(refusal)),
                Err(error) => error,
            };
        }
        if walk_refused(out.status) {
            return fail(ProbeFailure::PluginsDirLinked {
                dir: self.site.plugins_dir.value.clone(),
                detail: crate::remote::call::last_line(&out.stderr),
            });
        }
        fail(unreadable(&format!(
            "the identity could not walk the plugins directory (exit {}){}",
            out.status,
            crate::remote::call::last_line(&out.stderr)
                .map(|m| format!(": '{m}'"))
                .unwrap_or_default()
        )))
    }
}

/// An unreadable step H.
///
/// # Arguments
///
/// * `reason` - What is wrong
fn unreadable(reason: &str) -> ProbeFailure {
    ProbeFailure::Unreadable {
        step: "H",
        reason: reason.to_string(),
    }
}
