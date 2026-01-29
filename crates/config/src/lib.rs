//! APVM Configuration Library
//!
//! Provides configuration types for APVM.
//! This crate is designed to be used by the core library and CLI.
//!
//! # Design Philosophy
//!
//! - **No defaults**: Libraries should not hardcode default paths. Consumers
//!   (like CLI) must provide explicit paths. This enables better testability
//!   and clear ownership of configuration decisions.
//! - **No file I/O**: This crate provides types only. File operations
//!   (load/save) are the consumer's responsibility.
//! - **No path conventions**: Path layout (like `~/.myapp`) is CLI's decision,
//!   not the library's.
//!
//! # Example
//!
//! ```rust
//! use apvm_config::Config;
//! use std::path::PathBuf;
//!
//! // Create config with explicit paths
//! let config = Config::new(
//!     PathBuf::from("/var/cache/myapp"),
//!     PathBuf::from("/var/lib/myapp/builds"),
//! );
//!
//! // Or with a token
//! let config = Config::with_token(
//!     "ghp_xxxxxxxxxxxx",
//!     PathBuf::from("/cache"),
//!     PathBuf::from("/builds"),
//! );
//! ```

mod config;

pub use config::Config;
