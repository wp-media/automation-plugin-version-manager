//! # apvm-sites
//!
//! Manage WordPress plugins on remote sites over SSH: the library behind
//! `apvm server|site|group` and `apvm wp`, the Node bindings, and apvm-core's
//! build-and-install path (design: `docs/plans/site-deployment.md`).
//!
//! This version holds the data layer, the remote script builder, the
//! SSH transport and the probe:
//!
//! - [`Inventory`]: servers, the WordPress sites on them, groups of sites,
//!   and default SSH settings, read from and written to the
//!   `inventory.json` document. It is always valid: a document is fully
//!   checked when parsed, and a refused change leaves it untouched. Every
//!   problem is reported with its location (`sites[1].server: unknown
//!   server 'eu-2'`).
//! - [`inventory::io`]: loads, saves and updates the inventory file safely
//!   (an exclusive lock, atomic `0600` writes, symbolic links refused), and
//!   reads the `APVM_INVENTORY` override for front ends.
//! - [`Targets`] and [`ResolvedSite`]: which sites a command works on (a
//!   union of sites, groups and servers, in inventory order, never empty),
//!   with each site's effective settings and where each came from.
//! - [`PluginHints`]: what apvm's builders know about their plugins (the
//!   slug each variant installs to, the markers that tell same-slug
//!   variants apart, family traits), with no plugin-specific code here.
//! - [`layout`]: names of the files front ends keep for this crate.
//! - [`PluginPackage`]: a plugin zip inspected locally before an install
//!   (single plugin folder, safe paths, size limits, headers read as
//!   WordPress reads them, variant markers, SHA-256).
//! - [`remote`]: the scripts apvm runs on a server: POSIX quoting, framed
//!   scripts for stdin (`sh -s`) and upload (`sh -c`) delivery, identity
//!   prefixes (`sudo -n -u U -g G`, `runuser`), the closed set of root
//!   commands, and the hermetic WP-CLI command; the transport contract and
//!   the SSH transport that runs them (every option in a private generated
//!   ssh_config, so jump hosts get them too), and the classification of
//!   ssh's own failures into stable codes.
//! - [`wp`]: the typed WP-CLI commands apvm runs on a site (no caller can
//!   add a flag), strict parsers for their output (PHP notices
//!   skipped and reported; HTML pages, truncated or ambiguous output
//!   refused), and the probe ([`wp::probe`]): who WP-CLI runs as on a site
//!   (decided by the plugins directory's owner and group ids), whether it
//!   is really WP-CLI, and what WordPress and the identity can do.
//! - [`Session`]: a probed site, whose commands run as its identity and
//!   whose plugin lists are bound to it.
//! - [`Error`] with stable [`Code`]s for the CLI's `--json` and the Node
//!   bindings.
//!
//! # Design rules
//!
//! - **No `apvm-*` dependencies but `apvm-process`**: apvm-core depends on
//!   this crate, and zip installs or `wp run` need no GitHub/build stack.
//!   `apvm-process` (child processes that never outlive their work) is a
//!   leaf crate both share, so a Node.js teardown kills `ssh` too.
//! - **No default locations**: callers pass every path; [`layout`] only
//!   shares names.
//! - **Allowlists live here**: every value that may reach the `ssh` argv, a
//!   shell command ssh builds, or a remote command is checked by the library,
//!   so all front ends, Node callers included, get the same protection.
//!   Values in messages are quoted with `'` and `\` escaped, and control and
//!   invisible formatting characters as `\u{…}`.
//!
//! # Example
//!
//! ```
//! use apvm_sites::{Code, Group, Inventory, Server, Site};
//!
//! # fn main() -> apvm_sites::Result<()> {
//! let mut inventory = Inventory::new();
//! inventory.add_server(Server::new("eu-1", "203.0.113.10"))?;
//! inventory.add_site(Site::new("shop", "eu-1", "/var/www/shop/public"))?;
//! inventory.add_group(Group::new("qa", vec!["shop".into()]))?;
//!
//! // A refused change names every problem and changes nothing.
//! let result = inventory.add_site(Site::new("blog", "eu-2", "var/www/blog"));
//! let Err(error) = result else {
//!     panic!("an unknown server and a relative path are refused");
//! };
//! assert_eq!(error.code(), Code::InvalidInput);
//! assert_eq!(error.problems().len(), 2);
//! assert_eq!(inventory.sites().len(), 1);
//!
//! // What is saved loads back unchanged.
//! let saved = inventory.to_json()?;
//! assert_eq!(Inventory::from_json(&saved)?, inventory);
//! # Ok(())
//! # }
//! ```

mod allowlist;
mod error;
mod hints;
pub mod inventory;
pub mod layout;
pub mod package;
mod problem;
pub mod remote;
pub mod session;
pub mod wp;

