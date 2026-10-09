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
pub(crate) use resolver::expand_commit;
pub use resolver::{RefResolver, RefSource, ResolvedRef, is_prerelease_tag};
pub use token::{
    KNOWN_TOKEN_PREFIXES, ResolvedToken, TokenSource, is_valid_token_format, resolve_github_token,
};
pub use workspace::BuildWorkspace;
