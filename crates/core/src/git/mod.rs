//! Git operations.

mod remote;
mod repository;
mod resolver;
#[cfg(test)]
pub(crate) mod testutil;
pub mod token;
mod workspace;

pub use remote::{RemoteGit, RemoteRefs};
pub use repository::Repository;
pub use resolver::{RefResolver, RefSource, ResolvedRef, is_prerelease_tag};
pub use token::{ResolvedToken, TokenSource, is_valid_token_format, resolve_github_token};
pub use workspace::BuildWorkspace;
