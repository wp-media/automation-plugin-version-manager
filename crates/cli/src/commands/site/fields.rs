//! Every setting the `site` flags change, declared once.
//!
//! A [`Field`] is a setting (`--run-as`, `--unset run-as`, `run_as`); a
//! [`Setting`] is a field with its value. Each fact about a field — its
//! `--unset` name, its flag, its place in the record, whether it is
//! required — is one arm of the exhaustive [`Field::spec`], and how the
//! record stores it is one arm of each [`TableRecord`] method, so a new
//! field does not compile until every fact is given. The shared
//! [`field_table`] derives the rest: applying and clearing settings,
//! set + unset conflicts, what changed, the `--unset` values and remedy,
//! and the flag names of refused input.
//!
//! None of these values is a path on this machine (`--path`,
//! `--plugins-dir`, `--tmp-dir` and the `--wp-cli` tokens are remote and
//! must be absolute), so unlike `server`, nothing warns about relative
//! paths: the library refuses them.

use apvm_sites::{Owner, OwnershipPolicy, RunAs, Site};
use clap::ValueEnum;
use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};

use crate::commands::inventory::field_table::{self, TableField, TableRecord, TableSetting};

/// A setting of a site. The variant docs are the `--unset` value help.
/// Visible to `commands`, like the shared table traits it implements.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Field {
    /// Server the site is on
    Server,
    /// WordPress root on the server
    Path,
    /// Site URL
    Url,
    /// WP-CLI command (back to wp)
    WpCli,
    /// Who WP-CLI runs as (back to auto)
    RunAs,
    /// Owner of the plugin files (back to auto)
    Owner,
    /// Ownership policy (back to mirror)
    Ownership,
    /// Pinned plugins directory
    PluginsDir,
    /// Root allowed (back to no)
    AllowRoot,
    /// Remote temporary directory (back to /tmp)
    TmpDir,
    /// Remote command timeout
    Timeout,
    /// Display name
    Name,
    /// Free-form notes
    Notes,
}

/// The record a command changes: the site table describes one record, but
/// the shared table is written for tables that serve several.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Scope {
    /// A site (`site add` / `site edit`).
    Site,
}

/// The fixed facts of a [`Field`].
#[derive(Clone, Copy, Debug)]
struct Spec {
    /// The `--unset` value, also used in change reports (`run-as`).
    name: &'static str,
    /// The flag that sets it.
    flag: &'static str,
    /// Where the library reports refused input of it (`run_as`).
    location: &'static str,
    /// Whether it is required, so `--unset` cannot clear it.
    required: bool,
}

/// An optional [`Spec`] (only the server and the path are required).
const fn spec(name: &'static str, flag: &'static str, location: &'static str) -> Spec {
    Spec {
        name,
        flag,
        location,
        required: false,
    }
}

impl Field {
    /// The facts of this field: the one table every aspect derives from.
    const fn spec(self) -> Spec {
        match self {
            Self::Server => Spec {
                required: true,
                ..spec("server", "--server", "server")
            },
            Self::Path => Spec {
                required: true,
                ..spec("path", "--path", "path")
            },
            Self::Url => spec("url", "--url", "url"),
            Self::WpCli => spec("wp-cli", "--wp-cli", "wp_cli"),
            Self::RunAs => spec("run-as", "--run-as", "run_as"),
            Self::Owner => spec("owner", "--owner", "owner"),
            Self::Ownership => spec("ownership", "--ownership", "ownership"),
            Self::PluginsDir => spec("plugins-dir", "--plugins-dir", "plugins_dir"),
            Self::AllowRoot => spec("allow-root", "--allow-root", "allow_root"),
            Self::TmpDir => spec("tmp-dir", "--tmp-dir", "remote_tmp_dir"),
            Self::Timeout => spec("timeout", "--timeout", "command_timeout_secs"),
            Self::Name => spec("name", "--name", "name"),
            Self::Notes => spec("notes", "--notes", "notes"),
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

    fn clearable_in(self, Scope::Site: Scope) -> bool {
        !self.spec().required
    }

    fn flag_map(Scope::Site: Scope) -> Vec<(&'static str, &'static str)> {
        flag_map()
    }
}

/// A [`Field`] with its value, as the flags give it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Setting {
    /// `--server`
    Server(String),
    /// `--path`
    Path(String),
    /// `--url`
    Url(String),
    /// `--wp-cli`, split into its tokens (see [`wp_cli_tokens`])
    WpCli(Vec<String>),
    /// `--run-as`, as written (`auto`, `login` or a user)
    RunAs(String),
    /// `--owner`, as written (`auto`, `user` or `user:group`)
    Owner(String),
    /// `--ownership`
    Ownership(OwnershipPolicy),
    /// `--plugins-dir`
    PluginsDir(String),
    /// `--allow-root`
    AllowRoot(bool),
    /// `--tmp-dir`
    TmpDir(String),
    /// `--timeout`
    Timeout(u32),
    /// `--name`
    Name(String),
    /// `--notes`
    Notes(String),
}

impl TableSetting for Setting {
    type Field = Field;

