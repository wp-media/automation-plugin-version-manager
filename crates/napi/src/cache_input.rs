//! Strict reading of the cache methods' JS arguments.
//!
//! napi's own object conversion is lenient: it accepts a string or `true`
//! where an object is expected, and ignores unknown keys. For `clean()` —
//! whose default removes *everything* — that turns `clean({ dryrun: true })`
//! or `clean('30d')` into a full wipe, and `ApvmCache.open('/dir')` into the
//! default cache. So these arguments are read field by field:
//!
//! - an argument that is not a plain object (or absent) is refused — an
//!   array, a `Map`, a class instance, `Object.create(defaults)`: options
//!   are read from own properties only, so inherited ones (a class's
//!   getters, a prototype's defaults) would otherwise be silently ignored;
//! - an unknown key is refused (a typo never falls back to a default);
//! - a field of the wrong type is refused;
//! - for `clean()` and `open()`, a key that is present but `undefined` or
//!   `null` is refused too: `{ project: process.env.UNSET }` must not mean
//!   "every project", nor `{ cacheDir: undefined }` the default cache.
//!
//! An exception the argument itself throws while being read (a getter, a
//! `Proxy` trap) propagates synchronously, unchanged.
//!
//! Reading ([`read_object`], [`read_string`], [`read_bool`]) touches JS
//! values; deciding ([`check_object_type`], [`check_plain`], [`check_keys`],
//! [`clean_request`], …) is pure and unit-tested.

use apvm_core::CleanRequest;
use apvm_core::maintenance::{CleanTarget, VerifyMode};
use napi::ValueType;
use napi::bindgen_prelude::{
    JsObjectValue, JsValue, KeyCollectionMode, KeyConversion, KeyFilter, Object, Unknown,
};

/// Keys `clean()` accepts.
pub const CLEAN_KEYS: &[&str] = &["olderThan", "project", "dryRun", "target"];
/// Keys `gc()` and `verify()` accept.
pub const CHECKSUM_KEYS: &[&str] = &["checksum"];
/// Keys of an `ApvmConfig` (`open()` reads only `cacheDir`).
pub const CONFIG_KEYS: &[&str] = &["cacheDir", "cacheEnabled", "githubToken"];

/// One option as read from JS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    /// The key is not there.
    Absent,
    /// The key is there with `undefined` or `null`.
    Empty,
    /// The key holds a value of the expected type.
    Value(T),
}

/// The fields of `clean()`'s options, read but not yet validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanFields {
    pub older_than: Field<String>,
    pub project: Field<String>,
    pub dry_run: Field<bool>,
    pub target: Field<String>,
}

impl Default for CleanFields {
    fn default() -> Self {
        Self {
            older_than: Field::Absent,
            project: Field::Absent,
            dry_run: Field::Absent,
            target: Field::Absent,
        }
    }
}

// =============================================================================
// Reading JS values
// =============================================================================

/// `value` as an options object: `Ok(None)` when absent (`undefined` or
/// `null`), the object when it is a plain object with only `allowed` keys.
///
/// # Errors
///
/// A message naming `what` when `value` is not an object (or is an array),
/// is not a plain object, or has a key outside `allowed`.
pub fn read_object<'env>(
    value: Option<Unknown<'env>>,
    what: &str,
    allowed: &[&str],
) -> Result<Option<Object<'env>>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let kind = value.get_type().map_err(|e| e.to_string())?;
    let is_array = kind == ValueType::Object && value.is_array().map_err(|e| e.to_string())?;
    check_object_type(kind, is_array, what)?;
    if kind != ValueType::Object {
        return Ok(None);
    }
    let object = value.coerce_to_object().map_err(|e| e.to_string())?;
    let (prototype, grandparent) = prototype_kinds(&object).map_err(|e| e.to_string())?;
    check_plain(prototype, grandparent, what)?;
    let keys = own_keys(&object).map_err(|e| e.to_string())?;
    check_keys(&keys, allowed, what)?;
    Ok(Some(object))
}

