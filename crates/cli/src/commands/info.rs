//! Info command implementation.
//!
//! Displays detailed information about a specific plugin, including what its
//! builds install on a WordPress site: the plugin slug of each variant, the
//! marker that tells same-slug variants apart, and the family traits of a
//! project with several slugs.

use clap::Args;

use apvm_core::Apvm;
use apvm_core::build::plugins::{Builder, FamilyTraits, VersionRequirement};

/// Indent of the lines under a variant: the column of the variant's name.
const VARIANT_DETAIL_INDENT: &str = "               ";

/// Arguments for the info command.
#[derive(Args, Debug)]
pub struct InfoArgs {
    /// Plugin name (e.g., "backwpup")
    pub plugin: String,
}

impl InfoArgs {
    /// Execute the info command.
    pub fn execute(&self, apvm: &Apvm) -> apvm_core::Result<()> {
        let project = apvm.registry.get(&self.plugin)?;
        let builder = project.builder.as_ref();

        // General info
        println!("Plugin: {}", project.name);
        println!("  Repository:  {}", project.repo_url);
        println!("  GitHub:      {}/{}", project.owner, project.repo);
        println!("  Branch:      {}", project.default_branch);
        if project.is_private {
            println!("  Private:     yes (requires GITHUB_TOKEN)");
        } else {
            println!("  Private:     no");
        }
        if let Some(line) = slugs_line(&project.plugin_slugs()) {
            println!("{line}");
        }

        // Version info
        println!();
        println!("Version:");
        match builder.version_requirement() {
            VersionRequirement::Required => {
                println!("  Requirement: Required (must be provided via --ver)");
            }
            VersionRequirement::Embedded => {
                println!("  Requirement: Embedded (auto-detected from source, --ver ignored)");
            }
            VersionRequirement::Optional => {
                println!("  Requirement: Optional (auto-detected or override with --ver)");
            }
        }
        if let Some(default) = builder.default_version() {
            println!("  Default:     {}", default);
        }

        // Variants info
        println!();
        let variants = builder.variants();
        if variants.is_empty() {
            println!("Variants:      none (single output)");
        } else {
            println!("Variants:");
            for v in &variants {
                println!("  {:<12} {:<14} {}", v.id, v.name, v.description);
                for line in variant_identity_lines(builder, v.id) {
                    println!("{line}");
                }
            }
            let defaults = builder.default_variants();
            if !defaults.is_empty() {
                println!("  Default:     {}", defaults.join(", "));
            }
        }

        // Plugin family
        if let Some(traits) = builder.family_traits() {
            println!();
            for line in family_lines(traits) {
                println!("{line}");
            }
        }

        // Tool dependencies
        let deps = builder.tool_dependencies();
        if !deps.is_empty() {
            println!();
            println!("Tool dependencies:");
            for dep in &deps {
                let status = if dep.required { "required" } else { "optional" };
                if dep.install_commands.is_empty() {
                    println!("  {:<12} ({})", dep.name, status);
                } else {
                    println!(
                        "  {:<12} ({}, auto-install: {})",
                        dep.name,
                        status,
                        dep.install_commands.join(" && ")
                    );
                }
            }
        }

        Ok(())
    }
}

/// The plugin block's line naming the WordPress plugin slug(s) the
/// project's builds install to.
///
/// # Arguments
///
/// * `slugs` - The declared slugs, in variant order, each once
///
/// # Returns
///
/// `Slug:` for one slug, `Slugs:` for several, or `None` when the builder
/// declares none (the line is then left out).
fn slugs_line(slugs: &[&str]) -> Option<String> {
    match slugs {
        [] => None,
        [slug] => Some(format!("  Slug:        {slug}")),
        _ => Some(format!("  Slugs:       {}", slugs.join(", "))),
    }
}

/// The lines printed under a variant: the slug it installs to and the
/// marker that identifies it, each when the builder declares one (built-in
/// builders declare markers only for variants sharing a slug).
///
/// The needle is quoted with Rust's debug escaping, so a quote, a backslash
/// or a control character in it cannot blur the line.
///
/// # Arguments
///
/// * `builder` - The project's builder
/// * `variant` - The variant id
///
/// # Returns
///
/// Zero to two lines, aligned with the variant's name column.
fn variant_identity_lines(builder: &dyn Builder, variant: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(slug) = builder.plugin_slug(Some(variant)) {
        lines.push(format!("{VARIANT_DETAIL_INDENT}Slug:    {slug}"));
    }
    if let Some(marker) = builder.variant_marker(variant) {
        lines.push(format!(
            "{VARIANT_DETAIL_INDENT}Marker:  {} contains {:?}",
            marker.path, marker.needle
        ));
    }
    lines
}

