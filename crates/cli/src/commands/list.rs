//! List command implementation.
//!
//! Displays all registered plugins available for building.

use apvm_core::Apvm;

/// Execute the list command.
///
/// Prints all registered plugins with their repository and visibility.
pub fn execute(apvm: &Apvm) {
    let mut projects: Vec<_> = apvm.registry.list().collect();
    projects.sort_by_key(|p| &p.name);

    if projects.is_empty() {
        println!("No plugins registered.");
        return;
    }

    println!("Available plugins:\n");
    for project in &projects {
        let visibility = if project.is_private { "(private)" } else { "(public)" };
        println!(
            "  {:<14} {}/{} {}",
            project.name, project.owner, project.repo, visibility
        );
    }
}
