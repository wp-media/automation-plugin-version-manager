//! The one-table field pattern of the inventory commands (`server`, `site`).
//!
//! Each command declares the settings its flags change once: a
//! [`TableField`] enum whose facts come from one exhaustive `match`, a
//! [`TableSetting`] enum (a field with its value) and a [`TableRecord`]
//! impl per record (how the record stores each setting). Everything else is
//! derived here, the same way for every command: applying and clearing
//! settings, set + unset conflicts, what changed, the `--unset` values and
//! remedy, and one locked change of a record ([`Change`]). A new field
//! therefore cannot be forgotten in one of these places.
//!
//! The traits are visible to `commands` only: each command's field types
//! are private to it, and a trait impl may not expose a type beyond the
//! trait's own visibility.

use std::path::Path;

use apvm_sites::inventory::io;
use apvm_sites::{Code, Inventory};
use clap::ValueEnum;
use clap::builder::{PossibleValuesParser, TypedValueParser};

use super::{Failure, name_flags};

/// A setting that a command's flags change (`--user`, `--unset user`).
pub(in crate::commands) trait TableField:
    ValueEnum + Copy + Eq + Send + Sync + 'static
{
    /// The records the table describes (one table may serve several, such
    /// as a server and the SSH defaults).
    type Scope: Copy + Eq;

    /// The `--unset` value of this field, as change reports name it.
    fn name(self) -> &'static str;

    /// Whether `scope`'s `--unset` can clear this field (it is optional and
    /// a field of that record).
    fn clearable_in(self, scope: Self::Scope) -> bool;

    /// Where each flag of `scope`'s commands lands in its record, for
    /// [`name_flags`]: `(location, flag)`.
    fn flag_map(scope: Self::Scope) -> Vec<(&'static str, &'static str)>;
}

/// A [`TableField`] with its value.
pub(in crate::commands) trait TableSetting: Clone + PartialEq {
    /// The field this setting sets.
    type Field: TableField;

    /// The field this setting sets.
    fn field(&self) -> Self::Field;

    /// The flag the user typed for it (`--no-multiplex`, not its field).
    fn flag(&self) -> &'static str;

    /// Whether the value is empty (`--notes ""`): the library refuses it,
    /// and `--unset` is how to clear the field.
    fn is_empty_value(&self) -> bool;
}

/// A record whose settings a command changes.
pub(in crate::commands) trait TableRecord: Clone {
    /// The settings of its table.
    type Setting: TableSetting;

    /// The value of `field`: `None` when unset, or not a field of this
    /// record.
    fn get(&self, field: FieldOf<Self>) -> Option<Self::Setting>;

    /// Store `setting`. A field this record does not have is ignored (the
    /// flags of a command only set fields of its record).
    fn put(&mut self, setting: Self::Setting);

    /// Clear `field`. A required field, or one this record does not have,
    /// is left as is (`--unset` never offers them).
    fn clear(&mut self, field: FieldOf<Self>);
}

/// The field type of record `R`.
pub(in crate::commands) type FieldOf<R> = <<R as TableRecord>::Setting as TableSetting>::Field;

/// The scope type of setting `S`.
pub(in crate::commands) type ScopeOf<S> = <<S as TableSetting>::Field as TableField>::Scope;

/// Every field of `F`, in declaration (report) order.
fn every<F: TableField>() -> impl Iterator<Item = F> {
    F::value_variants().iter().copied()
}

/// `record` with `settings` stored, then the `unset` fields cleared.
///
/// # Arguments
///
/// * `record` - The current record
/// * `settings` - What the flags set
/// * `unset` - What `--unset` clears
pub fn apply<R: TableRecord>(record: &R, settings: &[R::Setting], unset: &[FieldOf<R>]) -> R {
    let mut record = record.clone();
    for setting in settings {
        record.put(setting.clone());
    }
    for field in unset {
        record.clear(*field);
    }
    record
}

/// Refuse a field that is both set and cleared in one command.
///
/// # Errors
///
/// [`Failure::Usage`] naming the first such field by the flag typed
/// (`--no-multiplex and --unset multiplex contradict each other: pass only
/// one`).
pub fn check<S: TableSetting>(settings: &[S], unset: &[S::Field]) -> Result<(), Failure> {
    match settings
        .iter()
        .find(|setting| unset.contains(&setting.field()))
    {
        Some(setting) => Err(Failure::Usage(format!(
            "{} and --unset {} contradict each other: pass only one",
            setting.flag(),
            setting.field().name()
        ))),
        None => Ok(()),
    }
}

/// The fields whose values differ between two versions of a record, in
/// field order.
pub fn changed<R: TableRecord>(before: &R, after: &R) -> Vec<FieldOf<R>> {
    every::<FieldOf<R>>()
        .filter(|field| before.get(*field) != after.get(*field))
        .collect()
}