/// The own enumerable string keys of `object`, the ones options are read
/// from. Never inherited ones: an enumerable property someone added to
/// `Object.prototype` is not an option of every call (and `Object::keys`,
/// which walks the prototype chain, would report it as an unknown one).
fn own_keys(object: &Object<'_>) -> napi::Result<Vec<String>> {
    let names = object.get_all_property_names(
        KeyCollectionMode::OwnOnly,
        KeyFilter::Enumerable,
        KeyConversion::NumbersToStrings,
    )?;
    let mut keys = Vec::new();
    for index in 0..names.get_array_length()? {
        let name: Unknown<'_> = names.get_element(index)?;
        // Symbol keys are never options.
        if name.get_type()? == ValueType::String {
            keys.push(name.coerce_to_string()?.into_utf8()?.into_owned()?);
        }
    }
    Ok(keys)
}

/// The types of `object`'s prototype and of that prototype's own prototype
/// (`Null` when the first is `null`), for [`check_plain`].
fn prototype_kinds(object: &Object<'_>) -> napi::Result<(ValueType, ValueType)> {
    let prototype = object.get_prototype()?;
    let kind = prototype.get_type()?;
    let grandparent = match kind {
        ValueType::Object | ValueType::Function => {
            prototype.coerce_to_object()?.get_prototype()?.get_type()?
        }
        _ => ValueType::Null,
    };
    Ok((kind, grandparent))
}

/// `key` of `object`: absent, empty (`undefined` / `null`), or its value
/// with its JS type.
fn raw_field<'env>(
    object: &Object<'env>,
    key: &str,
) -> Result<Field<(Unknown<'env>, ValueType)>, String> {
    if !object.has_own_property(key).map_err(|e| e.to_string())? {
        return Ok(Field::Absent);
    }
    let value: Unknown<'env> = object.get_named_property(key).map_err(|e| e.to_string())?;
    Ok(match value.get_type().map_err(|e| e.to_string())? {
        ValueType::Undefined | ValueType::Null => Field::Empty,
        kind => Field::Value((value, kind)),
    })
}

/// Read `key` of `object` as a string.
///
/// # Errors
///
/// A message naming `what` and `key` when the value is not a string.
pub fn read_string(object: &Object<'_>, key: &str, what: &str) -> Result<Field<String>, String> {
    Ok(match raw_field(object, key)? {
        Field::Absent => Field::Absent,
        Field::Empty => Field::Empty,
        // Coerced only once known to be a string: a plain conversion.
        Field::Value((value, ValueType::String)) => Field::Value(
            value
                .coerce_to_string()
                .and_then(|s| s.into_utf8())
                .and_then(|s| s.into_owned())
                .map_err(|e| e.to_string())?,
        ),
        Field::Value((_, other)) => return Err(wrong_type(what, key, "a string", other)),
    })
}

/// Read `key` of `object` as a boolean.
///
/// # Errors
///
/// A message naming `what` and `key` when the value is not a boolean.
pub fn read_bool(object: &Object<'_>, key: &str, what: &str) -> Result<Field<bool>, String> {
    Ok(match raw_field(object, key)? {
        Field::Absent => Field::Absent,
        Field::Empty => Field::Empty,
        Field::Value((value, ValueType::Boolean)) => {
            Field::Value(value.coerce_to_bool().map_err(|e| e.to_string())?)
        }
        Field::Value((_, other)) => return Err(wrong_type(what, key, "a boolean", other)),
    })
}

/// Read `clean()`'s options object (absent = no options).
///
/// # Errors
///
/// As for [`read_object`], [`read_string`] and [`read_bool`].
pub fn read_clean_fields(value: Option<Unknown<'_>>) -> Result<CleanFields, String> {
    const WHAT: &str = "clean() options";
    let Some(object) = read_object(value, WHAT, CLEAN_KEYS)? else {
        return Ok(CleanFields::default());
    };
    Ok(CleanFields {
        older_than: read_string(&object, "olderThan", WHAT)?,
        project: read_string(&object, "project", WHAT)?,
        dry_run: read_bool(&object, "dryRun", WHAT)?,
        target: read_string(&object, "target", WHAT)?,
    })
}