/// The `Family:` section: how the project's sibling plugins behave when they
/// are on one site together.
///
/// # Arguments
///
/// * `traits` - The family traits the builder declares
///
/// # Returns
///
/// The section's lines, header included.
fn family_lines(traits: FamilyTraits) -> Vec<String> {
    let conflicts = if traits.conflicts_when_active {
        "yes (when several are active, only one runs per request)"
    } else {
        "no"
    };
    let shared_data = if traits.shared_data {
        "yes (each one's uninstall removes data the others use)"
    } else {
        "no"
    };
    vec![
        "Family:".to_string(),
        format!("  Conflicts:   {conflicts}"),
        format!("  Shared data: {shared_data}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use apvm_core::build::plugins::{
        BackWPupBuilder, BuildArtifact, ImagifyBuilder, VariantMarker, WpRocketBuilder,
    };
    use apvm_core::{BuildContext, BuildStep};

    #[test]
    fn slugs_line_names_one_or_several_slugs() {
        assert_eq!(slugs_line(&[]), None);
        assert_eq!(
            slugs_line(&["wp-rocket"]).as_deref(),
            Some("  Slug:        wp-rocket")
        );
        assert_eq!(
            slugs_line(&["backwpup", "backwpup-pro"]).as_deref(),
            Some("  Slugs:       backwpup, backwpup-pro")
        );
    }

    #[test]
    fn backwpup_variants_show_their_slug_and_marker() {
        let lines = |variant| variant_identity_lines(&BackWPupBuilder, variant);
        assert_eq!(lines("free"), ["               Slug:    backwpup"]);
        assert_eq!(
            lines("pro-de"),
            [
                "               Slug:    backwpup-pro",
                "               Marker:  inc/Pro/License/License.php contains \
                 \"https://backwpup.de/\"",
            ]
        );
        assert_eq!(
            lines("pro-en"),
            [
                "               Slug:    backwpup-pro",
                "               Marker:  inc/Pro/License/License.php contains \
                 \"https://backwpup.com/\"",
            ]
        );
        assert!(lines("unknown").is_empty());
    }

    /// A builder whose only marker needle holds a quote and a newline.
    struct OddNeedle;

    impl Builder for OddNeedle {
        fn setup_commands(&self) -> Vec<BuildStep> {
            Vec::new()
        }

        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            Vec::new()
        }

        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> apvm_core::Result<Vec<BuildArtifact>> {
            Ok(Vec::new())
        }

        fn variant_marker(&self, _variant: &str) -> Option<VariantMarker> {
            Some(VariantMarker {
                path: "a.php".to_string(),
                needle: "say \"hi\"\nnow".to_string(),
            })
        }
    }

    #[test]
    fn marker_needles_are_escaped_and_missing_slugs_omitted() {
        assert_eq!(
            variant_identity_lines(&OddNeedle, "x"),
            ["               Marker:  a.php contains \"say \\\"hi\\\"\\nnow\""]
        );
    }

    #[test]
    fn single_output_builders_declare_one_slug() {
        for (builder, slug) in [
            (&WpRocketBuilder as &dyn Builder, "wp-rocket"),
            (&ImagifyBuilder, "imagify"),
        ] {
            assert!(builder.variants().is_empty());
            assert_eq!(builder.plugin_slug(None), Some(slug));
            assert_eq!(builder.family_traits(), None);
        }
    }

    #[test]
    fn family_lines_describe_each_trait() {
        let both = FamilyTraits {
            conflicts_when_active: true,
            shared_data: true,
        };
        assert_eq!(
            family_lines(both),
            [
                "Family:",
                "  Conflicts:   yes (when several are active, only one runs per request)",
                "  Shared data: yes (each one's uninstall removes data the others use)",
            ]
        );
        let neither = FamilyTraits {
            conflicts_when_active: false,
            shared_data: false,
        };
        assert_eq!(
            family_lines(neither),
            ["Family:", "  Conflicts:   no", "  Shared data: no"]
        );
    }
}
