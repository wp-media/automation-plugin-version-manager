//! Git operations.

mod repository;
mod resolver;
pub mod token;
mod workspace;

pub use repository::Repository;
pub use resolver::{RefResolver, RefSource, ResolvedRef, is_prerelease_tag};
pub use token::{ResolvedToken, TokenSource, is_valid_token_format, resolve_github_token};
pub use workspace::BuildWorkspace;