/// The `--unset` remedy for empty values (`--notes ""`), which the library
/// refuses: `to clear it: --unset notes`. `None` without an empty value
/// that `scope`'s `--unset` can clear (an empty `--host` has no remedy).
pub fn empty_value_remedy<S: TableSetting>(settings: &[S], scope: ScopeOf<S>) -> Option<String> {
    let empty: Vec<&str> = settings
        .iter()
        .filter(|setting| setting.is_empty_value())
        .map(TableSetting::field)
        .filter(|field| field.clearable_in(scope))
        .map(TableField::name)
        .collect();
    let pronoun = match empty.len() {
        0 => return None,
        1 => "it",
        _ => "them",
    };
    Some(format!("to clear {pronoun}: --unset {}", empty.join(",")))
}

/// The `--unset` values of `scope`'s command: the fields it can clear,
/// with their help.
pub fn unset_parser<F: TableField>(scope: F::Scope) -> impl TypedValueParser<Value = F> {
    let values = every::<F>()
        .filter(move |field| field.clearable_in(scope))
        .filter_map(|field| field.to_possible_value());
    PossibleValuesParser::new(values).try_map(|name| F::from_str(&name, false))
}

/// `"<done>: a, b.\n"`, or `"<unchanged>\n"` when nothing changed.
///
/// # Arguments
///
/// * `done` - The report of a change (`Updated server 'eu-1'`)
/// * `unchanged` - The report when nothing changed, with its full stop
/// * `changed` - The changed fields, named by their `--unset` value
pub fn change_line<F: TableField>(done: &str, unchanged: &str, changed: &[F]) -> String {
    let names: Vec<&str> = changed.iter().map(|field| field.name()).collect();
    names_line(done, unchanged, &names)
}

/// [`change_line`] with the changes already named (a command that also
/// reports something that is not a field, such as joined groups).
///
/// # Arguments
///
/// * `done` - The report of a change
/// * `unchanged` - The report when nothing changed, with its full stop
/// * `names` - What changed
pub fn names_line(done: &str, unchanged: &str, names: &[&str]) -> String {
    if names.is_empty() {
        return format!("{unchanged}\n");
    }
    format!("{done}: {}.\n", names.join(", "))
}

/// A checked change of one record: what the flags set and clear.
pub struct Change<'a, S: TableSetting> {
    /// Which record it changes.
    scope: ScopeOf<S>,
    /// What the flags set.
    settings: &'a [S],
    /// What `--unset` clears.
    unset: &'a [S::Field],
}

impl<'a, S: TableSetting> Change<'a, S> {
    /// The change, unless it sets and clears the same field.
    ///
    /// # Errors
    ///
    /// [`Failure::Usage`] from [`check`].
    pub fn new(
        scope: ScopeOf<S>,
        settings: &'a [S],
        unset: &'a [S::Field],
    ) -> Result<Self, Failure> {
        check(settings, unset)?;
        Ok(Self {
            scope,
            settings,
            unset,
        })
    }

    /// Apply the change in one `io::update` (see [`Change::apply_to`]).
    ///
    /// # Returns
    ///
    /// The record before and after.
    ///
    /// # Errors
    ///
    /// Those of [`Change::apply_to`] and of `io::update`.
    pub fn save<R: TableRecord<Setting = S>, T>(
        &self,
        path: &Path,
        current: impl Fn(&Inventory) -> Result<R, Failure>,
        store: impl Fn(&mut Inventory, R) -> Result<T, apvm_sites::Error>,
    ) -> Result<(R, R), Failure> {
        io::update(path, |inventory| self.apply_to(inventory, current, store))
    }

    /// Apply the change to a loaded (locked) inventory: `current` reads the
    /// record, `store` hands the changed one to the library, which
    /// validates it (what it returns, such as the replaced record, is not
    /// needed). For a command that changes more in the same `io::update`.
    ///
    /// # Returns
    ///
    /// The record before and after.
    ///
    /// # Errors
    ///
    /// Those of `current`, and the library's (refused input naming the
    /// flags, with the `--unset` remedy for an empty value).
    pub fn apply_to<R: TableRecord<Setting = S>, T>(
        &self,
        inventory: &mut Inventory,
        current: impl Fn(&Inventory) -> Result<R, Failure>,
        store: impl Fn(&mut Inventory, R) -> Result<T, apvm_sites::Error>,
    ) -> Result<(R, R), Failure> {
        self.apply_with(inventory, current, store, |_, _, _| None)
    }

