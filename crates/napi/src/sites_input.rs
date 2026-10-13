//! Strict reading of `SiteInventory`'s JS arguments into `apvm-sites`
//! records.
//!
//! napi's own object conversion is lenient: it ignores unknown keys and
//! truncates numbers (`70000` read as a `u16` port wraps, `22.5` becomes
//! `22`). For inventory records a typo must never fall back to a default
//! (`owenrship: 'check'` would leave the site on `mirror`, which *repairs*
//! files), so records are read field by field, as the cache options are
//! ([`crate::cache_input`]):
//!
//! - a record must be a plain object (an object literal, `JSON.parse`
//!   output, a spread copy of what the inventory returned); fields are read
//!   from own properties only;
//! - an unknown key is refused, as `inventory.json` refuses one, and so is
//!   a non-enumerable own key (`Object.defineProperty` without
//!   `enumerable: true`): a field is either read and validated or refused,
//!   never silently skipped (symbol keys are ignored);
//! - a list field must be a plain array: an own property other than its
//!   items and `length` (`Object.assign(['wp'], { extra: 1 })`) is
//!   refused;
//! - a field of the wrong type is refused; integers must be whole numbers
//!   within the field's type (the library then checks the allowed range);
//! - an optional field set to `undefined` or `null` is unset, as `null` is
//!   in `inventory.json`; a required one is refused;
//! - strings are converted to UTF-8 as Node-API does: a lone UTF-16
//!   surrogate (`'\uD800'`, which UTF-8 cannot hold) becomes U+FFFD (`�`).
//!   Fields with an ASCII allowlist (ids, hosts, users, SSH paths, …) then
//!   refuse it; names, notes and remote paths keep U+FFFD in its place, and
//!   so does an inventory path argument (it names the file spelled with
//!   U+FFFD).
//!
//! Every message names the call and the field (`upsertServer():
//! \`ssh.port\` must be …`); the bindings throw them as `InvalidArg`. The
//! values' own rules (allowlists, ranges, references) are the library's:
//! it checks them when the record is inserted. An exception the argument
//! itself throws while being read (a getter, a `Proxy` trap) propagates
//! unchanged.
//!
//! Reading touches JS values; deciding ([`check_record_type`],
//! [`check_fields`], [`check_plain_record`], [`whole_number`]) is pure and
//! unit-tested.

use std::str::FromStr;

use apvm_sites::{Defaults, Group, Owner, OwnershipPolicy, RunAs, Server, Site, SshSettings};
use napi::ValueType;
use napi::bindgen_prelude::{FromNapiValue, Function, JsObjectValue, JsValue, Object, Unknown};

use napi::bindgen_prelude::KeyFilter;

use crate::cache_input::{
    Field, is_plain, own_string_keys, prototype_kinds, raw_field, type_name, wrong_type,
};

/// Keys of an SSH settings record (`JsSshSettings`).
pub const SSH_KEYS: &[&str] = &[
    "user",
    "port",
    "identityFile",
    "connectTimeoutSecs",
    "knownHostsFile",
    "sshConfigFile",
    "proxyJump",
    "multiplex",
    "binary",
];
/// Keys of the defaults record (`JsDefaults`).
pub const DEFAULTS_KEYS: &[&str] = &["ssh"];
/// Keys of a server record (`JsServer`).
pub const SERVER_KEYS: &[&str] = &["id", "name", "host", "ssh", "notes"];
/// Keys of a site record (`JsSite`).
pub const SITE_KEYS: &[&str] = &[
    "id",
    "name",
    "server",
    "path",
    "url",
    "wpCli",
    "runAs",
    "owner",
    "ownership",
    "pluginsDir",
    "allowRoot",
    "remoteTmpDir",
    "commandTimeoutSecs",
    "notes",
];
/// Keys of a group record (`JsGroup`).
pub const GROUP_KEYS: &[&str] = &["id", "sites", "notes"];
/// Keys of `save()`'s options (`SaveOptions`).
pub const SAVE_KEYS: &[&str] = &["force"];

/// The largest TCP port.
const MAX_PORT: u32 = u16::MAX as u32;

// =============================================================================
// Records
// =============================================================================

