//! What apvm knows about its projects' plugins (plan §6.9).
//!
//! apvm-sites holds no plugin-specific code. Builders in apvm-core declare,
//! per project, which slug each variant installs to, the marker that tells
//! same-slug variants apart on a site, and the family traits of projects
//! with several slugs (BackWPup's free and pro). The registry turns that
//! into [`PluginHints`] for the site manager, so a future multi-variant
//! project needs only its builder.
//!
//! [`PluginHints::new`] validates everything, so a value of this type is
//! always consistent:
//!
//! - project names are ids and unique; a project lists at least one plugin;
//! - [`FamilyTraits`] need at least two slugs (a family is siblings);
//! - a slug belongs to one project, once;
//! - variant names are unique within a project, and either every slug of a
//!   project has variants or none has;
//! - a slug with several variants has a marker on each, and no two of its
//!   markers overlap (same file, one needle inside the other);
//! - marker paths are canonical relative paths; needles are printable ASCII
//!   (they may hold `'` and `\`, so scripts must quote them).

use crate::allowlist;
use crate::error::{Error, Result};
use crate::problem::{FirstSeen, Problem, Scope};

/// Plugin knowledge for every apvm project, always valid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginHints {
    /// The projects, in the order given.
    projects: Vec<ProjectHints>,
}

/// One apvm project's plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectHints {
    /// The apvm project name, such as `backwpup`.
    pub name: String,
    /// The plugins (slugs) the project installs: one for most projects, two
    /// for BackWPup (`backwpup`, `backwpup-pro`).
    pub plugins: Vec<PluginHint>,
    /// How the project's slugs behave together; only for projects with
    /// several slugs.
    pub family: Option<FamilyTraits>,
}

/// How the sibling plugins of a family behave together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyTraits {
    /// Only one sibling runs per request when several are active (BackWPup's
    /// shared main-file guard), so activating one next to an active sibling
    /// switches by default.
    pub conflicts_when_active: bool,
    /// The siblings' uninstall routines remove data the others use.
    pub shared_data: bool,
}

/// One plugin (slug) and the variants that install to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHint {
    /// The WordPress plugin slug (directory name).
    pub slug: String,
    /// The variants that install to this slug: none for a project without
    /// variants, at least one otherwise.
    pub variants: Vec<VariantHint>,
}

/// A build variant and how to recognize it on a site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantHint {
    /// The variant name, as `apvm build --variants` takes it.
    pub name: String,
    /// What tells this variant apart; required when its slug has several
    /// variants.
    pub marker: Option<VariantMarker>,
}

/// A file inside the plugin that contains a fixed text only this variant
/// has (checked with `grep -F` on sites and inside zips).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantMarker {
    /// Path relative to the plugin directory, such as
    /// `inc/Pro/License/License.php`.
    pub path: String,
    /// The fixed text, such as `https://backwpup.de/`: printable ASCII,
    /// possibly with `'` or `\`, so scripts must quote it.
    pub needle: String,
}

impl PluginHints {
    /// Validate and wrap the projects' hints.
    ///
    /// # Arguments
    ///
    /// * `projects` - Hints per project
    ///
    /// # Errors
    ///
    /// [`Error::InvalidHints`], listing every problem with its location
    /// (such as `projects[0].plugins[1].variants[0].marker`).
    pub fn new(projects: Vec<ProjectHints>) -> Result<Self> {
        let problems = validate(&projects);
        if !problems.is_empty() {
            return Err(Error::InvalidHints { problems });
        }
        Ok(Self { projects })
    }

    /// Every project, in the order given.
    pub fn projects(&self) -> &[ProjectHints] {
        &self.projects
    }

    /// The project with this name.
    pub fn project(&self, name: &str) -> Option<&ProjectHints> {
        self.projects.iter().find(|project| project.name == name)
    }

    /// The project that installs this slug.
    pub fn project_for_slug(&self, slug: &str) -> Option<&ProjectHints> {
        self.projects
            .iter()
            .find(|project| project.plugin(slug).is_some())
    }

    /// The hint for this slug.
    pub fn plugin(&self, slug: &str) -> Option<&PluginHint> {
        self.projects
            .iter()
            .find_map(|project| project.plugin(slug))
    }
}

impl ProjectHints {
    /// The hint for this slug, if the project installs it.
    pub fn plugin(&self, slug: &str) -> Option<&PluginHint> {
        self.plugins.iter().find(|plugin| plugin.slug == slug)
    }

    /// The project's slugs, in order.
    pub fn slugs(&self) -> impl Iterator<Item = &str> {
        self.plugins.iter().map(|plugin| plugin.slug.as_str())
    }
}

