//! The pure rules of package inspection (plan §6.10): entry paths, the
//! single plugin folder, which PHP files can hold the plugin header, the
//! main file, and the variant. No I/O: the zip is read in `archive`.
//!
//! The structure rules mirror what WordPress does with the zip (verified in
//! WordPress 7.1.3): `_unzip_file_ziparchive()` skips `__MACOSX/` entries
//! and creates every other file and folder; `WP_Upgrader::install_package()`
//! uses the extracted tree's only top-level item as the plugin folder when
//! it is a folder, and otherwise names the folder after the zip; and
//! `get_plugins()` looks for headers in the folder's own `*.php` files,
//! skipping names that start with `.`.

use std::collections::BTreeSet;

use super::{PackageDefect, PackageWarning, PluginHeaders, VariantDetection};
use crate::allowlist;
use crate::hints::{PluginHint, VariantMarker};

/// The folder WordPress never extracts: macOS resource forks.
const MACOS_METADATA: &str = "__MACOSX/";

/// What package inspection needs to know about one zip entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Index in the archive.
    pub(crate) index: usize,
    /// The name, as stored (`/`-separated; a trailing `/` marks a folder).
    pub(crate) name: String,
    /// Whether it is a folder (its name ends with `/`).
    pub(crate) is_dir: bool,
    /// Whether it is a symbolic link (Unix mode `S_IFLNK`).
    pub(crate) is_symlink: bool,
    /// Uncompressed size declared by the central directory.
    pub(crate) size: u64,
}

impl Entry {
    /// Whether WordPress extracts it: everything but `__MACOSX/…`.
    pub(crate) fn is_extracted(&self) -> bool {
        !self.name.starts_with(MACOS_METADATA)
    }
}

/// Why an entry name is unsafe, or `None` when it is a clean relative path.
///
/// Stricter than `ZipFile::enclosed_name()`, which drops a leading `/` and
/// resolves `a/../b`: a hostile zip is refused, never reinterpreted. `../`
/// anywhere is refused, as WordPress's `validate_file()` would skip such an
/// entry (`a../b` included), so what is inspected is what gets installed.
///
/// # Arguments
///
/// * `name` - The entry name
pub(crate) fn path_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("has an empty name");
    }
    if name.contains('\0') {
        return Some("has a NUL byte in its name");
    }
    if name.contains('\\') {
        return Some("has a backslash in its name; zip paths use '/'");
    }
    if name.starts_with('/') {
        return Some("is an absolute path");
    }
    let path = name.strip_suffix('/').unwrap_or(name);
    if name.contains("../") || path.split('/').any(|part| part == "..") {
        return Some("climbs out of its folder ('..')");
    }
    if path.split('/').any(|part| part.is_empty() || part == ".") {
        return Some("is not canonical (an empty or '.' component)");
    }
    None
}

/// The plugin folder: the only top-level item, a folder whose name is a
/// valid slug.
///
/// # Arguments
///
/// * `entries` - Every entry, paths already checked ([`path_problem`])
///
/// # Errors
///
/// [`PackageDefect::FileAtRoot`], [`PackageDefect::Empty`],
/// [`PackageDefect::SeveralRoots`] or [`PackageDefect::InvalidSlug`].
pub(crate) fn plugin_root(entries: &[Entry]) -> Result<String, PackageDefect> {
    let mut roots = BTreeSet::new();
    for entry in entries.iter().filter(|entry| entry.is_extracted()) {
        let Some((root, _)) = entry.name.split_once('/') else {
            return Err(PackageDefect::FileAtRoot {
                entry: entry.name.clone(),
            });
        };
        roots.insert(root);
    }
    let mut roots = roots.into_iter();
    let (Some(root), None) = (roots.next(), roots.next()) else {
        return Err(several_or_none(entries));
    };
    allowlist::slug(root).map_err(|reason| PackageDefect::InvalidSlug {
        slug: root.to_string(),
        reason,
    })?;
    Ok(root.to_string())
}

/// The error for a zip without exactly one top-level folder.
fn several_or_none(entries: &[Entry]) -> PackageDefect {
    let roots: BTreeSet<&str> = entries
        .iter()
        .filter(|entry| entry.is_extracted())
        .filter_map(|entry| entry.name.split_once('/').map(|(root, _)| root))
        .collect();
    if roots.is_empty() {
        return PackageDefect::Empty;
    }
    PackageDefect::SeveralRoots {
        roots: roots.into_iter().map(str::to_string).collect(),
    }
}

/// Whether WordPress reads `entry` for plugin headers: a file directly in
/// the plugin folder, named `*.php`, not starting with `.`.
///
/// # Arguments
///
/// * `entry` - The entry
/// * `root` - The plugin folder ([`plugin_root`])
pub(crate) fn may_hold_headers(entry: &Entry, root: &str) -> bool {
    let Some(file) = entry
        .name
        .strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return false;
    };
    !entry.is_dir && !file.contains('/') && !file.starts_with('.') && file.ends_with(".php")
}