    /// [`Change::apply_to`], with the command's own remedy for a refusal:
    /// `remedy` sees the inventory (unchanged: a refused change changes
    /// nothing), the changed record and the library's error, and may name
    /// what to do (shown after the `--unset` remedy, if any).
    ///
    /// # Errors
    ///
    /// As [`Change::apply_to`].
    pub fn apply_with<R: TableRecord<Setting = S>, T>(
        &self,
        inventory: &mut Inventory,
        current: impl Fn(&Inventory) -> Result<R, Failure>,
        store: impl Fn(&mut Inventory, R) -> Result<T, apvm_sites::Error>,
        remedy: impl Fn(&Inventory, &R, &apvm_sites::Error) -> Option<String>,
    ) -> Result<(R, R), Failure> {
        let before = current(inventory)?;
        let after = apply(&before, self.settings, self.unset);
        if let Err(error) = store(inventory, after.clone()) {
            let own = remedy(inventory, &after, &error);
            let failure = self.refused(error);
            return Err(match own {
                Some(own) => failure.with_remedy(own),
                None => failure,
            });
        }
        Ok((before, after))
    }

    /// The library's refusal, naming the flags; refused input with an
    /// empty value also gets the CLI's remedy (`to clear it: --unset notes`).
    pub fn refused(&self, error: apvm_sites::Error) -> Failure {
        let remedy = (error.code() == Code::InvalidInput)
            .then(|| empty_value_remedy(self.settings, self.scope))
            .flatten();
        let failure = name_flags(error, &S::Field::flag_map(self.scope));
        match remedy {
            Some(remedy) => failure.with_remedy(remedy),
            None => failure,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The generic machinery on a two-field table of its own, so these
    //! tests do not depend on any command's table.

    use super::*;

    /// A tiny table: a required `id`-like field and an optional one.
    #[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
    enum Pet {
        /// Required
        Kind,
        /// Optional
        Nick,
    }

    impl TableField for Pet {
        type Scope = ();

        fn name(self) -> &'static str {
            match self {
                Self::Kind => "kind",
                Self::Nick => "nick",
            }
        }

        fn clearable_in(self, (): ()) -> bool {
            self == Self::Nick
        }

        fn flag_map((): ()) -> Vec<(&'static str, &'static str)> {
            vec![("kind", "--kind"), ("nick", "--nick")]
        }
    }

    /// A [`Pet`] with its value.
    #[derive(Clone, Debug, PartialEq)]
    enum PetSetting {
        /// `--kind`
        Kind(String),
        /// `--nick`
        Nick(String),
    }

    impl TableSetting for PetSetting {
        type Field = Pet;

        fn field(&self) -> Pet {
            match self {
                Self::Kind(_) => Pet::Kind,
                Self::Nick(_) => Pet::Nick,
            }
        }

        fn flag(&self) -> &'static str {
            match self {
                Self::Kind(_) => "--kind",
                Self::Nick(_) => "--nick",
            }
        }

        fn is_empty_value(&self) -> bool {
            matches!(self, Self::Kind(text) | Self::Nick(text) if text.is_empty())
        }
    }

    /// The record of the tiny table.
    #[derive(Clone, Debug, PartialEq, Default)]
    struct PetRecord {
        /// Required.
        kind: String,
        /// Optional.
        nick: Option<String>,
    }

    impl TableRecord for PetRecord {
        type Setting = PetSetting;

        fn get(&self, field: Pet) -> Option<PetSetting> {
            match field {
                Pet::Kind => Some(PetSetting::Kind(self.kind.clone())),
                Pet::Nick => self.nick.clone().map(PetSetting::Nick),
            }
        }

        fn put(&mut self, setting: PetSetting) {
            match setting {
                PetSetting::Kind(kind) => self.kind = kind,
                PetSetting::Nick(nick) => self.nick = Some(nick),
            }
        }

        fn clear(&mut self, field: Pet) {
            if field == Pet::Nick {
                self.nick = None;
            }
        }
    }

    #[test]
    fn apply_sets_then_clears_and_changed_names_the_difference() {
        let before = PetRecord {
            kind: "cat".into(),
            nick: Some("Tom".into()),
        };
        let settings = [PetSetting::Kind("dog".into())];
        let after = apply(&before, &settings, &[Pet::Nick, Pet::Kind]);
        assert_eq!(after.kind, "dog", "a required field is never cleared");
        assert_eq!(after.nick, None);
        assert_eq!(changed(&before, &after), [Pet::Kind, Pet::Nick]);
        assert!(changed(&before, &before).is_empty());
    }

    #[test]
    fn check_names_the_flag_typed() {
        let settings = [PetSetting::Nick("x".into())];
        assert!(check(&settings, &[Pet::Kind]).is_ok());
        let refused = check(&settings, &[Pet::Nick]).expect_err("contradiction");
        assert_eq!(
            refused.to_string(),
            "--nick and --unset nick contradict each other: pass only one"
        );
        assert_eq!(refused.code(), "invalid_input");
    }