/// Every problem in `projects`, in order.
fn validate(projects: &[ProjectHints]) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut root = Scope::root(&mut out);
    let mut names = FirstSeen::default();
    // Slugs are unique across projects.
    let mut slugs = FirstSeen::default();
    for (index, project) in projects.iter().enumerate() {
        let mut scope = root.child(&format!("projects[{index}]"));
        scope.check("name", allowlist::id(&project.name));
        scope.unique(&mut names, "name", "project", &project.name);
        project_rules(project, &mut scope);
        let mut variants = FirstSeen::default();
        for (index, plugin) in project.plugins.iter().enumerate() {
            let mut plugin_scope = scope.child(&format!("plugins[{index}]"));
            plugin_scope.check("slug", allowlist::slug(&plugin.slug));
            plugin_scope.unique(&mut slugs, "slug", "slug", &plugin.slug);
            plugin_rules(plugin, &mut plugin_scope, &mut variants);
        }
    }
    out
}

/// Rules for a project as a whole: at least one plugin, family traits only
/// with several slugs, and variants on every slug or on none (else some
/// variant would have no slug, or some slug no variant).
fn project_rules(project: &ProjectHints, scope: &mut Scope<'_>) {
    if project.plugins.is_empty() {
        scope.problem("plugins", "must list at least one plugin");
    }
    if project.family.is_some() && project.plugins.len() < 2 {
        scope.problem("family", "a family needs at least two slugs");
    }
    let with_variants = project.plugins.iter().filter(|p| !p.variants.is_empty());
    if with_variants.count() == 0 {
        return;
    }
    for (index, plugin) in project.plugins.iter().enumerate() {
        if plugin.variants.is_empty() {
            scope.problem(
                &format!("plugins[{index}].variants"),
                "must list at least one variant: other slugs of this project have variants",
            );
        }
    }
}

/// Rules for one plugin's variants; `variants` records where each variant
/// name of the project was first used.
fn plugin_rules<'h>(plugin: &'h PluginHint, scope: &mut Scope<'_>, variants: &mut FirstSeen<'h>) {
    let several = plugin.variants.len() > 1;
    for (index, variant) in plugin.variants.iter().enumerate() {
        let mut variant_scope = scope.child(&format!("variants[{index}]"));
        variant_scope.check("name", allowlist::id(&variant.name));
        variant_scope.unique(variants, "name", "variant", &variant.name);
        match &variant.marker {
            Some(marker) => {
                variant_scope.check("marker.path", allowlist::marker_path(&marker.path));
                variant_scope.check("marker.needle", allowlist::needle(&marker.needle));
            }
            None if several => variant_scope.problem(
                "marker",
                "is required: the slug has several variants to tell apart",
            ),
            None => {}
        }
    }
    overlapping_markers(plugin, scope);
}

