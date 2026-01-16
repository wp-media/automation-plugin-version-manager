//! APVM Configuration Library
//!
//! Provides path management and configuration types for APVM.
//! This crate is designed to be used by both the CLI and the core library.
//!
//! # Design Philosophy
//!
//! - **No file I/O**: This crate provides types and default paths only.
//!   File operations (load/save) are the consumer's responsibility.
//! - **Dependency Injection**: All paths can be overridden at construction time.
//! - **Cross-platform**: Uses `directories` crate for platform-appropriate defaults.
//!
//! # Default Paths
//!
//! | Path | Default | Description |
//! |------|---------|-------------|
//! | APVM Directory | `~/.apvm` | Base directory for APVM data |
//! | Config File | `~/.apvm/config.json` | Configuration file |
//! | Cache Directory | `~/.apvm/cache` | Repository cache |
//! | Builds Directory | `~/apvm-builds` | Built artifacts storage |
//!
//! # Example
//!
//! ```rust
//! use apvm_config::{Config, Paths};
//!
//! // Use default paths
//! let paths = Paths::default();
//! println!("APVM dir: {:?}", paths.apvm_dir());
//!
//! // Or create with custom paths
//! let paths = Paths::builder()
//!     .apvm_dir("/custom/apvm")
//!     .builds_dir("/custom/builds")
//!     .build();
//!
//! // Config with defaults
//! let config = Config::default();
//!
//! // Config with custom paths
//! let config = Config::with_paths(paths);
//! ```

mod paths;
mod config;

pub use paths::{Paths, PathsBuilder};
pub use config::Config;
