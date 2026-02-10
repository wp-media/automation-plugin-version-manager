//! Info command implementation.
//!
//! Displays detailed information about a specific plugin.

use clap::Args;

use apvm_core::Apvm;
use apvm_core::build::plugins::VersionRequirement;

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
            }
            let defaults = builder.default_variants();
            if !defaults.is_empty() {
                println!("  Default:     {}", defaults.join(", "));
            }
        }

        // Required commands
        let required = builder.required_commands();
        if !required.is_empty() {
            println!();
            println!("Required tools: {}", required.join(", "));
        }

        Ok(())
    }
}
