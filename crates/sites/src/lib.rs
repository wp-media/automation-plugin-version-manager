//! # apvm-sites
//!
//! Manage WordPress plugins on remote sites over SSH: the library behind
//! `apvm server|site|group` and `apvm wp`, the Node bindings, and apvm-core's
//! build-and-install path (design: `docs/plans/site-deployment.md`).
//!
//! This version holds the data layer:
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
//! - [`Error`] with stable [`Code`]s for the CLI's `--json` and the Node
//!   bindings.
//!
//! # Design rules
//!
//! - **No `apvm-*` dependencies**: apvm-core depends on this crate, and zip
//!   installs or `wp run` need no GitHub/build stack.
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

#[cfg(test)]
mod tests {
    /// The lines of a Cargo manifest that would make the crate depend on
    /// another apvm crate: any mention of `apvm-` outside comments and the
    /// package name (a dependency key, a `[dependencies.apvm-…]` table, a
    /// renamed `package = "apvm-…"`), and any `path = "…"` dependency.
    fn apvm_dependency_lines(manifest: &str) -> Vec<&str> {
        manifest
            .lines()
            .map(|line| line.split('#').next().unwrap_or_default().trim())
            .filter(|line| *line != r#"name = "apvm-sites""#)
            .filter(|line| line.contains("apvm-") || line.contains("path = \""))
            .collect()
    }

    #[test]
    fn the_crate_depends_on_no_apvm_crate() {
        // Plan D1: apvm-core depends on apvm-sites, never the reverse, and
        // the remote layer needs no GitHub/build stack.
        let manifest = include_str!("../Cargo.toml");
        assert_eq!(apvm_dependency_lines(manifest), Vec::<&str>::new());
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