    #[test]
    fn empty_values_get_the_remedy_only_when_clearable() {
        let empty_kind = PetSetting::Kind(String::new());
        let empty_nick = PetSetting::Nick(String::new());
        assert_eq!(
            empty_value_remedy(std::slice::from_ref(&empty_kind), ()),
            None
        );
        assert_eq!(
            empty_value_remedy(&[empty_kind, empty_nick], ()).as_deref(),
            Some("to clear it: --unset nick")
        );
        assert_eq!(
            empty_value_remedy(&[PetSetting::Nick("x".into())], ()),
            None
        );
    }

    #[test]
    fn unset_parser_offers_only_clearable_fields() {
        let command = clap::Command::new("t").arg(
            clap::Arg::new("unset")
                .long("unset")
                .value_parser(unset_parser::<Pet>(())),
        );
        let matches = command
            .clone()
            .try_get_matches_from(["t", "--unset", "nick"])
            .expect("nick is clearable");
        assert_eq!(matches.get_one::<Pet>("unset"), Some(&Pet::Nick));
        let refused = command
            .try_get_matches_from(["t", "--unset", "kind"])
            .expect_err("kind is required");
        assert_eq!(refused.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn lines_report_the_changes_or_that_nothing_changed() {
        assert_eq!(
            change_line::<Pet>("Updated p", "p is unchanged.", &[]),
            "p is unchanged.\n"
        );
        assert_eq!(
            change_line("Updated p", "p is unchanged.", &[Pet::Kind, Pet::Nick]),
            "Updated p: kind, nick.\n"
        );
        assert_eq!(
            names_line("Updated p", "same.", &["groups"]),
            "Updated p: groups.\n"
        );
    }

    #[test]
    fn a_change_writes_once_and_names_the_flags_of_refused_input() {
        use apvm_sites::{EntityKind, Server};
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("inventory.json");
        io::update(&path, |inventory| {
            inventory.add_server(Server::new("eu-1", "h"))
        })
        .expect("seed");
        let settings = [PetSetting::Nick(String::new())];
        let change = Change::new((), &settings, &[]).expect("no contradiction");
        // The pet stands in for a server's notes: an empty value is refused.
        let refused = change
            .save(
                &path,
                |inventory| {
                    let server = inventory.server("eu-1").cloned();
                    let server = server
                        .ok_or_else(|| super::super::not_found(EntityKind::Server, "eu-1"))?;
                    Ok(PetRecord {
                        kind: server.host,
                        nick: server.notes,
                    })
                },
                |inventory, pet| {
                    let mut server = Server::new("eu-1", pet.kind);
                    server.notes = pet.nick;
                    inventory.update_server(server)
                },
            )
            .expect_err("empty notes are refused");
        let text = refused.to_string();
        assert!(
            text.contains("\n  notes: "),
            "the map has no 'notes' location: {text}"
        );
        assert!(text.ends_with("\n  to clear it: --unset nick"), "{text}");
        assert!(
            Change::new((), &settings, &[Pet::Nick]).is_err(),
            "set + unset is refused before any I/O"
        );
    }

    #[test]
    fn apply_with_adds_the_commands_remedy_after_the_unset_one() {
        use apvm_sites::Server;
        let mut inventory = Inventory::new();
        inventory
            .add_server(Server::new("eu-1", "h"))
            .expect("valid server");
        let settings = [PetSetting::Nick(String::new())];
        let change = Change::new((), &settings, &[]).expect("no contradiction");
        let current = |inventory: &Inventory| {
            let server = inventory.server("eu-1").cloned().expect("seeded");
            Ok(PetRecord {
                kind: server.host,
                nick: server.notes,
            })
        };
        let store = |inventory: &mut Inventory, pet: PetRecord| {
            let mut server = Server::new("eu-1", pet.kind);
            server.notes = pet.nick;
            inventory.update_server(server)
        };
        let refused = change
            .apply_with(
                &mut inventory,
                current,
                store,
                |inventory, pet: &PetRecord, error| {
                    assert!(
                        inventory.server("eu-1").is_some(),
                        "the unchanged inventory"
                    );
                    assert_eq!(error.code(), Code::InvalidInput);
                    Some(format!("own remedy for {:?}", pet.nick))
                },
            )
            .expect_err("empty notes are refused");
        assert!(
            refused
                .to_string()
                .ends_with("\n  to clear it: --unset nick\n  own remedy for Some(\"\")"),
            "{refused}"
        );
        // A change the library accepts never asks for a remedy.
        let fine = [PetSetting::Nick("n".into())];
        let change = Change::new((), &fine, &[]).expect("no contradiction");
        let (_, after) = change
            .apply_with(&mut inventory, current, store, |_, _: &PetRecord, _| {
                unreachable!("no refusal, no remedy")
            })
            .expect("accepted");
        assert_eq!(after.nick.as_deref(), Some("n"));
    }
}