    fn field(&self) -> Field {
        match self {
            Self::Server(_) => Field::Server,
            Self::Path(_) => Field::Path,
            Self::Url(_) => Field::Url,
            Self::WpCli(_) => Field::WpCli,
            Self::RunAs(_) => Field::RunAs,
            Self::Owner(_) => Field::Owner,
            Self::Ownership(_) => Field::Ownership,
            Self::PluginsDir(_) => Field::PluginsDir,
            Self::AllowRoot(_) => Field::AllowRoot,
            Self::TmpDir(_) => Field::TmpDir,
            Self::Timeout(_) => Field::Timeout,
            Self::Name(_) => Field::Name,
            Self::Notes(_) => Field::Notes,
        }
    }

    fn flag(&self) -> &'static str {
        self.field().spec().flag
    }

    /// An empty text, or a `--wp-cli` without a token (`--wp-cli " "`).
    fn is_empty_value(&self) -> bool {
        match self {
            Self::WpCli(tokens) => tokens.is_empty(),
            Self::Server(text)
            | Self::Path(text)
            | Self::Url(text)
            | Self::RunAs(text)
            | Self::Owner(text)
            | Self::PluginsDir(text)
            | Self::TmpDir(text)
            | Self::Name(text)
            | Self::Notes(text) => text.is_empty(),
            Self::Ownership(_) | Self::AllowRoot(_) | Self::Timeout(_) => false,
        }
    }
}

/// How a site stores each setting. `run_as` and `owner` keep their
/// written form, which reads back unchanged (`RunAs::from(text)` displays
/// as `text`), so a report compares what the user typed.
impl TableRecord for Site {
    type Setting = Setting;

    fn get(&self, field: Field) -> Option<Setting> {
        match field {
            Field::Server => Some(Setting::Server(self.server.clone())),
            Field::Path => Some(Setting::Path(self.path.clone())),
            Field::Url => self.url.clone().map(Setting::Url),
            Field::WpCli => self.wp_cli.clone().map(Setting::WpCli),
            Field::RunAs => self
                .run_as
                .as_ref()
                .map(|run_as| Setting::RunAs(run_as.to_string())),
            Field::Owner => self
                .owner
                .as_ref()
                .map(|owner| Setting::Owner(owner.to_string())),
            Field::Ownership => self.ownership.map(Setting::Ownership),
            Field::PluginsDir => self.plugins_dir.clone().map(Setting::PluginsDir),
            Field::AllowRoot => self.allow_root.map(Setting::AllowRoot),
            Field::TmpDir => self.remote_tmp_dir.clone().map(Setting::TmpDir),
            Field::Timeout => self.command_timeout_secs.map(Setting::Timeout),
            Field::Name => self.name.clone().map(Setting::Name),
            Field::Notes => self.notes.clone().map(Setting::Notes),
        }
    }

    fn put(&mut self, setting: Setting) {
        match setting {
            Setting::Server(server) => self.server = server,
            Setting::Path(path) => self.path = path,
            Setting::Url(url) => self.url = Some(url),
            Setting::WpCli(tokens) => self.wp_cli = Some(tokens),
            Setting::RunAs(run_as) => self.run_as = Some(RunAs::from(run_as)),
            Setting::Owner(owner) => self.owner = Some(Owner::from(owner)),
            Setting::Ownership(policy) => self.ownership = Some(policy),
            Setting::PluginsDir(dir) => self.plugins_dir = Some(dir),
            Setting::AllowRoot(allow) => self.allow_root = Some(allow),
            Setting::TmpDir(dir) => self.remote_tmp_dir = Some(dir),
            Setting::Timeout(secs) => self.command_timeout_secs = Some(secs),
            Setting::Name(name) => self.name = Some(name),
            Setting::Notes(notes) => self.notes = Some(notes),
        }
    }