pub use error::{Code, Error, Result};
pub use hints::{FamilyTraits, PluginHint, PluginHints, ProjectHints, VariantHint, VariantMarker};
pub use inventory::{
    Defaults, EntityKind, Group, INVENTORY_VERSION, Inventory, Owner, OwnershipPolicy, RunAs,
    Server, ServerRemoval, Site, SiteRemoval, SshSettings,
};
pub use inventory::{
    ResolvedServer, ResolvedSite, ResolvedSsh, ResolvedTargets, Setting, SettingSource,
    TargetWarning, Targets,
};
pub use package::{PackageDefect, PackageWarning, PluginHeaders, PluginPackage, VariantDetection};
pub use problem::Problem;
pub use session::{CommandRun, Session, SitePlugins, SiteTarget, TargetOp};

#[cfg(test)]
mod tests {
    /// The only apvm dependency lines allowed, exactly as written and only
    /// in their section: the workspace's apvm-process, a leaf crate with no
    /// apvm dependencies of its own, which apvm-core shares; its
    /// test-support feature only as a dev-dependency (never in release
    /// builds).
    const ALLOWED: [(&str, &str); 2] = [
        ("[dependencies]", r#"apvm-process = { workspace = true }"#),
        (
            "[dev-dependencies]",
            r#"apvm-process = { workspace = true, features = ["test-support"] }"#,
        ),
    ];

    /// The lines of a Cargo manifest that would make the crate depend on
    /// another apvm crate: any mention of `apvm-` outside comments, the
    /// package name and [`ALLOWED`] in its section (a dependency key, a
    /// `[dependencies.apvm-…]` table, a renamed `package = "apvm-…"`), and
    /// any `path = "…"` dependency.
    fn apvm_dependency_lines(manifest: &str) -> Vec<&str> {
        let mut section = "";
        let mut found = Vec::new();
        for line in manifest.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.starts_with('[') {
                section = line;
            }
            let allowed = ALLOWED.contains(&(section, line)) || line == r#"name = "apvm-sites""#;
            if !allowed && (line.contains("apvm-") || line.contains("path = \"")) {
                found.push(line);
            }
        }
        found
    }

    #[test]
    fn the_crate_depends_on_no_apvm_crate() {
        // Plan D1: apvm-core depends on apvm-sites, never the reverse, and
        // the remote layer needs no GitHub/build stack. apvm-process (child
        // processes) is the one shared leaf crate.
        let manifest = include_str!("../Cargo.toml");
        assert_eq!(apvm_dependency_lines(manifest), Vec::<&str>::new());
    }

    #[test]
    fn only_the_shared_process_crate_is_allowed_exactly_as_written() {
        for manifest in [
            "[dependencies]\napvm-processx = { workspace = true }",
            "[dependencies]\napvm-process = { path = \"../process\" }",
            "[dependencies]\napvm-process = { workspace = true, features = [\"x\"] }",
            "[dependencies.apvm-process]\nworkspace = true",
        ] {
            assert!(!apvm_dependency_lines(manifest).is_empty(), "{manifest}");
        }
        for (section, line) in ALLOWED {
            let manifest = format!("{section}\n{line}");
            assert_eq!(
                apvm_dependency_lines(&manifest),
                Vec::<&str>::new(),
                "{manifest}"
            );
        }
    }

    #[test]
    fn the_shared_process_crate_is_refused_in_any_other_section() {
        let (plain, support) = (ALLOWED[0].1, ALLOWED[1].1);
        for manifest in [
            // The test-support feature in release builds.
            format!("[dependencies]\n{support}"),
            format!("[target.'cfg(unix)'.dependencies]\n{support}"),
            // The plain line anywhere but [dependencies].
            format!("[dev-dependencies]\n{plain}"),
            format!("[build-dependencies]\n{plain}"),
            format!("[target.'cfg(unix)'.dependencies]\n{plain}"),
            // Before any section, or back in another one.
            plain.to_string(),
            format!("[dependencies]\n[features]\n{plain}"),
        ] {
            assert!(!apvm_dependency_lines(&manifest).is_empty(), "{manifest}");
        }
    }

    #[test]
    fn the_dependency_guard_catches_every_spelling() {
        for manifest in [
            "[dependencies]\napvm-config = { workspace = true }",
            "[dependencies.apvm-config]\nworkspace = true",
            "[dependencies]\nconfig = { package = \"apvm-config\", workspace = true }",
            "[target.'cfg(unix)'.dependencies]\napvm-core.workspace = true",
            "[dependencies]\nsibling = { path = \"../core\" }",
        ] {
            assert!(!apvm_dependency_lines(manifest).is_empty(), "{manifest}");
        }
        let clean = "[package]\nname = \"apvm-sites\"\n# No `apvm-*` dependencies\n\
                     [dependencies]\nserde = { workspace = true }";
        assert_eq!(apvm_dependency_lines(clean), Vec::<&str>::new());
    }
}