/// Picks the main file among the PHP files with a `Plugin Name` header,
/// fed one at a time so only the best headers are kept in memory.
#[derive(Debug)]
pub(crate) struct MainFile {
    /// `<root>/<root>.php`: the main file whenever it has a header.
    preferred: String,
    /// The best file so far and its headers.
    best: Option<(String, PluginHeaders)>,
    /// Every file with a header, in the order offered.
    headered: Vec<String>,
}

impl MainFile {
    /// Start choosing for the plugin folder `root`.
    pub(crate) fn new(root: &str) -> Self {
        Self {
            preferred: format!("{root}/{root}.php"),
            best: None,
            headered: Vec::new(),
        }
    }

    /// Consider a file with a `Plugin Name` header.
    ///
    /// # Arguments
    ///
    /// * `name` - The entry name (`<root>/<file>.php`)
    /// * `headers` - Its headers
    pub(crate) fn offer(&mut self, name: &str, headers: PluginHeaders) {
        self.headered.push(name.to_string());
        let better = match &self.best {
            None => true,
            Some((best, _)) if *best == self.preferred => false,
            Some((best, _)) => name == self.preferred || name < best.as_str(),
        };
        if better {
            self.best = Some((name.to_string(), headers));
        }
    }

    /// The main file and its headers, plus a warning when several files
    /// have a header (WordPress lists each as its own plugin).
    ///
    /// # Errors
    ///
    /// [`PackageDefect::NoPluginFile`] when no file has a header.
    pub(crate) fn finish(
        self,
        root: &str,
    ) -> Result<(String, PluginHeaders, Option<PackageWarning>), PackageDefect> {
        let Some((main, headers)) = self.best else {
            return Err(PackageDefect::NoPluginFile {
                slug: root.to_string(),
            });
        };
        let mut others: Vec<String> = self.headered.into_iter().filter(|n| *n != main).collect();
        others.sort();
        let warning = (!others.is_empty()).then(|| PackageWarning::SeveralPluginFiles {
            main: main.clone(),
            others,
        });
        Ok((main, headers, warning))
    }
}

/// The markers to look for: the entry each one names, and its variant's
/// index in `hint.variants`.
///
/// # Arguments
///
/// * `hint` - The hints for the package's slug, if any
/// * `root` - The plugin folder
pub(crate) fn marker_targets<'h>(
    hint: Option<&'h PluginHint>,
    root: &str,
) -> Vec<(usize, String, &'h VariantMarker)> {
    let Some(hint) = hint else {
        return Vec::new();
    };
    hint.variants
        .iter()
        .enumerate()
        .filter_map(|(index, variant)| {
            let marker = variant.marker.as_ref()?;
            Some((index, format!("{root}/{}", marker.path), marker))
        })
        .collect()
}

