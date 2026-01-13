//! BWU project builder.

use super::Builder;

/// Builder for the BWU project.
pub struct BwuBuilder;

impl Builder for BwuBuilder {
    fn build_command(&self, version: &str) -> String {
        format!("npm run build -- --version={}", version)
    }

    fn setup_commands(&self) -> Vec<String> {
        vec!["npm install".to_string()]
    }
}