/// Markers of one slug that cannot tell their variants apart: same file,
/// and one needle contains the other (a file with the longer needle always
/// matches both).
fn overlapping_markers(plugin: &PluginHint, scope: &mut Scope<'_>) {
    let markers: Vec<(usize, &VariantMarker)> = plugin
        .variants
        .iter()
        .enumerate()
        .filter_map(|(index, variant)| variant.marker.as_ref().map(|marker| (index, marker)))
        .collect();
    for (position, (index, marker)) in markers.iter().enumerate() {
        let overlaps = |(_, earlier): &&(usize, &VariantMarker)| {
            earlier.path == marker.path
                && (earlier.needle.contains(&marker.needle)
                    || marker.needle.contains(&earlier.needle))
        };
        if let Some((earlier, _)) = markers.iter().take(position).find(overlaps) {
            scope.problem(
                &format!("variants[{index}].marker"),
                format!(
                    "overlaps the marker of variants[{earlier}]: same file, and one needle \
                     contains the other"
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;

    /// A variant with a marker.
    fn marked(name: &str, path: &str, needle: &str) -> VariantHint {
        VariantHint {
            name: name.into(),
            marker: Some(VariantMarker {
                path: path.into(),
                needle: needle.into(),
            }),
        }
    }

    /// A variant without a marker.
    fn plain(name: &str) -> VariantHint {
        VariantHint {
            name: name.into(),
            marker: None,
        }
    }

    /// A plugin hint.
    fn plugin(slug: &str, variants: Vec<VariantHint>) -> PluginHint {
        PluginHint {
            slug: slug.into(),
            variants,
        }
    }

    /// A single-slug project without variants.
    fn single(name: &str) -> ProjectHints {
        ProjectHints {
            name: name.into(),
            plugins: vec![plugin(name, Vec::new())],
            family: None,
        }
    }

    /// BackWPup as plan §3.1 and §6.9 describe it.
    fn backwpup() -> ProjectHints {
        const LICENSE: &str = "inc/Pro/License/License.php";
        ProjectHints {
            name: "backwpup".into(),
            plugins: vec![
                plugin("backwpup", vec![plain("free")]),
                plugin(
                    "backwpup-pro",
                    vec![
                        marked("pro-de", LICENSE, "https://backwpup.de/"),
                        marked("pro-en", LICENSE, "https://backwpup.com/"),
                    ],
                ),
            ],
            family: Some(FamilyTraits {
                conflicts_when_active: true,
                shared_data: true,
            }),
        }
    }

    /// Every apvm project.
    fn all() -> Vec<ProjectHints> {
        vec![backwpup(), single("wp-rocket"), single("imagify")]
    }

    /// The problems `projects` produce, as `location: message`.
    fn problems(projects: Vec<ProjectHints>) -> Vec<String> {
        match PluginHints::new(projects) {
            Ok(_) => Vec::new(),
            Err(err) => {
                assert_eq!(err.code(), Code::InvalidInput);
                err.problems().iter().map(Problem::to_string).collect()
            }
        }
    }

    #[test]
    fn apvm_projects_are_valid_hints() {
        let hints = PluginHints::new(all()).unwrap();
        assert_eq!(hints.projects().len(), 3);
        assert_eq!(PluginHints::default().projects(), []);
        assert!(PluginHints::new(Vec::new()).is_ok());
    }

    #[test]
    fn lookups_follow_slugs_and_names() {
        let hints = PluginHints::new(all()).unwrap();
        assert_eq!(hints.project("imagify").unwrap().name, "imagify");
        assert!(hints.project("nope").is_none());
        assert_eq!(
            hints.project_for_slug("backwpup-pro").unwrap().name,
            "backwpup"
        );
        assert_eq!(
            hints.project_for_slug("wp-rocket").unwrap().name,
            "wp-rocket"
        );
        assert!(hints.project_for_slug("akismet").is_none());
        let pro = hints.plugin("backwpup-pro").unwrap();
        assert_eq!(pro.variants.len(), 2);
        assert!(hints.plugin("BackWPup").is_none(), "slugs match exactly");
        assert_eq!(
            hints
                .project("backwpup")
                .unwrap()
                .slugs()
                .collect::<Vec<_>>(),
            ["backwpup", "backwpup-pro"]
        );
    }

    #[test]
    fn a_family_needs_at_least_two_slugs() {
        let mut project = single("imagify");
        project.family = Some(FamilyTraits {
            conflicts_when_active: false,
            shared_data: true,
        });
        assert_eq!(
            problems(vec![project]),
            ["projects[0].family: a family needs at least two slugs"]
        );
    }

    #[test]
    fn a_project_lists_at_least_one_plugin() {
        let mut project = single("imagify");
        project.plugins.clear();
        assert_eq!(
            problems(vec![project]),
            ["projects[0].plugins: must list at least one plugin"]
        );
    }

    #[test]
    fn a_slug_belongs_to_one_project_once() {
        let mut other = single("other");
        other.plugins = vec![plugin("backwpup", Vec::new())];
        assert_eq!(
            problems(vec![backwpup(), other]),
            ["projects[1].plugins[0].slug: duplicate slug 'backwpup' \
                 (first at projects[0].plugins[0])"]
        );
        let mut twice = single("imagify");
        twice.plugins.push(plugin("imagify", Vec::new()));
        assert_eq!(
            problems(vec![twice]),
            ["projects[0].plugins[1].slug: duplicate slug 'imagify' \
                 (first at projects[0].plugins[0])"]
        );
    }

    #[test]
    fn project_names_are_unique_ids() {
        assert_eq!(
            problems(vec![single("imagify"), {
                let mut copy = single("imagify");
                copy.plugins[0].slug = "imagify-2".into();
                copy
            }]),
            ["projects[1].name: duplicate project 'imagify' (first at projects[0])"]
        );
        let mut project = single("imagify");
        project.name = "Imagify".into();
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].name: contains 'I'; only lowercase letters, digits, '.', '_' \
                 and '-' are allowed"
            ]
        );
    }

    #[test]
    fn variant_names_are_unique_within_a_project() {
        let mut project = backwpup();
        project.plugins[1].variants[1].name = "free".into();
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[1].variants[1].name: duplicate variant 'free' \
                 (first at projects[0].plugins[0].variants[0])"
            ]
        );
        // The same name in two projects is fine.
        let mut other = backwpup();
        other.name = "fork".into();
        other.plugins[0].slug = "fork".into();
        other.plugins[1].slug = "fork-pro".into();
        assert!(problems(vec![backwpup(), other]).is_empty());
        let mut project = single("imagify");
        project.plugins[0].variants = vec![plain("Pro")];
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[0].variants[0].name: contains 'P'; only lowercase \
                 letters, digits, '.', '_' and '-' are allowed"
            ]
        );
    }

    #[test]
    fn variants_are_on_every_slug_or_on_none() {
        // A slug without variants next to one with variants: some build
        // variant could never be resolved to it.
        let mut project = backwpup();
        project.plugins[0].variants.clear();
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[0].variants: must list at least one variant: other \
                 slugs of this project have variants"
            ]
        );
        // Several slugs without any variant (a family declared without
        // builds) are fine.
        let project = ProjectHints {
            name: "pair".into(),
            plugins: vec![plugin("pair", Vec::new()), plugin("pair-pro", Vec::new())],
            family: Some(FamilyTraits {
                conflicts_when_active: true,
                shared_data: true,
            }),
        };
        assert!(problems(vec![project]).is_empty());
    }

    #[test]
    fn same_slug_variants_need_markers() {
        let mut project = backwpup();
        project.plugins[1].variants[0].marker = None;
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[1].variants[0].marker: is required: the slug has \
                 several variants to tell apart"
            ]
        );
        // A lone variant may still carry a marker.
        let mut project = single("imagify");
        project.plugins[0].variants = vec![marked("only", "imagify.php", "Imagify")];
        assert!(problems(vec![project]).is_empty());
    }

    #[test]
    fn markers_are_allowlisted() {
        let mut project = backwpup();
        project.plugins[1].variants[0] = marked("pro-de", "../wp-config.php", "DB_PASSWORD");
        project.plugins[1].variants[1] = marked("pro-en", "License.php", "a\nb");
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[1].variants[0].marker.path: must be canonical: no \
                 empty, '.' or '..' components and no trailing '/'",
                "projects[0].plugins[1].variants[1].marker.needle: contains U+000A; only \
                 printable ASCII is allowed",
            ]
        );
        let mut project = backwpup();
        project.plugins[1].variants[0] = marked("pro-de", "/etc/passwd", "root");
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[1].variants[0].marker.path: must be relative to the \
                 plugin directory (no leading '/')"
            ]
        );
    }

    #[test]
    fn overlapping_markers_are_refused() {
        let mut project = backwpup();
        project.plugins[1].variants[1].marker = Some(VariantMarker {
            path: "inc/Pro/License/License.php".into(),
            needle: "https://backwpup.de/x".into(),
        });
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[1].variants[1].marker: overlaps the marker of \
                 variants[0]: same file, and one needle contains the other"
            ]
        );
        // The other direction: the earlier needle is the longer one.
        let mut project = backwpup();
        project.plugins[1].variants[0].marker = Some(VariantMarker {
            path: "inc/Pro/License/License.php".into(),
            needle: "https://backwpup.com/x".into(),
        });
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[1].variants[1].marker: overlaps the marker of \
                 variants[0]: same file, and one needle contains the other"
            ]
        );
        // Identical markers.
        let mut project = backwpup();
        project.plugins[1].variants[1].marker = project.plugins[1].variants[0].marker.clone();
        assert_eq!(problems(vec![project]).len(), 1);
        // A marker overlapping a non-adjacent earlier one is found too.
        let license = "inc/License.php";
        let project = ProjectHints {
            name: "trio".into(),
            plugins: vec![plugin(
                "trio",
                vec![
                    marked("one", license, "alpha"),
                    marked("two", license, "beta"),
                    marked("three", license, "xalphax"),
                ],
            )],
            family: None,
        };
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[0].variants[2].marker: overlaps the marker of \
                 variants[0]: same file, and one needle contains the other"
            ]
        );
        // The same needle in different files tells variants apart.
        let mut project = backwpup();
        project.plugins[1].variants[1] = marked("pro-en", "en.txt", "https://backwpup.de/");
        assert!(problems(vec![project]).is_empty());
    }

    #[test]
    fn slugs_are_allowlisted() {
        let mut project = single("imagify");
        project.plugins[0].slug = "../imagify".into();
        assert_eq!(
            problems(vec![project]),
            [
                "projects[0].plugins[0].slug: contains '/'; only letters, digits, '.', '_' \
                 and '-' are allowed"
            ]
        );
    }

    #[test]
    fn every_problem_is_reported() {
        let project = ProjectHints {
            name: String::new(),
            plugins: vec![plugin("", vec![plain(""), plain("")])],
            family: Some(FamilyTraits {
                conflicts_when_active: true,
                shared_data: false,
            }),
        };
        let found = problems(vec![project]);
        assert_eq!(
            found
                .iter()
                .map(|p| p.split(':').next().unwrap_or_default())
                .collect::<Vec<_>>(),
            [
                "projects[0].name",
                "projects[0].family",
                "projects[0].plugins[0].slug",
                "projects[0].plugins[0].variants[0].name",
                "projects[0].plugins[0].variants[0].marker",
                "projects[0].plugins[0].variants[1].name",
                "projects[0].plugins[0].variants[1].name",
                "projects[0].plugins[0].variants[1].marker",
            ],
            "{found:#?}"
        );
    }
}