/// Read `gc()` / `verify()` options (`what` names the method) into the
/// verification depth: `checksum: true` → [`VerifyMode::Checksum`], else
/// [`VerifyMode::Size`] (`undefined` / `null` mean the default here: the
/// flag only chooses how deep to look).
///
/// # Errors
///
/// As for [`read_object`], [`read_string`] and [`read_bool`].
pub fn read_verify_mode(value: Option<Unknown<'_>>, what: &str) -> Result<VerifyMode, String> {
    let Some(object) = read_object(value, what, CHECKSUM_KEYS)? else {
        return Ok(VerifyMode::Size);
    };
    Ok(verify_mode(&read_bool(&object, "checksum", what)?))
}

/// Read the `cacheDir` of `ApvmCache.open()`'s config.
///
/// # Errors
///
/// As for [`read_object`] and [`read_string`], plus [`cache_dir`]'s.
pub fn read_cache_dir(value: Option<Unknown<'_>>) -> Result<Option<String>, String> {
    const WHAT: &str = "ApvmCache.open() config";
    let Some(object) = read_object(value, WHAT, CONFIG_KEYS)? else {
        return Ok(None);
    };
    cache_dir(read_string(&object, "cacheDir", WHAT)?)
}

// =============================================================================
// Deciding (pure)
// =============================================================================

/// Refuse a non-object argument (absent — `undefined` / `null` — is fine).
///
/// # Errors
///
/// A message naming `what` and the type received.
pub fn check_object_type(kind: ValueType, is_array: bool, what: &str) -> Result<(), String> {
    match kind {
        ValueType::Undefined | ValueType::Null => Ok(()),
        ValueType::Object if !is_array => Ok(()),
        ValueType::Object => Err(format!("{what} must be an object, got an array")),
        other => Err(format!(
            "{what} must be an object, got {}",
            type_name(other)
        )),
    }
}

/// Refuse an object that is not plain. A plain object — an object literal,
/// `JSON.parse` output, `Object.create(null)`, from any realm — has a `null`
/// prototype or one whose own prototype is `null` (`Object.prototype`).
/// Anything else (a class instance, a `Map`, `Object.create(defaults)`)
/// may carry options the own-property reading would silently ignore.
///
/// # Errors
///
/// A message naming `what` and the fix.
pub fn check_plain(prototype: ValueType, grandparent: ValueType, what: &str) -> Result<(), String> {
    if prototype == ValueType::Null || grandparent == ValueType::Null {
        return Ok(());
    }
    Err(format!(
        "{what} must be a plain object, got one with a prototype of its own (a class \
         instance, a Map, Object.create(...)); copy its options into an object literal"
    ))
}