/// The fields of one JS record, read with messages that name the call and
/// the field's path (`ssh.port`).
struct Fields<'env, 'a> {
    /// The record.
    object: Object<'env>,
    /// The call, for messages (`upsertServer()`).
    what: &'a str,
    /// Path of the record inside the argument (`""`, or `"ssh."`).
    prefix: &'a str,
}

impl<'env, 'a> Fields<'env, 'a> {
    /// Check that `value` is a plain object with only `allowed` keys.
    ///
    /// # Arguments
    ///
    /// * `value` - The JS value
    /// * `what` - The call, for messages
    /// * `prefix` - Path of the record inside the argument (`""` at the top)
    /// * `allowed` - The keys the record may have
    ///
    /// # Errors
    ///
    /// A message when `value` is not an object, is an array or not plain, or
    /// has an unknown or a non-enumerable key.
    fn read(
        value: Unknown<'env>,
        what: &'a str,
        prefix: &'a str,
        allowed: &[&str],
    ) -> Result<Self, String> {
        let object = plain_record(value, &record_label(what, prefix))?;
        let all = own_string_keys(&object, KeyFilter::AllProperties).map_err(|e| e.to_string())?;
        check_fields(&all, allowed, what, prefix)?;
        let enumerable =
            own_string_keys(&object, KeyFilter::Enumerable).map_err(|e| e.to_string())?;
        check_enumerable(&all, &enumerable, what, prefix)?;
        Ok(Self {
            object,
            what,
            prefix,
        })
    }

