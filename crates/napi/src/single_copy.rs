//! Refuse a second copy of this addon in the same process.
//!
//! Each copy of the `.node` file (duplicate installs, bundlers that copy it)
//! links its own SQLite. SQLite's POSIX file locks are per process, so two
//! SQLite libraries in one process cannot see each other's locks: working
//! on the same cache through both corrupts it or crashes the process (SIGBUS
//! — verified). The default cache (`~/.apvm/cache`) makes sharing likely, so
//! the entry points that lead to the cache refuse a second copy outright.
//!
//! Each copy is identified by the address of a static in its own image; the
//! first copy to reach an entry point records it on `globalThis`. A copy
//! loaded only inside a worker thread (another `globalThis`) is not seen,
//! and where `globalThis` cannot be extended (frozen or sealed, as hardened
//! JavaScript environments do) nothing can be recorded, so the guard is
//! skipped rather than making the addon unusable.

use napi::bindgen_prelude::{JsObjectValue, JsValue, Unknown};
use napi::{Env, Property, PropertyAttributes, Status, ValueType};

/// Lives once per loaded copy of the addon; its address names the copy.
static COPY_ID: u8 = 0;

/// The `globalThis` key the first copy records itself under.
const GLOBAL_KEY: &str = "__apvmNapiAddonCopy";

/// This copy's identity.
fn copy_id() -> String {
    format!("{:p}", &COPY_ID)
}

/// Ensure no other copy of the addon is in use in this JS realm: record this
/// copy on first use, then accept only it. Recording is best-effort: a
/// `globalThis` that cannot be extended skips the guard (see the module
/// docs).
///
/// # Errors
///
/// `GenericFailure` naming the fix when another copy got there first; the
/// error of a failed `globalThis` lookup.
pub fn ensure_single_copy(env: &Env) -> napi::Result<()> {
    let mut global = env.get_global()?;
    let mine = copy_id();
    if global.has_own_property(GLOBAL_KEY)? {
        let recorded: Unknown<'_> = global.get_named_property(GLOBAL_KEY)?;
        let recorded = match recorded.get_type()? {
            ValueType::String => recorded.coerce_to_string()?.into_utf8()?.into_owned()?,
            _ => String::new(),
        };
        return check_same_copy(&recorded, &mine)
            .map_err(|message| napi::Error::new(Status::GenericFailure, message));
    }
    let value = env.create_string(&mine)?;
    // Read-only, hidden, permanent: nothing can clear the record. On a
    // frozen or sealed `globalThis` the definition fails (`InvalidArg`, no
    // JS exception left pending): proceed unguarded.
    let _recorded = global.define_properties(&[Property::new()
        .with_utf8_name(GLOBAL_KEY)?
        .with_value(&value)
        .with_property_attributes(PropertyAttributes::Default)]);
    Ok(())
}

/// Pure decision of [`ensure_single_copy`] (plain `String` errors: a
/// `napi::Error` cannot exist outside Node, so unit tests could not link).
///
/// # Errors
///
/// The message to throw when `recorded` is not `mine`.
fn check_same_copy(recorded: &str, mine: &str) -> Result<(), String> {
    if recorded == mine {
        return Ok(());
    }
    Err(
        "a second copy of apvm-napi is loaded in this process (another .node file): two \
         copies link two SQLite libraries, which corrupt each other's cache locks. Load a \
         single copy — deduplicate the apvm-napi dependency or stop bundling it twice."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_copy_is_accepted_and_another_refused() {
        let mine = copy_id();
        assert!(check_same_copy(&mine, &mine).is_ok());
        let err = check_same_copy("0xdead", &mine).unwrap_err();
        assert!(err.contains("second copy of apvm-napi"), "{err}");
        // A value that is not ours (not a string) is another copy too.
        assert!(check_same_copy("", &mine).is_err());
    }

    #[test]
    fn a_copy_has_one_stable_identity() {
        assert_eq!(copy_id(), copy_id());
    }
}