/// Refuse any key outside `allowed`.
///
/// # Errors
///
/// A message naming the first unknown key and the accepted ones.
pub fn check_keys(keys: &[String], allowed: &[&str], what: &str) -> Result<(), String> {
    match keys.iter().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "{what}: unknown option `{key}` (expected {})",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

/// Validate `clean()`'s fields into a [`CleanRequest`]. The duration and
/// project rules are the core's, checked when the request runs.
///
/// # Errors
///
/// A message for a present-but-empty field, or an unknown `target`.
pub fn clean_request(fields: CleanFields) -> Result<CleanRequest, String> {
    const WHAT: &str = "clean() options";
    let older_than = required(fields.older_than, "olderThan", WHAT)?;
    let project = required(fields.project, "project", WHAT)?;
    let dry_run = required(fields.dry_run, "dryRun", WHAT)?;
    let target = match required(fields.target, "target", WHAT)? {
        None => CleanTarget::All,
        Some(name) => clean_target(&name).ok_or_else(|| {
            format!("{WHAT}: `target` must be one of All, Builds, Releases, got '{name}'")
        })?,
    };
    Ok(CleanRequest::default()
        .older_than(older_than)
        .project(project)
        .target(target)
        .dry_run(dry_run.unwrap_or(false)))
}

/// `JsCleanTarget` value → storage target.
pub fn clean_target(name: &str) -> Option<CleanTarget> {
    match name {
        "All" => Some(CleanTarget::All),
        "Builds" => Some(CleanTarget::Builds),
        "Releases" => Some(CleanTarget::Releases),
        _ => None,
    }
}

/// The `checksum` flag → verification depth.
pub fn verify_mode(checksum: &Field<bool>) -> VerifyMode {
    if *checksum == Field::Value(true) {
        VerifyMode::Checksum
    } else {
        VerifyMode::Size
    }
}

/// `open()`'s `cacheDir`: absent → the default; present → must be a string.
///
/// # Errors
///
/// A message when `cacheDir` is present but `undefined` / `null`.
pub fn cache_dir(field: Field<String>) -> Result<Option<String>, String> {
    required(
        field,
        "cacheDir",
        "ApvmCache.open() config (omit it to use APVM_CACHE_DIR or ~/.apvm/cache)",
    )
}

/// A field that may be omitted but, when present, must hold a value.
fn required<T>(field: Field<T>, key: &str, what: &str) -> Result<Option<T>, String> {
    match field {
        Field::Absent => Ok(None),
        Field::Value(value) => Ok(Some(value)),
        Field::Empty => Err(format!(
            "{what}: `{key}` is undefined or null; omit the key instead"
        )),
    }
}

/// The message for a field of the wrong type.
fn wrong_type(what: &str, key: &str, expected: &str, got: ValueType) -> String {
    format!("{what}: `{key}` must be {expected}, got {}", type_name(got))
}

/// The JS `typeof`-style name of `kind`.
fn type_name(kind: ValueType) -> &'static str {
    match kind {
        ValueType::Undefined => "undefined",
        ValueType::Null => "null",
        ValueType::Boolean => "a boolean",
        ValueType::Number => "a number",
        ValueType::String => "a string",
        ValueType::Symbol => "a symbol",
        ValueType::Object => "an object",
        ValueType::Function => "a function",
        ValueType::External => "an external",
        _ => "an unsupported value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned key names, as read from a JS object.
    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn absent_and_plain_objects_are_accepted() {
        for kind in [ValueType::Undefined, ValueType::Null, ValueType::Object] {
            assert_eq!(check_object_type(kind, false, "x"), Ok(()));
        }
    }

    #[test]
    fn non_objects_and_arrays_are_refused() {
        let refused = [
            (
                ValueType::String,
                false,
                "x must be an object, got a string",
            ),
            (
                ValueType::Boolean,
                false,
                "x must be an object, got a boolean",
            ),
            (
                ValueType::Number,
                false,
                "x must be an object, got a number",
            ),
            (
                ValueType::Function,
                false,
                "x must be an object, got a function",
            ),
            (ValueType::Object, true, "x must be an object, got an array"),
        ];
        for (kind, is_array, message) in refused {
            assert_eq!(
                check_object_type(kind, is_array, "x"),
                Err(message.to_string())
            );
        }
    }

    #[test]
    fn only_plain_objects_are_read() {
        use ValueType::{Function, Null, Object};
        // `{}` (prototype `Object.prototype`, whose prototype is null) and
        // `Object.create(null)`.
        assert_eq!(check_plain(Object, Null, "x"), Ok(()));
        assert_eq!(check_plain(Null, Null, "x"), Ok(()));
        // A class instance, a Map, `Object.create(defaults)` (prototype with
        // its own prototype), `Object.create(fn)`.
        for (prototype, grandparent) in [(Object, Object), (Function, Object)] {
            let err = check_plain(prototype, grandparent, "x").unwrap_err();
            assert!(err.starts_with("x must be a plain object"), "{err}");
            assert!(err.contains("object literal"), "{err}");
        }
    }

    #[test]
    fn unknown_keys_are_refused_with_the_accepted_ones() {
        assert_eq!(
            check_keys(&keys(&["dryRun", "project"]), CLEAN_KEYS, "x"),
            Ok(())
        );
        assert_eq!(check_keys(&[], CLEAN_KEYS, "x"), Ok(()));
        assert_eq!(
            check_keys(&keys(&["dryRun", "dryrun"]), CLEAN_KEYS, "x"),
            Err("x: unknown option `dryrun` (expected olderThan, project, dryRun, target)".into())
        );
    }

    #[test]
    fn no_clean_fields_remove_everything_for_real() {
        assert_eq!(
            clean_request(CleanFields::default()),
            Ok(CleanRequest::default())
        );
    }

    #[test]
    fn clean_fields_are_passed_through() {
        let fields = CleanFields {
            older_than: Field::Value("30d".into()),
            project: Field::Value("backwpup".into()),
            dry_run: Field::Value(true),
            target: Field::Value("Releases".into()),
        };
        assert_eq!(
            clean_request(fields),
            Ok(CleanRequest::default()
                .older_than(Some("30d".into()))
                .project(Some("backwpup".into()))
                .target(CleanTarget::Releases)
                .dry_run(true))
        );
    }

    #[test]
    fn a_present_but_empty_clean_field_is_refused() {
        let cases: [(CleanFields, &str); 4] = [
            (
                CleanFields {
                    older_than: Field::Empty,
                    ..CleanFields::default()
                },
                "olderThan",
            ),
            (
                CleanFields {
                    project: Field::Empty,
                    ..CleanFields::default()
                },
                "project",
            ),
            (
                CleanFields {
                    dry_run: Field::Empty,
                    ..CleanFields::default()
                },
                "dryRun",
            ),
            (
                CleanFields {
                    target: Field::Empty,
                    ..CleanFields::default()
                },
                "target",
            ),
        ];
        for (fields, key) in cases {
            assert_eq!(
                clean_request(fields),
                Err(format!(
                    "clean() options: `{key}` is undefined or null; omit the key instead"
                ))
            );
        }
    }

    #[test]
    fn every_target_maps_and_others_are_refused() {
        assert_eq!(clean_target("All"), Some(CleanTarget::All));
        assert_eq!(clean_target("Builds"), Some(CleanTarget::Builds));
        assert_eq!(clean_target("Releases"), Some(CleanTarget::Releases));
        for bad in ["builds", "Nope", ""] {
            assert_eq!(clean_target(bad), None, "{bad}");
        }
        let fields = CleanFields {
            target: Field::Value("Nope".into()),
            ..CleanFields::default()
        };
        assert_eq!(
            clean_request(fields),
            Err(
                "clean() options: `target` must be one of All, Builds, Releases, got 'Nope'".into()
            )
        );
    }

    #[test]
    fn wrong_types_name_the_option_and_what_was_given() {
        assert_eq!(
            wrong_type("clean() options", "dryRun", "a boolean", ValueType::String),
            "clean() options: `dryRun` must be a boolean, got a string"
        );
    }

    #[test]
    fn every_js_type_has_a_readable_name() {
        // These names end up in user-facing "got …" messages.
        let names = [
            (ValueType::Undefined, "undefined"),
            (ValueType::Null, "null"),
            (ValueType::Boolean, "a boolean"),
            (ValueType::Number, "a number"),
            (ValueType::String, "a string"),
            (ValueType::Symbol, "a symbol"),
            (ValueType::Object, "an object"),
            (ValueType::Function, "a function"),
            (ValueType::External, "an external"),
        ];
        for (kind, name) in names {
            assert_eq!(type_name(kind), name);
        }
        assert_eq!(
            check_object_type(ValueType::Symbol, false, "x"),
            Err("x must be an object, got a symbol".to_string())
        );
    }

    #[test]
    fn checksum_selects_the_depth_and_defaults_to_size() {
        assert_eq!(verify_mode(&Field::Value(true)), VerifyMode::Checksum);
        for other in [Field::Value(false), Field::Empty, Field::Absent] {
            assert_eq!(verify_mode(&other), VerifyMode::Size, "{other:?}");
        }
    }

    #[test]
    fn open_cache_dir_must_be_omitted_or_a_string() {
        assert_eq!(cache_dir(Field::Absent), Ok(None));
        assert_eq!(cache_dir(Field::Value("/c".into())), Ok(Some("/c".into())));
        let err = cache_dir(Field::Empty).unwrap_err();
        assert!(err.contains("`cacheDir` is undefined or null"), "{err}");
        assert!(err.contains("omit it to use APVM_CACHE_DIR"), "{err}");
    }
}
