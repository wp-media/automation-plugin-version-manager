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
/// - `PlatformUnsupported` → `GenericFailure`
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
        // Not an argument the caller can fix — it's a host-environment limit —
        // so a generic failure, not InvalidArg.
        | apvm_core::Error::PlatformUnsupported { .. }
        | apvm_core::Error::Io(_)
        | apvm_core::Error::Json(_) => Status::GenericFailure,
        apvm_core::Error::ReleaseNotFound { .. }
        | apvm_core::Error::NoMatchingReleaseAssets { .. } => Status::InvalidArg,
        apvm_core::Error::ReleasesNotAvailable { .. } => Status::InvalidArg,
        // A storage error carries an inner `apvm_storage::Error`; classify it
        // with the same rules a direct storage call would use.
        apvm_core::Error::Storage(inner) => storage_error_status(inner),
        apvm_core::Error::Cache(_) => Status::GenericFailure,
        apvm_core::Error::Update(_) => Status::GenericFailure,
        apvm_core::Error::Uninstall(_) => Status::GenericFailure,
        // CLI-only (skill management is not exposed through the bindings),
        // but mapped anyway so the conversion stays total.
        apvm_core::Error::Skill(_) => Status::GenericFailure,
    };
    napi::Error::new(status, err.to_string())
}

/// Classify an [`apvm_storage::Error`] into an N-API [`Status`].
///
/// Validation failures (`InvalidInput`, `SourceFileMissing`) map to
/// `InvalidArg` — the caller can fix them by changing arguments. Everything
/// else (I/O, database, corruption) is a `GenericFailure` the caller can
/// only report or retry.
fn storage_error_status(err: &apvm_storage::Error) -> Status {
    match err {
        apvm_storage::Error::InvalidInput { .. }
        | apvm_storage::Error::SourceFileMissing { .. } => Status::InvalidArg,
        _ => Status::GenericFailure,
    }
}
