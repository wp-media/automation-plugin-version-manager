//! Every setting the `group` flags change, declared once.
//!
//! The same one-table pattern as `site/fields.rs`: a [`Field`] is a setting
//! (`--notes`, `--unset notes`, `notes`), a [`Setting`] is a field with its
//! value, each fact about a field is one arm of the exhaustive
//! [`Field::spec`], and how the record stores it is one arm of each
//! [`TableRecord`] method. The shared [`field_table`] derives the rest.
//!
//! A group's member sites are not a setting: `--add` and `--remove`
//! change them one site at a time (see `group.rs`), so they are not in
//! this table.

use apvm_sites::Group;
use clap::ValueEnum;
use clap::builder::TypedValueParser;

use crate::commands::inventory::field_table::{self, TableField, TableRecord, TableSetting};

/// A setting of a group. The variant docs are the `--unset` value help.
/// Visible to `commands`, like the shared table traits it implements.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Field {
    /// Free-form notes
    Notes,
}

/// The record a command changes (one: the shared table is written for
/// tables that serve several).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Scope {
    /// A group (`group add` / `group edit`).
    Group,
}

/// The fixed facts of a [`Field`].
#[derive(Clone, Copy, Debug)]
struct Spec {
    /// The `--unset` value, also used in change reports (`notes`).
    name: &'static str,
    /// The flag that sets it.
    flag: &'static str,
    /// Where the library reports refused input of it (`notes`).
    location: &'static str,
}

impl Field {
    /// The facts of this field: the one table every aspect derives from.
    /// Every group setting is optional (the id and the sites are not
    /// settings), so none is marked required.
    const fn spec(self) -> Spec {
        match self {
            Self::Notes => Spec {
                name: "notes",
                flag: "--notes",
                location: "notes",
            },
        }
    }

    /// Every field, in report order.
    fn all() -> impl Iterator<Item = Self> {
        Self::value_variants().iter().copied()
    }
}

impl TableField for Field {
    type Scope = Scope;

    fn name(self) -> &'static str {
        self.spec().name
    }

    fn clearable_in(self, Scope::Group: Scope) -> bool {
        true
    }

    fn flag_map(Scope::Group: Scope) -> Vec<(&'static str, &'static str)> {
        flag_map()
    }
}

/// A [`Field`] with its value, as the flags give it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Setting {
    /// `--notes`
    Notes(String),
}

impl TableSetting for Setting {
    type Field = Field;

    fn field(&self) -> Field {
        match self {
            Self::Notes(_) => Field::Notes,
        }
    }

    fn flag(&self) -> &'static str {
        self.field().spec().flag
    }

    fn is_empty_value(&self) -> bool {
        match self {
            Self::Notes(text) => text.is_empty(),
        }
    }
}

/// How a group stores each setting.
impl TableRecord for Group {
    type Setting = Setting;

    fn get(&self, field: Field) -> Option<Setting> {
        match field {
            Field::Notes => self.notes.clone().map(Setting::Notes),
        }
    }

    fn put(&mut self, setting: Setting) {
        match setting {
            Setting::Notes(notes) => self.notes = Some(notes),
        }
    }

    fn clear(&mut self, field: Field) {
        match field {
            Field::Notes => self.notes = None,
        }
    }
}

/// Where each flag of the group commands lands in the record, for
/// `inventory::name_flags`: `(location, flag)`, the id first. The member
/// sites are checked by the command before the library sees them (an
/// unknown one is `site '…' does not exist`, a repeat counts once), so
/// the library never refuses `sites[i]` for them.
pub(super) fn flag_map() -> Vec<(&'static str, &'static str)> {
    let fields = Field::all()
        .map(Field::spec)
        .map(|spec| (spec.location, spec.flag));
    std::iter::once(("id", "<ID>")).chain(fields).collect()
}

/// The `--unset` values: every field, with its help.
pub(super) fn unset_parser() -> impl TypedValueParser<Value = Field> {
    field_table::unset_parser::<Field>(Scope::Group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::inventory::field_table::{apply, changed, empty_value_remedy};

    /// One sample setting of each field.
    fn one_of_each() -> Vec<Setting> {
        vec![Setting::Notes("n".into())]
    }

    #[test]
    fn every_field_has_one_sample_setting() {
        let fields: Vec<Field> = one_of_each().iter().map(TableSetting::field).collect();
        assert_eq!(fields, Field::all().collect::<Vec<_>>());
    }

    #[test]
    fn the_group_stores_reads_and_clears_every_field() {
        let blank = Group::new("qa", vec!["shop".into()]);
        for setting in one_of_each() {
            let field = setting.field();
            let mut group = blank.clone();
            group.put(setting.clone());
            assert_eq!(group.get(field), Some(setting.clone()), "{field:?}");
            assert_eq!(changed(&blank, &group), [field], "{field:?}");
            group.clear(field);
            assert_eq!(group.get(field), None, "{field:?} after clear");
            assert_eq!(group, blank, "only the field changed");
        }
    }

    #[test]
    fn unset_names_flags_and_help_agree() {
        for field in Field::all() {
            let value = field.to_possible_value().expect("listed");
            let spec = field.spec();
            assert_eq!(value.get_name(), spec.name);
            assert_eq!(spec.flag, format!("--{}", spec.name), "{field:?}");
            assert!(value.get_help().is_some(), "{field:?} has help");
            assert!(field.clearable_in(Scope::Group));
        }
    }

    #[test]
    fn the_flag_map_covers_the_id_and_every_field() {
        assert_eq!(flag_map(), [("id", "<ID>"), ("notes", "--notes")]);
        assert_eq!(Field::flag_map(Scope::Group), flag_map());
    }

    #[test]
    fn an_empty_note_is_an_empty_value_with_the_unset_remedy() {
        assert!(Setting::Notes(String::new()).is_empty_value());
        assert!(!Setting::Notes(" ".into()).is_empty_value());
        assert_eq!(
            empty_value_remedy(&[Setting::Notes(String::new())], Scope::Group).as_deref(),
            Some("to clear it: --unset notes")
        );
    }

    #[test]
    fn apply_sets_then_clears_and_never_touches_the_sites() {
        let group = Group::new("qa", vec!["shop".into(), "blog".into()]);
        let noted = apply(&group, &[Setting::Notes("n".into())], &[]);
        assert_eq!(noted.notes.as_deref(), Some("n"));
        assert_eq!(noted.sites, group.sites);
        assert_eq!(apply(&noted, &[], &[Field::Notes]), group);
    }
}
