//! APVM Configuration Library
//!
//! Provides path management and configuration types for APVM.
//! This crate is designed to be used by both the CLI and the core library.
//!
//! # Design Philosophy
//!
//! - **No defaults**: Libraries should not hardcode default paths. Consumers
//!   (like CLI) must provide explicit paths. This enables better testability
//!   and clear ownership of configuration decisions.
//! - **No file I/O**: This crate provides types only. File operations
//!   (load/save) are the consumer's responsibility.
//! - **Dependency Injection**: All paths must be explicitly provided at
//!   construction time.
//!
//! # Example
//!
//! ```rust
//! use apvm_config::{Config, Paths};
//! use std::path::PathBuf;
//!
//! // Create paths with explicit directories
//! let paths = Paths::new(
//!     PathBuf::from("/var/lib/myapp"),
//!     PathBuf::from("/var/lib/myapp/builds"),
//! );
//! println!("Base dir: {:?}", paths.apvm_dir());
//!
//! // Or use builder for partial construction
//! let paths = Paths::builder()
//!     .apvm_dir("/custom/path")
//!     .builds_dir("/custom/builds")
//!     .build();
//!
//! // Config from paths
//! let config = Config::with_paths(paths);
//!
//! // Or create config directly
//! let config = Config::new(
//!     PathBuf::from("/cache"),
//!     PathBuf::from("/builds"),
//! );
//! ```

mod paths;
mod config;

pub use paths::{Paths, PathsBuilder};
pub use config::Config;
