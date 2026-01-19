//! Git operations.

mod cache;
mod repository;
mod resolver;

pub use cache::RepoCache;
pub use repository::Repository;
pub use resolver::{RefResolver, RefSource, ResolvedRef};