    fn clear(&mut self, field: Field) {
        match field {
            Field::Server | Field::Path => {}
            Field::Url => self.url = None,
            Field::WpCli => self.wp_cli = None,
            Field::RunAs => self.run_as = None,
            Field::Owner => self.owner = None,
            Field::Ownership => self.ownership = None,
            Field::PluginsDir => self.plugins_dir = None,
            Field::AllowRoot => self.allow_root = None,
            Field::TmpDir => self.remote_tmp_dir = None,
            Field::Timeout => self.command_timeout_secs = None,
            Field::Name => self.name = None,
            Field::Notes => self.notes = None,
        }
    }
}

/// The tokens of a `--wp-cli` command (`"/usr/bin/php8.3 /opt/wp-cli.phar"`):
/// split at ASCII whitespace. The library allows only `[A-Za-z0-9._/+-]`
/// in a token, so no quoting is needed and none is honored: a quote is
/// refused, and so is any other whitespace (a no-break space stays in its
/// token, where the library refuses and escapes it).
///
/// # Arguments
///
/// * `command` - The flag's value
pub(super) fn wp_cli_tokens(command: &str) -> Vec<String> {
    command.split_ascii_whitespace().map(String::from).collect()
}

/// Where each flag of the site commands lands in the record, for
/// `inventory::name_flags`: `(location, flag)`, the id first.
pub(super) fn flag_map() -> Vec<(&'static str, &'static str)> {
    let fields = Field::all()
        .map(Field::spec)
        .map(|spec| (spec.location, spec.flag));
    std::iter::once(("id", "<ID>")).chain(fields).collect()
}

/// The `--unset` values: every optional field, with its help.
pub(super) fn unset_parser() -> impl TypedValueParser<Value = Field> {
    field_table::unset_parser::<Field>(Scope::Site)
}