/// The package's variant, from the markers found in it (plan §6.9).
///
/// - no hint, or a slug without variants: not applicable;
/// - one variant without a marker: that variant (the slug tells it);
/// - otherwise: the variant whose marker matched, when exactly one did.
///   Several matches give a warning; none leaves it undetermined.
///
/// # Arguments
///
/// * `hint` - The hints for the package's slug, if any
/// * `matched` - Indexes (in `hint.variants`) of the variants whose marker
///   was found
pub(crate) fn detect_variant(
    hint: Option<&PluginHint>,
    matched: &BTreeSet<usize>,
) -> (VariantDetection, Option<PackageWarning>) {
    let Some(hint) = hint.filter(|hint| !hint.variants.is_empty()) else {
        return (VariantDetection::NotApplicable, None);
    };
    if let [only] = hint.variants.as_slice()
        && only.marker.is_none()
    {
        return (VariantDetection::Detected(only.name.clone()), None);
    }
    let names: Vec<String> = matched
        .iter()
        .filter_map(|&index| hint.variants.get(index))
        .map(|variant| variant.name.clone())
        .collect();
    match names.as_slice() {
        [] => (VariantDetection::Undetermined, None),
        [name] => (VariantDetection::Detected(name.clone()), None),
        _ => (
            VariantDetection::Undetermined,
            Some(PackageWarning::AmbiguousVariant { variants: names }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hints::VariantHint;

    /// A file entry.
    fn file(index: usize, name: &str) -> Entry {
        Entry {
            index,
            name: name.into(),
            is_dir: name.ends_with('/'),
            is_symlink: false,
            size: 1,
        }
    }

    /// Entries from names, indexed in order.
    fn entries(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| file(index, name))
            .collect()
    }

    /// Headers named `name`.
    fn headers(name: &str) -> PluginHeaders {
        match PluginHeaders::parse(format!("Plugin Name: {name}").as_bytes()) {
            Some(headers) => headers,
            None => panic!("{name:?} must parse"),
        }
    }

    /// A variant with a marker in `License.php`.
    fn marked(name: &str, needle: &str) -> VariantHint {
        VariantHint {
            name: name.into(),
            marker: Some(VariantMarker {
                path: "inc/License.php".into(),
                needle: needle.into(),
            }),
        }
    }

    #[test]
    fn clean_relative_paths_pass() {
        for name in [
            "plugin/",
            "plugin/plugin.php",
            "plugin/a/b/c.txt",
            "plugin/..hidden",
            "plugin/a..b/c",
            "plugin/\u{e9}t\u{e9}.php",
            "plugin/.htaccess",
            "plugin/C:/x",
        ] {
            assert_eq!(path_problem(name), None, "{name:?}");
        }
    }

    #[test]
    fn unsafe_paths_are_refused_with_their_reason() {
        for (name, reason) in [
            ("", "has an empty name"),
            ("plugin/a\0.php", "has a NUL byte in its name"),
            (
                "plugin\\a.php",
                "has a backslash in its name; zip paths use '/'",
            ),
            ("/etc/passwd", "is an absolute path"),
            ("/", "is an absolute path"),
            ("../evil.php", "climbs out of its folder ('..')"),
            ("plugin/../../evil.php", "climbs out of its folder ('..')"),
            ("plugin/..", "climbs out of its folder ('..')"),
            ("plugin/../", "climbs out of its folder ('..')"),
            ("plugin/a../b.php", "climbs out of its folder ('..')"),
            (
                "plugin//a.php",
                "is not canonical (an empty or '.' component)",
            ),
            (
                "./plugin/a.php",
                "is not canonical (an empty or '.' component)",
            ),
            (
                "plugin/./a.php",
                "is not canonical (an empty or '.' component)",
            ),
            ("plugin//", "is not canonical (an empty or '.' component)"),
        ] {
            assert_eq!(path_problem(name), Some(reason), "{name:?}");
        }
    }

    #[test]
    fn one_top_level_folder_is_the_plugin() {
        let list = entries(&["plugin/", "plugin/plugin.php", "plugin/inc/a.php"]);
        assert_eq!(plugin_root(&list).ok().as_deref(), Some("plugin"));
        // Without folder entries too.
        let list = entries(&["plugin/plugin.php"]);
        assert_eq!(plugin_root(&list).ok().as_deref(), Some("plugin"));
    }

    #[test]
    fn macos_metadata_is_ignored_like_wordpress_does() {
        let list = entries(&[
            "__MACOSX/",
            "__MACOSX/._plugin",
            "__MACOSX/plugin/._plugin.php",
            "plugin/plugin.php",
        ]);
        assert_eq!(plugin_root(&list).ok().as_deref(), Some("plugin"));
        // Only the exact folder name is skipped.
        let list = entries(&["__macosx/x", "plugin/plugin.php"]);
        assert!(matches!(
            plugin_root(&list),
            Err(PackageDefect::SeveralRoots { .. })
        ));
    }

    #[test]
    fn a_file_at_the_top_level_is_refused() {
        for name in ["readme.txt", ".DS_Store", "__MACOSX"] {
            let list = entries(&["plugin/plugin.php", name]);
            match plugin_root(&list) {
                Err(PackageDefect::FileAtRoot { entry }) => assert_eq!(entry, name),
                other => panic!("{name:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn several_or_no_top_level_folders_are_refused() {
        let list = entries(&["b/b.php", "a/a.php", "a/x.php", "c/"]);
        match plugin_root(&list) {
            Err(PackageDefect::SeveralRoots { roots }) => assert_eq!(roots, ["a", "b", "c"]),
            other => panic!("{other:?}"),
        }
        for names in [&[][..], &["__MACOSX/x"][..]] {
            assert!(matches!(
                plugin_root(&entries(names)),
                Err(PackageDefect::Empty)
            ));
        }
    }

    #[test]
    fn the_folder_name_must_be_a_slug() {
        for root in ["-plugin", "pl\u{fc}gin", "a b", &"x".repeat(101)] {
            let list = entries(&[&format!("{root}/x.php")]);
            match plugin_root(&list) {
                Err(PackageDefect::InvalidSlug { slug, .. }) => assert_eq!(slug, root),
                other => panic!("{root:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn header_candidates_are_top_level_php_files() {
        let root = "plugin";
        for (name, expected) in [
            ("plugin/plugin.php", true),
            ("plugin/other.php", true),
            ("plugin/.hidden.php", false),
            ("plugin/inc/deep.php", false),
            ("plugin/upper.PHP", false),
            ("plugin/readme.txt", false),
            ("plugin/dir.php/", false),
            ("plugin2/plugin.php", false),
            ("plugin.php", false),
        ] {
            assert_eq!(may_hold_headers(&file(0, name), root), expected, "{name:?}");
        }
    }

    #[test]
    fn the_main_file_is_root_php_when_it_has_a_header() {
        let mut main = MainFile::new("plugin");
        main.offer("plugin/a.php", headers("A"));
        main.offer("plugin/plugin.php", headers("Main"));
        main.offer("plugin/0.php", headers("Zero"));
        let (file, found, warning) = main.finish("plugin").unwrap();
        assert_eq!(file, "plugin/plugin.php");
        assert_eq!(found.name, "Main");
        assert_eq!(
            warning,
            Some(PackageWarning::SeveralPluginFiles {
                main: "plugin/plugin.php".into(),
                others: vec!["plugin/0.php".into(), "plugin/a.php".into()],
            })
        );
    }

    #[test]
    fn otherwise_the_first_headered_file_in_sorted_order() {
        let mut main = MainFile::new("backwpup-pro");
        main.offer("backwpup-pro/zeta.php", headers("Z"));
        main.offer("backwpup-pro/backwpup.php", headers("BackWPup Pro"));
        let (file, found, warning) = main.finish("backwpup-pro").unwrap();
        assert_eq!(file, "backwpup-pro/backwpup.php");
        assert_eq!(found.name, "BackWPup Pro");
        assert!(warning.is_some());

        let mut single = MainFile::new("plugin");
        single.offer("plugin/only.php", headers("Only"));
        let (file, _, warning) = single.finish("plugin").unwrap();
        assert_eq!(file, "plugin/only.php");
        assert_eq!(warning, None);
    }

    #[test]
    fn no_headered_file_is_an_error() {
        match MainFile::new("plugin").finish("plugin") {
            Err(PackageDefect::NoPluginFile { slug }) => assert_eq!(slug, "plugin"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn marker_targets_live_under_the_plugin_folder() {
        let hint = PluginHint {
            slug: "pro".into(),
            variants: vec![marked("de", "https://backwpup.de/"), marked("en", "x")],
        };
        let targets = marker_targets(Some(&hint), "pro");
        let names: Vec<(usize, &str)> = targets
            .iter()
            .map(|(index, name, _)| (*index, name.as_str()))
            .collect();
        assert_eq!(
            names,
            [(0, "pro/inc/License.php"), (1, "pro/inc/License.php")]
        );
        assert!(marker_targets(None, "pro").is_empty());
    }

    #[test]
    fn variants_follow_the_markers() {
        let pro = PluginHint {
            slug: "backwpup-pro".into(),
            variants: vec![
                marked("pro-de", "https://backwpup.de/"),
                marked("pro-en", "https://backwpup.com/"),
            ],
        };
        let set = |items: &[usize]| items.iter().copied().collect::<BTreeSet<usize>>();
        assert_eq!(
            detect_variant(Some(&pro), &set(&[1])),
            (VariantDetection::Detected("pro-en".into()), None)
        );
        assert_eq!(
            detect_variant(Some(&pro), &set(&[])),
            (VariantDetection::Undetermined, None)
        );
        assert_eq!(
            detect_variant(Some(&pro), &set(&[0, 1])),
            (
                VariantDetection::Undetermined,
                Some(PackageWarning::AmbiguousVariant {
                    variants: vec!["pro-de".into(), "pro-en".into()],
                })
            )
        );
    }

    #[test]
    fn a_lone_variant_is_told_by_its_slug_unless_it_has_a_marker() {
        let free = PluginHint {
            slug: "backwpup".into(),
            variants: vec![VariantHint {
                name: "free".into(),
                marker: None,
            }],
        };
        assert_eq!(
            detect_variant(Some(&free), &BTreeSet::new()).0,
            VariantDetection::Detected("free".into())
        );
        let marked_only = PluginHint {
            slug: "imagify".into(),
            variants: vec![marked("only", "Imagify")],
        };
        assert_eq!(
            detect_variant(Some(&marked_only), &BTreeSet::new()).0,
            VariantDetection::Undetermined
        );
        assert_eq!(
            detect_variant(Some(&marked_only), &BTreeSet::from([0])).0,
            VariantDetection::Detected("only".into())
        );
    }

    #[test]
    fn slugs_without_variants_have_none() {
        let plain = PluginHint {
            slug: "wp-rocket".into(),
            variants: Vec::new(),
        };
        assert_eq!(
            detect_variant(Some(&plain), &BTreeSet::new()),
            (VariantDetection::NotApplicable, None)
        );
        assert_eq!(
            detect_variant(None, &BTreeSet::new()),
            (VariantDetection::NotApplicable, None)
        );
    }
}
