//! Error handling for N-API bindings.
//!
//! Converts Rust error types into JavaScript exceptions that are safe to throw
//! in the Node.js runtime. Errors never panic — they are always propagated
//! as `napi::Error` which becomes a JavaScript `Error` on the JS side.
//!
//! Due to the orphan rule, we cannot implement `From<apvm_core::Error> for napi::Error`
//! directly. Instead, we provide conversion functions that are used at call sites
//! via `.map_err()`.

use napi::Status;

/// Convert an [`apvm_core::Error`] into a [`napi::Error`].
///
/// Maps each error variant to an appropriate N-API status code:
///
/// - `ProjectNotFound`, `RepositoryNotFound` → `InvalidArg`
/// - `PrivateRepoNoToken` → `InvalidArg` (with descriptive message)
/// - `Config` → `InvalidArg`
/// - `GitHub` → `GenericFailure`
/// - `Git`, `Build` → `GenericFailure`
/// - `Io`, `Json` → `GenericFailure`
///
/// The full error message (including any chained context) is preserved
/// in the JavaScript `Error.message` property.
pub fn core_error_to_napi(err: apvm_core::Error) -> napi::Error {
    let status = match &err {
        apvm_core::Error::ProjectNotFound(_) => Status::InvalidArg,
        apvm_core::Error::RepositoryNotFound(_) => Status::InvalidArg,
        apvm_core::Error::PrivateRepoNoToken { .. } => Status::InvalidArg,
        apvm_core::Error::Config(_) => Status::InvalidArg,
        apvm_core::Error::GitHub(_)
        | apvm_core::Error::Git(_)
        | apvm_core::Error::Build(_)
        | apvm_core::Error::Project(_)
        | apvm_core::Error::Io(_)
        | apvm_core::Error::Json(_) => Status::GenericFailure,
    };
    napi::Error::new(status, err.to_string())
}

/// Convert an [`apvm_storage::Error`] into a [`napi::Error`].
///
/// Storage errors are always mapped to `GenericFailure` since they
/// represent I/O or data integrity issues that the consumer cannot
/// fix by changing arguments.
#[allow(dead_code)]
pub fn storage_error_to_napi(err: apvm_storage::Error) -> napi::Error {
    napi::Error::new(Status::GenericFailure, err.to_string())
}
