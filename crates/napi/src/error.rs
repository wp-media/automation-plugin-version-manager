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
    napi::Error::new(core_error_status(&err), err.to_string())
}

/// The N-API [`Status`] for an [`apvm_core::Error`] — the classification
/// [`core_error_to_napi`] applies, shared with the cache bindings' error codes.
pub fn core_error_status(err: &apvm_core::Error) -> Status {
    match err {
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
    }
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::*;

    /// Errors the caller fixes by changing an argument: a JS `InvalidArg`.
    fn caller_errors() -> Vec<apvm_core::Error> {
        vec![
            apvm_core::Error::ProjectNotFound("nope".to_string()),
            apvm_core::Error::RepositoryNotFound(PathBuf::from("/repo")),
            apvm_core::Error::PrivateRepoNoToken {
                repo: "wp-media/backwpup-pro".to_string(),
            },
            apvm_core::Error::Config("bad".to_string()),
            apvm_core::Error::ReleaseNotFound {
                tag: "v9".to_string(),
                repo: "wp-media/backwpup-pro".to_string(),
            },
            apvm_core::Error::NoMatchingReleaseAssets {
                tag: "v9".to_string(),
                repo: "wp-media/backwpup-pro".to_string(),
                available: "a.zip".to_string(),
            },
            apvm_core::Error::ReleasesNotAvailable {
                project: "wp-rocket".to_string(),
                tag: "v9".to_string(),
            },
            apvm_core::Error::Storage(apvm_storage::Error::InvalidInput {
                what: "project",
                value: "Bad/Name".to_string(),
                reason: "not allowed".to_string(),
            }),
            apvm_core::Error::Storage(apvm_storage::Error::SourceFileMissing {
                path: PathBuf::from("/missing.zip"),
            }),
        ]
    }

    /// Errors only reportable or retryable: a JS `GenericFailure`.
    /// (`GitHub` and `Json` wrap foreign error types this crate cannot build.)
    fn environment_errors() -> Vec<apvm_core::Error> {
        vec![
            apvm_core::Error::Git("clone failed".to_string()),
            apvm_core::Error::Build("step failed".to_string()),
            apvm_core::Error::Project("x".to_string()),
            apvm_core::Error::PlatformUnsupported {
                project: "imagify".to_string(),
                platform: "windows".to_string(),
                reason: "needs bash".to_string(),
            },
            apvm_core::Error::Io(io::Error::other("disk")),
            apvm_core::Error::Storage(apvm_storage::Error::Data {
                details: "bad row".to_string(),
            }),
            apvm_core::Error::Cache("x".to_string()),
            apvm_core::Error::Update("x".to_string()),
            apvm_core::Error::Uninstall("x".to_string()),
            apvm_core::Error::Skill("x".to_string()),
        ]
    }

    #[test]
    fn caller_fixable_errors_are_invalid_arg() {
        for err in caller_errors() {
            assert_eq!(core_error_status(&err), Status::InvalidArg, "{err}");
        }
    }

    #[test]
    fn environment_errors_are_generic_failures() {
        for err in environment_errors() {
            assert_eq!(core_error_status(&err), Status::GenericFailure, "{err}");
        }
    }

    #[test]
    fn storage_errors_follow_the_direct_storage_rules() {
        // A wrapped storage error is classified as the storage call would be,
        // not blanket-mapped from the `Storage` wrapper.
        let wrapped = |inner| core_error_status(&apvm_core::Error::Storage(inner));
        assert_eq!(
            wrapped(apvm_storage::Error::SourceFileMissing {
                path: PathBuf::from("/a")
            }),
            Status::InvalidArg
        );
        assert_eq!(
            wrapped(apvm_storage::Error::DatabaseInUse {
                path: PathBuf::from("/c/apvm.db")
            }),
            Status::GenericFailure
        );
    }
}
