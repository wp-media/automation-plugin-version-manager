//! BackWPup project builder.

use super::{Builder, OptionalCommand};

/// Builder for the BackWPup project.
pub struct BackWPupBuilder;

impl Builder for BackWPupBuilder {
    fn required_commands(&self) -> Vec<&'static str> {
        vec!["npm", "composer"]
    }

    fn optional_commands(&self) -> Vec<OptionalCommand> {
        vec![OptionalCommand {
            name: "gulp",
            install_cmd: "npm install --global gulp-cli",
        }]
    }

    fn build_subdirectory(&self) -> Option<&'static str> {
        None
    }

    fn setup_commands(&self) -> Vec<String> {
        vec![
            "composer install".to_string(),
            "npm install".to_string(),
        ]
    }

    fn build_commands(&self, version: &str) -> Vec<String> {
        vec![
            // Build assets
            "gulp buildAssets".to_string(),
            "npx tailwindcss -i ./src/input.css -o ./assets/css/backwpup-admin.css".to_string(),
            // Build free version
            format!("gulp free --packageVersion=\"{}\" --compressPath=.", version),
            // Build pro versions (German and English)
            format!(
                "gulp pro --packageVersion=\"{}\" --compressPath=. --language=de",
                version
            ),
            format!(
                "gulp pro --packageVersion=\"{}\" --compressPath=. --language=en",
                version
            ),
        ]
    }
}