/// The `--ownership` values, each with its help; anything else is an
/// argument error (exit 2).
pub(super) fn ownership_parser() -> impl TypedValueParser<Value = OwnershipPolicy> {
    let values = OwnershipPolicy::ALL.map(|policy| {
        PossibleValue::new(policy.as_str()).help(match policy {
            OwnershipPolicy::Mirror => "Check owner, group and group bits; repair without root",
            OwnershipPolicy::MirrorRoot => "As mirror, plus the guarded root chown",
            OwnershipPolicy::Check => "Report only",
            OwnershipPolicy::Off => "Neither check nor repair",
        })
    });
    PossibleValuesParser::new(values).try_map(|name| name.parse::<OwnershipPolicy>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::inventory::field_table::{apply, changed, empty_value_remedy};

    /// One sample setting of each field.
    fn one_of_each() -> Vec<Setting> {
        vec![
            Setting::Server("eu-2".into()),
            Setting::Path("/srv/www".into()),
            Setting::Url("https://x.example".into()),
            Setting::WpCli(vec!["/usr/bin/wp".into()]),
            Setting::RunAs("login".into()),
            Setting::Owner("ftp:www-data".into()),
            Setting::Ownership(OwnershipPolicy::Check),
            Setting::PluginsDir("/srv/www/app/plugins".into()),
            Setting::AllowRoot(true),
            Setting::TmpDir("/var/tmp".into()),
            Setting::Timeout(600),
            Setting::Name("N".into()),
            Setting::Notes("n".into()),
        ]
    }

    #[test]
    fn every_field_has_one_sample_setting() {
        let fields: Vec<Field> = one_of_each().iter().map(TableSetting::field).collect();
        assert_eq!(fields, Field::all().collect::<Vec<_>>());
    }

    #[test]
    fn the_site_stores_reads_and_clears_every_field() {
        let blank = Site::new("shop", "eu-1", "/var/www/shop");
        for setting in one_of_each() {
            let field = setting.field();
            let mut site = blank.clone();
            site.put(setting.clone());
            assert_eq!(site.get(field), Some(setting.clone()), "{field:?}");
            assert_eq!(changed(&blank, &site), [field], "{field:?}");
            site.clear(field);
            let required = field.spec().required;
            let expected = required.then(|| setting.clone());
            assert_eq!(site.get(field), expected, "{field:?} after clear");
        }
    }

    #[test]
    fn written_forms_of_identities_read_back_unchanged() {
        for text in ["auto", "login", "deploy", "Root"] {
            let mut site = Site::new("s", "e", "/p");
            site.put(Setting::RunAs(text.into()));
            assert_eq!(site.get(Field::RunAs), Some(Setting::RunAs(text.into())));
        }
        for text in ["auto", "ftp", "ftp:www-data", "a:b:c", ":"] {
            let mut site = Site::new("s", "e", "/p");
            site.put(Setting::Owner(text.into()));
            assert_eq!(site.get(Field::Owner), Some(Setting::Owner(text.into())));
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
            assert_eq!(field.clearable_in(Scope::Site), !spec.required);
        }
    }

    #[test]
    fn the_flag_map_covers_the_id_and_every_field() {
        let map = flag_map();
        assert_eq!(map[0], ("id", "<ID>"));
        assert_eq!(map.len(), 1 + Field::all().count());
        assert!(map.contains(&("remote_tmp_dir", "--tmp-dir")));
        assert!(map.contains(&("command_timeout_secs", "--timeout")));
        assert!(map.contains(&("wp_cli", "--wp-cli")));
    }

    #[test]
    fn wp_cli_splits_at_any_whitespace() {
        assert_eq!(
            wp_cli_tokens(" /usr/bin/php8.3\t-n  /opt/wp-cli.phar \n"),
            ["/usr/bin/php8.3", "-n", "/opt/wp-cli.phar"]
        );
        assert!(wp_cli_tokens("").is_empty());
        assert!(wp_cli_tokens(" \t ").is_empty());
        // Quotes are not syntax, and only ASCII whitespace separates: the
        // rest stays in the token (and the library refuses it).
        assert_eq!(wp_cli_tokens("'wp'"), ["'wp'"]);
        assert_eq!(wp_cli_tokens("wp\u{a0}x"), ["wp\u{a0}x"]);
    }

    #[test]
    fn empty_values_are_text_or_a_command_without_tokens() {
        assert!(Setting::WpCli(Vec::new()).is_empty_value());
        assert!(!Setting::WpCli(vec!["wp".into()]).is_empty_value());
        assert!(Setting::RunAs(String::new()).is_empty_value());
        assert!(!Setting::Timeout(0).is_empty_value());
        assert!(!Setting::AllowRoot(false).is_empty_value());
        let settings = [Setting::Path(String::new()), Setting::WpCli(Vec::new())];
        assert_eq!(
            empty_value_remedy(&settings, Scope::Site).as_deref(),
            Some("to clear it: --unset wp-cli"),
            "the path is required: no remedy for it"
        );
    }

    #[test]
    fn apply_keeps_the_required_fields_on_unset() {
        let site = Site::new("s", "e", "/p");
        let applied = apply(
            &site,
            &[Setting::Url("https://a.example".into())],
            &[Field::Name],
        );
        assert_eq!(applied.url.as_deref(), Some("https://a.example"));
        let cleared = apply(&applied, &[], &[Field::Url, Field::Server, Field::Path]);
        assert_eq!(cleared, site);
    }

    #[test]
    fn ownership_accepts_only_the_policies() {
        let command = clap::Command::new("t").arg(
            clap::Arg::new("ownership")
                .long("ownership")
                .value_parser(ownership_parser()),
        );
        for policy in OwnershipPolicy::ALL {
            let matches = command
                .clone()
                .try_get_matches_from(["t", "--ownership", policy.as_str()])
                .expect("valid");
            assert_eq!(
                matches.get_one::<OwnershipPolicy>("ownership"),
                Some(&policy)
            );
        }
        let refused = command
            .try_get_matches_from(["t", "--ownership", "Mirror"])
            .expect_err("case matters");
        assert_eq!(refused.kind(), clap::error::ErrorKind::InvalidValue);
    }
}