    /// The field's path for messages (`ssh.port`).
    fn label(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    /// `key` as a JS value, `None` when it is absent, `undefined` or `null`.
    ///
    /// # Errors
    ///
    /// The message of a failed property read.
    fn value(&self, key: &str) -> Result<Option<(Unknown<'env>, ValueType)>, String> {
        Ok(match raw_field(&self.object, key)? {
            Field::Absent | Field::Empty => None,
            Field::Value(value) => Some(value),
        })
    }

    /// An optional string field.
    ///
    /// # Errors
    ///
    /// A message when the value is not a string.
    fn string(&self, key: &str) -> Result<Option<String>, String> {
        self.value(key)?
            .map(|(value, kind)| string_value(value, kind, self.what, &self.label(key)))
            .transpose()
    }

    /// A required string field.
    ///
    /// # Errors
    ///
    /// A message when it is missing, `undefined` / `null`, or not a string.
    fn required_string(&self, key: &str) -> Result<String, String> {
        self.string(key)?
            .ok_or_else(|| format!("{}: `{}` is required", self.what, self.label(key)))
    }

    /// An optional boolean field.
    ///
    /// # Errors
    ///
    /// A message when the value is not a boolean.
    fn bool(&self, key: &str) -> Result<Option<bool>, String> {
        let Some((value, kind)) = self.value(key)? else {
            return Ok(None);
        };
        if kind != ValueType::Boolean {
            return Err(wrong_type(self.what, &self.label(key), "a boolean", kind));
        }
        value.coerce_to_bool().map(Some).map_err(|e| e.to_string())
    }

    /// An optional integer field, at most `max`.
    ///
    /// # Errors
    ///
    /// A message when the value is not a number, or not a whole number from
    /// 0 to `max`.
    fn integer(&self, key: &str, max: u32) -> Result<Option<u32>, String> {
        let Some((value, kind)) = self.value(key)? else {
            return Ok(None);
        };
        if kind != ValueType::Number {
            return Err(wrong_type(self.what, &self.label(key), "a number", kind));
        }
        let number = value
            .coerce_to_number()
            .and_then(|number| number.get_double())
            .map_err(|e| e.to_string())?;
        whole_number(number, max)
            .map(Some)
            .ok_or_else(|| not_whole(self.what, &self.label(key), number, max))
    }

    /// An optional list-of-strings field.
    ///
    /// # Errors
    ///
    /// A message when the value is not an array, or an item is not a string.
    fn string_list(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        let Some((value, kind)) = self.value(key)? else {
            return Ok(None);
        };
        let is_array = kind == ValueType::Object && value.is_array().map_err(|e| e.to_string())?;
        if !is_array {
            let label = self.label(key);
            return Err(wrong_type(self.what, &label, "an array of strings", kind));
        }
        let array = value.coerce_to_object().map_err(|e| e.to_string())?;
        let length = array.get_array_length().map_err(|e| e.to_string())?;
        let keys = own_string_keys(&array, KeyFilter::AllProperties).map_err(|e| e.to_string())?;
        check_array_keys(&keys, length, self.what, &self.label(key))?;
        (0..length)
            .map(|index| {
                let item: Unknown<'_> = array.get_element(index).map_err(|e| e.to_string())?;
                let kind = item.get_type().map_err(|e| e.to_string())?;
                string_value(
                    item,
                    kind,
                    self.what,
                    &self.label(&format!("{key}[{index}]")),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// An SSH settings record nested at `key` (absent = none set).
    ///
    /// # Errors
    ///
    /// As for [`read_ssh`].
    fn ssh(&self, key: &str) -> Result<SshSettings, String> {
        let Some((value, _)) = self.value(key)? else {
            return Ok(SshSettings::default());
        };
        read_ssh(value, self.what, &format!("{}.", self.label(key)))
    }
}

/// `value` as a plain, non-array object (a record).
///
/// # Arguments
///
/// * `value` - The JS value
/// * `record` - What the record is called, for messages
///
/// # Errors
///
/// A message when `value` is not an object, is an array or is not plain.
fn plain_record<'env>(value: Unknown<'env>, record: &str) -> Result<Object<'env>, String> {
    let kind = value.get_type().map_err(|e| e.to_string())?;
    let is_array = kind == ValueType::Object && value.is_array().map_err(|e| e.to_string())?;
    check_record_type(kind, is_array, record)?;
    let object = value.coerce_to_object().map_err(|e| e.to_string())?;
    let (prototype, grandparent) = prototype_kinds(&object).map_err(|e| e.to_string())?;
    check_plain_record(prototype, grandparent, record)?;
    Ok(object)
}

/// A present value that must be a string (a lone surrogate becomes U+FFFD,
/// see the module docs).
///
/// # Arguments
///
/// * `value`, `kind` - The JS value and its type
/// * `what` - The call, for messages
/// * `label` - The field or argument, for messages (`ssh.user`, `path`)
///
/// # Errors
///
/// A message naming `what` and `label` when it is not a string.
fn string_value(
    value: Unknown<'_>,
    kind: ValueType,
    what: &str,
    label: &str,
) -> Result<String, String> {
    if kind != ValueType::String {
        return Err(wrong_type(what, label, "a string", kind));
    }
    // Coerced only once known to be a string: a plain conversion.
    value
        .coerce_to_string()
        .and_then(|s| s.into_utf8())
        .and_then(|s| s.into_owned())
        .map_err(|e| e.to_string())
}

/// Read SSH settings (`JsSshSettings`) nested in a record.
///
/// # Arguments
///
/// * `value` - The JS value
/// * `what` - The call, for messages
/// * `prefix` - Path of the record (`ssh.`)
///
/// # Errors
///
/// A message for a non-record, an unknown key or a wrong type.
fn read_ssh(value: Unknown<'_>, what: &str, prefix: &str) -> Result<SshSettings, String> {
    let fields = Fields::read(value, what, prefix, SSH_KEYS)?;
    let port = fields
        .integer("port", MAX_PORT)?
        .map(|port| {
            u16::try_from(port)
                .map_err(|_| not_whole(what, &fields.label("port"), f64::from(port), MAX_PORT))
        })
        .transpose()?;
    Ok(SshSettings {
        user: fields.string("user")?,
        port,
        identity_file: fields.string("identityFile")?,
        connect_timeout_secs: fields.integer("connectTimeoutSecs", u32::MAX)?,
        known_hosts_file: fields.string("knownHostsFile")?,
        ssh_config_file: fields.string("sshConfigFile")?,
        proxy_jump: fields.string("proxyJump")?,
        multiplex: fields.bool("multiplex")?,
        binary: fields.string("binary")?,
    })
}

/// Read the defaults record (`JsDefaults`) of `setDefaults()`.
///
/// # Errors
///
/// A message for a missing or non-record argument, an unknown key or a
/// wrong type.
pub fn read_defaults(value: Unknown<'_>) -> Result<Defaults, String> {
    const WHAT: &str = "setDefaults()";
    let fields = Fields::read(value, WHAT, "", DEFAULTS_KEYS)?;
    Ok(Defaults {
        ssh: fields.ssh("ssh")?,
    })
}

/// Read a server record (`JsServer`) of `upsertServer()`.
///
/// # Errors
///
/// As for [`read_defaults`], plus a missing `id` or `host`.
pub fn read_server(value: Unknown<'_>) -> Result<Server, String> {
    const WHAT: &str = "upsertServer()";
    let fields = Fields::read(value, WHAT, "", SERVER_KEYS)?;
    Ok(Server {
        id: fields.required_string("id")?,
        name: fields.string("name")?,
        host: fields.required_string("host")?,
        ssh: fields.ssh("ssh")?,
        notes: fields.string("notes")?,
    })
}

/// Read a site record (`JsSite`) of `upsertSite()`.
///
/// # Errors
///
/// As for [`read_defaults`], plus a missing `id`, `server` or `path`, and
/// an unknown `ownership`.
pub fn read_site(value: Unknown<'_>) -> Result<Site, String> {
    const WHAT: &str = "upsertSite()";
    let fields = Fields::read(value, WHAT, "", SITE_KEYS)?;
    let ownership = fields
        .string("ownership")?
        .map(|policy| {
            OwnershipPolicy::from_str(&policy).map_err(|err| format!("{WHAT}: `ownership`: {err}"))
        })
        .transpose()?;
    Ok(Site {
        id: fields.required_string("id")?,
        name: fields.string("name")?,
        server: fields.required_string("server")?,
        path: fields.required_string("path")?,
        url: fields.string("url")?,
        wp_cli: fields.string_list("wpCli")?,
        run_as: fields.string("runAs")?.map(RunAs::from),
        owner: fields.string("owner")?.map(Owner::from),
        ownership,
        plugins_dir: fields.string("pluginsDir")?,
        allow_root: fields.bool("allowRoot")?,
        remote_tmp_dir: fields.string("remoteTmpDir")?,
        command_timeout_secs: fields.integer("commandTimeoutSecs", u32::MAX)?,
        notes: fields.string("notes")?,
    })
}

/// Read a group record (`JsGroup`) of `upsertGroup()`.
///
/// # Errors
///
/// As for [`read_defaults`], plus a missing `id` or `sites`.
pub fn read_group(value: Unknown<'_>) -> Result<Group, String> {
    const WHAT: &str = "upsertGroup()";
    let fields = Fields::read(value, WHAT, "", GROUP_KEYS)?;
    Ok(Group {
        id: fields.required_string("id")?,
        sites: fields
            .string_list("sites")?
            .ok_or_else(|| format!("{WHAT}: `sites` is required"))?,
        notes: fields.string("notes")?,
    })
}

// =============================================================================
// Arguments
// =============================================================================

/// Read `save()`'s options: whether to force the save (absent, `undefined`
/// or `null`, at any level, means `false`, the safe choice).
///
/// # Errors
///
/// A message for a non-record argument, an unknown key or a wrong type.
pub fn read_save_force(value: Option<Unknown<'_>>) -> Result<bool, String> {
    const WHAT: &str = "save() options";
    let Some(value) = present(value)? else {
        return Ok(false);
    };
    let fields = Fields::read(value, WHAT, "", SAVE_KEYS)?;
    Ok(fields.bool("force")?.unwrap_or(false))
}

/// Read an optional inventory path argument: absent, `undefined` or `null`
/// mean "the default".
///
/// # Arguments
///
/// * `value` - The argument
/// * `what` - The call, for messages (`load()`)
///
/// # Errors
///
/// A message when it is present and not a string.
pub fn read_path(value: Option<Unknown<'_>>, what: &str) -> Result<Option<String>, String> {
    let Some(value) = present(value)? else {
        return Ok(None);
    };
    string_argument(value, what, "path").map(Some)
}

/// Read a required id argument (`removeSite(id)`).
///
/// # Errors
///
/// A message when it is not a string.
pub fn read_id(value: Unknown<'_>, what: &str) -> Result<String, String> {
    string_argument(value, what, "id")
}

/// Read an optional boolean argument (`removeServer(id, cascade?)`):
/// absent, `undefined` or `null` mean `false`.
///
/// # Errors
///
/// A message when it is present and not a boolean.
pub fn read_flag(value: Option<Unknown<'_>>, what: &str, name: &str) -> Result<bool, String> {
    let Some(value) = present(value)? else {
        return Ok(false);
    };
    let kind = value.get_type().map_err(|e| e.to_string())?;
    if kind != ValueType::Boolean {
        return Err(wrong_type(what, name, "a boolean", kind));
    }
    value.coerce_to_bool().map_err(|e| e.to_string())
}

/// Read `update()`'s callback.
///
/// # Errors
///
/// A message when it is not a function.
pub fn read_callback<'env>(
    value: Unknown<'env>,
) -> Result<Function<'env, Object<'env>, Unknown<'env>>, String> {
    const WHAT: &str = "SiteInventory.update()";
    let kind = value.get_type().map_err(|e| e.to_string())?;
    if kind != ValueType::Function {
        return Err(wrong_type(WHAT, "fn", "a function", kind));
    }
    Function::from_unknown(value).map_err(|e| e.to_string())
}

/// `value` unless it is `undefined` or `null`.
///
/// # Errors
///
/// The message of a failed type check.
fn present(value: Option<Unknown<'_>>) -> Result<Option<Unknown<'_>>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let kind = value.get_type().map_err(|e| e.to_string())?;
    Ok((!matches!(kind, ValueType::Undefined | ValueType::Null)).then_some(value))
}

/// A string argument.
///
/// # Errors
///
/// A message naming `what` and `name` when it is not a string.
fn string_argument(value: Unknown<'_>, what: &str, name: &str) -> Result<String, String> {
    let kind = value.get_type().map_err(|e| e.to_string())?;
    string_value(value, kind, what, name)
}

// =============================================================================
// Deciding (pure)
// =============================================================================

/// What a record is called in messages: the call, or the call and the
/// record's path (`upsertServer() \`ssh\``).
fn record_label(what: &str, prefix: &str) -> String {
    match prefix.strip_suffix('.') {
        Some(path) if !path.is_empty() => format!("{what} `{path}`"),
        _ => what.to_string(),
    }
}

/// Refuse a record that is not an object: absent ones included, since a
/// record argument is required.
///
/// # Errors
///
/// A message naming `record` and the type received.
pub fn check_record_type(kind: ValueType, is_array: bool, record: &str) -> Result<(), String> {
    match kind {
        ValueType::Object if !is_array => Ok(()),
        ValueType::Object => Err(format!("{record} must be an object, got an array")),
        other => Err(format!(
            "{record} must be an object, got {}",
            type_name(other)
        )),
    }
}

/// Refuse a record that is not a plain object (see
/// [`crate::cache_input::check_plain`]): fields are read from own
/// properties only, so inherited ones would be silently ignored.
///
/// # Errors
///
/// A message naming `record` and the fix.
pub fn check_plain_record(
    prototype: ValueType,
    grandparent: ValueType,
    record: &str,
) -> Result<(), String> {
    if is_plain(prototype, grandparent) {
        return Ok(());
    }
    Err(format!(
        "{record} must be a plain object, got one with a prototype of its own (a class \
         instance, a Map, Object.create(...)); copy its fields into an object literal"
    ))
}

/// Refuse any key outside `allowed`.
///
/// # Errors
///
/// A message naming the first unknown key (with the record's path) and the
/// accepted ones.
pub fn check_fields(
    keys: &[String],
    allowed: &[&str],
    what: &str,
    prefix: &str,
) -> Result<(), String> {
    match keys.iter().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "{what}: unknown field `{prefix}{key}` (expected {})",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

/// Refuse an own key that is not enumerable: `keys` are every own key,
/// `enumerable` the enumerable ones. Such a field would be read by some
/// tools and skipped by others (`Object.keys`, a spread, `JSON.stringify`),
/// so it is neither read nor ignored.
///
/// # Errors
///
/// A message naming the first such key (with the record's path) and the
/// fix.
pub fn check_enumerable(
    keys: &[String],
    enumerable: &[String],
    what: &str,
    prefix: &str,
) -> Result<(), String> {
    match keys.iter().find(|key| !enumerable.contains(key)) {
        Some(key) => Err(format!(
            "{what}: field `{prefix}{key}` is not enumerable; define it as a plain \
             property (e.g. in an object literal)"
        )),
        None => Ok(()),
    }
}

/// Refuse an array with an own property other than its items and
/// `length` (`Object.assign(['wp'], { extra: 1 })`): like an unknown key
/// in a record, it would be silently ignored.
///
/// # Arguments
///
/// * `keys` - Every own string key of the array
/// * `length` - Its length
/// * `what`, `label` - The call and the field, for messages
///
/// # Errors
///
/// A message naming the first such property.
pub fn check_array_keys(
    keys: &[String],
    length: u32,
    what: &str,
    label: &str,
) -> Result<(), String> {
    let is_item = |key: &str| {
        // Canonical indices only: `"01"` is a property, not item 1.
        key.parse::<u32>()
            .is_ok_and(|index| index < length && index.to_string() == key)
    };
    match keys.iter().find(|key| *key != "length" && !is_item(key)) {
        Some(key) => Err(format!(
            "{what}: `{label}` must be a plain array of strings, got one with the property \
             `{key}`"
        )),
        None => Ok(()),
    }
}

/// `value` as a whole number from 0 to `max`, or `None` (fractional,
/// negative, too large, `NaN`, infinite). `-0` is 0.
pub fn whole_number(value: f64, max: u32) -> Option<u32> {
    if !value.is_finite() || value.fract() != 0.0 || value < 0.0 || value > f64::from(max) {
        return None;
    }
    // Exact: a whole number within 0..=u32::MAX.
    Some(value as u32)
}

/// The message for a number that is not a whole number from 0 to `max`.
fn not_whole(what: &str, label: &str, value: f64, max: u32) -> String {
    format!("{what}: `{label}` must be a whole number from 0 to {max}, got {value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned key names, as read from a JS object.
    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn records_must_be_non_array_objects() {
        assert_eq!(check_record_type(ValueType::Object, false, "x"), Ok(()));
        assert_eq!(
            check_record_type(ValueType::Object, true, "x"),
            Err("x must be an object, got an array".to_string())
        );
        let refused = [
            (ValueType::Undefined, "undefined"),
            (ValueType::Null, "null"),
            (ValueType::String, "a string"),
            (ValueType::Number, "a number"),
            (ValueType::Function, "a function"),
        ];
        for (kind, name) in refused {
            assert_eq!(
                check_record_type(kind, false, "upsertSite()"),
                Err(format!("upsertSite() must be an object, got {name}"))
            );
        }
    }

    #[test]
    fn only_plain_records_are_accepted() {
        use ValueType::{Null, Object};
        assert_eq!(check_plain_record(Object, Null, "x"), Ok(()));
        assert_eq!(check_plain_record(Null, Null, "x"), Ok(()));
        let err = check_plain_record(Object, Object, "upsertServer() `ssh`").unwrap_err();
        assert!(
            err.starts_with("upsertServer() `ssh` must be a plain object"),
            "{err}"
        );
        assert!(
            err.ends_with("copy its fields into an object literal"),
            "{err}"
        );
    }

    #[test]
    fn record_labels_name_the_nested_path() {
        assert_eq!(record_label("upsertServer()", ""), "upsertServer()");
        assert_eq!(
            record_label("upsertServer()", "ssh."),
            "upsertServer() `ssh`"
        );
        assert_eq!(record_label("setDefaults()", "ssh."), "setDefaults() `ssh`");
    }

    #[test]
    fn known_fields_are_accepted_and_unknown_ones_named() {
        assert_eq!(
            check_fields(&keys(&["id", "host"]), SERVER_KEYS, "x", ""),
            Ok(())
        );
        assert_eq!(check_fields(&[], SITE_KEYS, "x", ""), Ok(()));
        assert_eq!(
            check_fields(&keys(&["user", "prot"]), SSH_KEYS, "upsertServer()", "ssh."),
            Err(format!(
                "upsertServer(): unknown field `ssh.prot` (expected {})",
                SSH_KEYS.join(", ")
            ))
        );
        // snake_case is the file's spelling, not the JS one.
        assert!(check_fields(&keys(&["allow_root"]), SITE_KEYS, "x", "").is_err());
    }

    #[test]
    fn non_enumerable_keys_are_named() {
        assert_eq!(
            check_enumerable(&keys(&["id", "host"]), &keys(&["host", "id"]), "x", ""),
            Ok(())
        );
        assert_eq!(check_enumerable(&[], &[], "x", ""), Ok(()));
        assert_eq!(
            check_enumerable(
                &keys(&["user", "port"]),
                &keys(&["user"]),
                "upsertServer()",
                "ssh."
            ),
            Err(
                "upsertServer(): field `ssh.port` is not enumerable; define it as a plain \
                 property (e.g. in an object literal)"
                    .to_string()
            )
        );
    }

    #[test]
    fn arrays_hold_only_their_items_and_length() {
        let ok = keys(&["0", "1", "length"]);
        assert_eq!(check_array_keys(&ok, 2, "x", "wpCli"), Ok(()));
        assert_eq!(
            check_array_keys(&keys(&["length"]), 0, "x", "wpCli"),
            Ok(())
        );
        let refused = [
            (keys(&["0", "length", "extra"]), 1, "extra"),
            (keys(&["0", "1", "length"]), 1, "1"),
            (keys(&["01", "length"]), 2, "01"),
            (keys(&["-1", "length"]), 1, "-1"),
        ];
        for (keys, length, key) in refused {
            assert_eq!(
                check_array_keys(&keys, length, "upsertSite()", "wpCli"),
                Err(format!(
                    "upsertSite(): `wpCli` must be a plain array of strings, got one with the \
                     property `{key}`"
                ))
            );
        }
    }

    #[test]
    fn key_lists_mirror_the_records_in_camel_case() {
        assert_eq!(SSH_KEYS.len(), 9);
        assert_eq!(SITE_KEYS.len(), 14);
        for key in SSH_KEYS
            .iter()
            .chain(SITE_KEYS)
            .chain(SERVER_KEYS)
            .chain(GROUP_KEYS)
            .chain(DEFAULTS_KEYS)
            .chain(SAVE_KEYS)
        {
            assert!(key.chars().all(|c| c.is_ascii_alphabetic()), "{key}");
            assert!(key.starts_with(|c: char| c.is_ascii_lowercase()), "{key}");
        }
    }

    #[test]
    fn whole_numbers_within_the_limit_are_accepted() {
        assert_eq!(whole_number(0.0, MAX_PORT), Some(0));
        assert_eq!(whole_number(-0.0, MAX_PORT), Some(0));
        assert_eq!(whole_number(22.0, MAX_PORT), Some(22));
        assert_eq!(whole_number(65535.0, MAX_PORT), Some(65535));
        assert_eq!(whole_number(f64::from(u32::MAX), u32::MAX), Some(u32::MAX));
    }

    #[test]
    fn other_numbers_are_refused_not_truncated() {
        for value in [
            65536.0,
            70000.0,
            22.5,
            -1.0,
            -0.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e300,
        ] {
            assert_eq!(whole_number(value, MAX_PORT), None, "{value}");
        }
        assert_eq!(whole_number(f64::from(u32::MAX) + 1.0, u32::MAX), None);
    }

    #[test]
    fn number_messages_name_the_field_and_the_value() {
        assert_eq!(
            not_whole("upsertServer()", "ssh.port", 70000.0, MAX_PORT),
            "upsertServer(): `ssh.port` must be a whole number from 0 to 65535, got 70000"
        );
        assert_eq!(
            not_whole("x", "commandTimeoutSecs", 1.5, u32::MAX),
            "x: `commandTimeoutSecs` must be a whole number from 0 to 4294967295, got 1.5"
        );
    }
}
