//! Project registry.

use std::collections::HashMap;

use apvm_sites::PluginHints;

use super::hints;
use crate::build::plugins::{BackWPupBuilder, Builder, ImagifyBuilder, WpRocketBuilder};
use crate::error::{Error, Result};

pub const DEFAULT_BRANCH_NAME: &str = "develop";

/// A known project.
pub struct Project {
    /// Project name (short identifier).
    pub name: String,
    /// Repository URL.
    pub repo_url: String,
    /// Repository owner (GitHub).
    pub owner: String,
    /// Repository name (GitHub).
    pub repo: String,
    /// Default branch name.
    pub default_branch: String,
    /// Whether the repository is private (requires authentication).
    ///
    /// When `true`, build operations will fail early if no GitHub token
    /// is available, providing a clear error message instead of failing
    /// later during clone/fetch with a cryptic git error.
    pub is_private: bool,
    /// Whether this project publishes GitHub Releases with downloadable assets.
    ///
    /// When `true`, the `release:` prefix and automatic release detection
    /// are enabled for this project. When `false`, `release:` returns a
    /// clear error message suggesting git-based alternatives.
    pub has_releases: bool,
    /// Project-specific builder.
    pub builder: Box<dyn Builder>,
}

impl Project {
    /// The WordPress plugin slugs this project's builds install to, in
    /// variant order, each once: `backwpup` and `backwpup-pro` for BackWPup,
    /// one slug for most projects, none for a builder that declares none
    /// (see [`Builder::plugin_slug`]).
    pub fn plugin_slugs(&self) -> Vec<&'static str> {
        hints::declared_slugs(self.builder.as_ref())
    }
}

/// Registry of known projects.
pub struct ProjectRegistry {
    projects: HashMap<String, Project>,
}

impl ProjectRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            projects: HashMap::new(),
        }
    }

    /// Create registry with all known projects.
    pub fn with_known_projects() -> Self {
        let mut registry = Self::new();

        // Register all supported projects
        registry.register(Project {
            name: "backwpup".to_string(),
            repo_url: "https://github.com/wp-media/backwpup-pro.git".to_string(),
            owner: "wp-media".to_string(),
            repo: "backwpup-pro".to_string(),
            default_branch: DEFAULT_BRANCH_NAME.to_string(),
            is_private: true,
            has_releases: true,
            builder: Box::new(BackWPupBuilder),
        });

        registry.register(Project {
            name: "wp-rocket".to_string(),
            repo_url: "https://github.com/wp-media/wp-rocket.git".to_string(),
            owner: "wp-media".to_string(),
            repo: "wp-rocket".to_string(),
            default_branch: DEFAULT_BRANCH_NAME.to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(WpRocketBuilder),
        });

        // Imagify is a public repository whose version lives in the
        // `imagify.php` plugin header (auto-detected, or overridden into source
        // when `--ver` is passed). Its GitHub Releases carry no downloadable
        // assets (distribution goes to WordPress.org SVN), so `has_releases` is
        // false: a specific released version is built from its tag
        // (e.g. `tag:v2.3.0`) rather than downloaded.
        registry.register(Project {
            name: "imagify".to_string(),
            repo_url: "https://github.com/wp-media/imagify-plugin.git".to_string(),
            owner: "wp-media".to_string(),
            repo: "imagify-plugin".to_string(),
            default_branch: DEFAULT_BRANCH_NAME.to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(ImagifyBuilder),
        });

        registry
    }

    /// Register a project.
    pub fn register(&mut self, project: Project) {
        self.projects.insert(project.name.clone(), project);
    }

    /// Get a project by name.
    pub fn get(&self, name: &str) -> Result<&Project> {
        self.projects
            .get(name)
            .ok_or_else(|| Error::ProjectNotFound(name.to_string()))
    }

    /// List all registered projects.
    pub fn list(&self) -> impl Iterator<Item = &Project> {
        self.projects.values()
    }

    /// What the registered projects' builders declare about their WordPress
    /// plugins (slug per variant, variant markers, family traits), as the
    /// [`PluginHints`] apvm-sites needs to manage them on sites (plan §6.9).
    ///
    /// Projects are in name order. A project whose builder declares no slug
    /// (one that keeps the [`Builder`] defaults) is left out.
    ///
    /// # Errors
    ///
    /// - [`Error::Project`] when a builder declares a slug for some of its
    ///   variants but not for the others;
    /// - [`Error::Sites`] when the declarations break a hint rule, such as
    ///   two projects declaring one slug or same-slug variants without
    ///   markers. It lists every problem, located by `projects[i]`: the
    ///   index among the declaring projects, in name order.
    pub fn plugin_hints(&self) -> Result<PluginHints> {
        let mut projects = Vec::new();
        for project in self.by_name() {
            if let Some(declared) = hints::project_hints(&project.name, project.builder.as_ref())? {
                projects.push(declared);
            }
        }
        Ok(PluginHints::new(projects)?)
    }

    /// The project whose builder declares the WordPress plugin `slug` for
    /// one of its variants (or for its only build), such as `backwpup` for
    /// `backwpup-pro`.
    ///
    /// Slugs match exactly (they are case-sensitive directory names). When
    /// several projects declare the slug, which [`plugin_hints`] refuses,
    /// the first by name is returned.
    ///
    /// # Arguments
    ///
    /// * `slug` - A WordPress plugin slug (its directory name)
    ///
    /// # Returns
    ///
    /// The project, or `None` when no registered project installs `slug`
    /// (any plugin apvm does not build, such as `akismet`).
    ///
    /// [`plugin_hints`]: Self::plugin_hints
    pub fn project_for_slug(&self, slug: &str) -> Option<&Project> {
        self.by_name()
            .into_iter()
            .find(|project| project.plugin_slugs().contains(&slug))
    }

    /// Every registered project, in name order (the map's own order is
    /// unspecified).
    fn by_name(&self) -> Vec<&Project> {
        let mut projects: Vec<&Project> = self.projects.values().collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        projects
    }
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::plugins::{BuildArtifact, BuildVariant, Builder, VersionRequirement};
    use crate::build::progress::BuildStep;

    /// Minimal test builder.
    struct TestBuilder;

    impl Builder for TestBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            VersionRequirement::Optional
        }

        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }

        fn build_commands(
            &self,
            _context: &BuildContext,
            _version: &str,
            _variants: &[&str],
        ) -> Vec<BuildStep> {
            vec![]
        }

        fn artifacts(
            &self,
            _context: &BuildContext,
            _version: &str,
            _variants: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }
    }

    #[test]
    fn test_registry_new_is_empty() {
        let registry = ProjectRegistry::new();
        assert_eq!(registry.list().count(), 0);
    }

    #[test]
    fn test_registry_default_is_empty() {
        let registry = ProjectRegistry::default();
        assert_eq!(registry.list().count(), 0);
    }

    #[test]
    fn test_registry_with_known_projects_has_backwpup() {
        let registry = ProjectRegistry::with_known_projects();
        let project = registry.get("backwpup");
        assert!(project.is_ok());
        assert_eq!(project.unwrap().name, "backwpup");
    }

    #[test]
    fn test_registry_with_known_projects_has_imagify() {
        let registry = ProjectRegistry::with_known_projects();
        let project = registry.get("imagify").expect("imagify must be registered");
        assert_eq!(project.name, "imagify");
        assert_eq!(project.repo, "imagify-plugin");
        assert_eq!(project.owner, "wp-media");
        assert_eq!(project.default_branch, DEFAULT_BRANCH_NAME);
        // Public repo whose version is in imagify.php (optional: auto-detected,
        // or overridden into source when --ver is passed), with no downloadable
        // release assets.
        assert!(!project.is_private);
        assert!(!project.has_releases);
        assert!(
            project.builder.version_requirement().is_optional(),
            "imagify version is auto-detected from / overridden into imagify.php"
        );
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = ProjectRegistry::new();
        registry.register(Project {
            name: "test-plugin".to_string(),
            repo_url: "https://github.com/test/repo.git".to_string(),
            owner: "test".to_string(),
            repo: "repo".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        let project = registry.get("test-plugin").unwrap();
        assert_eq!(project.owner, "test");
        assert_eq!(project.repo, "repo");
        assert!(!project.is_private);
    }

    #[test]
    fn test_registry_get_not_found() {
        let registry = ProjectRegistry::new();
        let result = registry.get("nonexistent");
        assert!(result.is_err());

        let err_string = result.err().unwrap().to_string();
        assert!(err_string.contains("nonexistent"));
    }

    #[test]
    fn test_registry_list() {
        let mut registry = ProjectRegistry::new();

        registry.register(Project {
            name: "plugin-a".to_string(),
            repo_url: "https://github.com/test/a.git".to_string(),
            owner: "test".to_string(),
            repo: "a".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        registry.register(Project {
            name: "plugin-b".to_string(),
            repo_url: "https://github.com/test/b.git".to_string(),
            owner: "test".to_string(),
            repo: "b".to_string(),
            default_branch: "develop".to_string(),
            is_private: true,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        let names: Vec<_> = registry.list().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"plugin-a"));
        assert!(names.contains(&"plugin-b"));
    }

    #[test]
    fn test_registry_register_overwrites() {
        let mut registry = ProjectRegistry::new();

        registry.register(Project {
            name: "plugin".to_string(),
            repo_url: "https://github.com/test/old.git".to_string(),
            owner: "test".to_string(),
            repo: "old".to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        registry.register(Project {
            name: "plugin".to_string(),
            repo_url: "https://github.com/test/new.git".to_string(),
            owner: "test".to_string(),
            repo: "new".to_string(),
            default_branch: "develop".to_string(),
            is_private: true,
            has_releases: false,
            builder: Box::new(TestBuilder),
        });

        // Should only have 1 project (overwritten)
        assert_eq!(registry.list().count(), 1);

        let project = registry.get("plugin").unwrap();
        assert_eq!(project.repo, "new");
        assert!(project.is_private);
    }

    // =========================================================================
    // Plugin hints (plan §3.1, §6.9)
    // =========================================================================

    /// The License.php path the BackWPup pro variants are told apart by.
    const LICENSE: &str = "inc/Pro/License/License.php";

    /// One declared plugin: (project, variant, slug, marker as (path, needle)).
    type DeclaredPlugin = (
        &'static str,
        Option<&'static str>,
        &'static str,
        Option<(&'static str, &'static str)>,
    );

    /// Plan §3.1, one row per (project, variant): the slug the build installs
    /// to and the marker (path, needle) that identifies the variant.
    const PLAN_3_1: &[DeclaredPlugin] = &[
        ("backwpup", Some("free"), "backwpup", None),
        (
            "backwpup",
            Some("pro-de"),
            "backwpup-pro",
            Some((LICENSE, "https://backwpup.de/")),
        ),
        (
            "backwpup",
            Some("pro-en"),
            "backwpup-pro",
            Some((LICENSE, "https://backwpup.com/")),
        ),
        ("wp-rocket", None, "wp-rocket", None),
        ("imagify", None, "imagify", None),
    ];

    /// A builder without variants that declares `slug`.
    struct SlugBuilder(&'static str);

    impl Builder for SlugBuilder {
        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }

        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            vec![]
        }

        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }

        fn plugin_slug(&self, _variant: Option<&str>) -> Option<&'static str> {
            Some(self.0)
        }
    }

    /// A builder with variants `a` and `b` that declares a slug for `a` only.
    struct PartialBuilder;

    impl Builder for PartialBuilder {
        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }

        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            vec![]
        }

        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }

        fn variants(&self) -> Vec<BuildVariant> {
            ["a", "b"]
                .into_iter()
                .map(|id| BuildVariant {
                    id,
                    name: id,
                    description: id,
                })
                .collect()
        }

        fn plugin_slug(&self, variant: Option<&str>) -> Option<&'static str> {
            (variant == Some("a")).then_some("partial")
        }
    }

    /// A builder whose declarations are complete but break a hint rule:
    /// variants `a` and `b` share the slug `shared` without markers, and it
    /// claims family traits for that single slug.
    struct MisdeclaredBuilder;

    impl Builder for MisdeclaredBuilder {
        fn setup_commands(&self) -> Vec<BuildStep> {
            vec![]
        }

        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            vec![]
        }

        fn artifacts(
            &self,
            _: &BuildContext,
            _: &str,
            _: &[&str],
        ) -> crate::Result<Vec<BuildArtifact>> {
            Ok(vec![])
        }

        fn variants(&self) -> Vec<BuildVariant> {
            PartialBuilder.variants()
        }

        fn plugin_slug(&self, variant: Option<&str>) -> Option<&'static str> {
            variant.map(|_| "shared")
        }

        fn family_traits(&self) -> Option<apvm_sites::FamilyTraits> {
            Some(apvm_sites::FamilyTraits {
                conflicts_when_active: true,
                shared_data: false,
            })
        }
    }

    /// A project named `name` built by `builder`.
    fn project(name: &str, builder: Box<dyn Builder>) -> Project {
        Project {
            name: name.to_string(),
            repo_url: format!("https://github.com/test/{name}.git"),
            owner: "test".to_string(),
            repo: name.to_string(),
            default_branch: "main".to_string(),
            is_private: false,
            has_releases: false,
            builder,
        }
    }

    #[test]
    fn known_projects_declare_the_plan_3_1_slugs_and_markers() {
        let registry = ProjectRegistry::with_known_projects();
        for &(name, variant, slug, marker) in PLAN_3_1 {
            let builder = registry.get(name).unwrap().builder.as_ref();
            assert_eq!(
                builder.plugin_slug(variant),
                Some(slug),
                "{name} {variant:?}"
            );
            let declared = variant.and_then(|v| builder.variant_marker(v));
            assert_eq!(
                declared
                    .as_ref()
                    .map(|m| (m.path.as_str(), m.needle.as_str())),
                marker,
                "{name} {variant:?}"
            );
        }
        // The table covers every registered (project, variant), no more.
        let mut registered: Vec<(String, Option<&str>)> = Vec::new();
        for project in registry.list() {
            let variants = project.builder.variants();
            if variants.is_empty() {
                registered.push((project.name.clone(), None));
            }
            for variant in variants {
                registered.push((project.name.clone(), Some(variant.id)));
            }
        }
        registered.sort();
        let mut table: Vec<(String, Option<&str>)> = PLAN_3_1
            .iter()
            .map(|&(name, variant, _, _)| (name.to_string(), variant))
            .collect();
        table.sort();
        assert_eq!(registered, table);
    }

    #[test]
    fn known_projects_family_traits_follow_plan_3_1() {
        let registry = ProjectRegistry::with_known_projects();
        let family = |name: &str| registry.get(name).unwrap().builder.family_traits();
        assert_eq!(
            family("backwpup"),
            Some(apvm_sites::FamilyTraits {
                conflicts_when_active: true,
                shared_data: true,
            })
        );
        assert_eq!(family("wp-rocket"), None);
        assert_eq!(family("imagify"), None);
    }

    #[test]
    fn known_projects_hints_are_valid_and_equal_plan_6_9() {
        use apvm_sites::{FamilyTraits, PluginHint, ProjectHints, VariantHint, VariantMarker};
        let variant = |name: &str, needle: Option<&str>| VariantHint {
            name: name.to_string(),
            marker: needle.map(|needle| VariantMarker {
                path: LICENSE.to_string(),
                needle: needle.to_string(),
            }),
        };
        let plugin = |slug: &str, variants: Vec<VariantHint>| PluginHint {
            slug: slug.to_string(),
            variants,
        };
        let single = |name: &str| ProjectHints {
            name: name.to_string(),
            plugins: vec![plugin(name, vec![])],
            family: None,
        };
        let expected = PluginHints::new(vec![
            ProjectHints {
                name: "backwpup".to_string(),
                plugins: vec![
                    plugin("backwpup", vec![variant("free", None)]),
                    plugin(
                        "backwpup-pro",
                        vec![
                            variant("pro-de", Some("https://backwpup.de/")),
                            variant("pro-en", Some("https://backwpup.com/")),
                        ],
                    ),
                ],
                family: Some(FamilyTraits {
                    conflicts_when_active: true,
                    shared_data: true,
                }),
            },
            single("imagify"),
            single("wp-rocket"),
        ])
        .unwrap();
        // `plugin_hints` returns only hints that passed apvm-sites validation.
        let hints = ProjectRegistry::with_known_projects()
            .plugin_hints()
            .unwrap();
        assert_eq!(hints, expected);
    }

    #[test]
    fn project_for_slug_round_trips_every_declared_slug() {
        let registry = ProjectRegistry::with_known_projects();
        let hints = registry.plugin_hints().unwrap();
        let mut seen = 0;
        for project in hints.projects() {
            for slug in project.slugs() {
                let found = registry.project_for_slug(slug).map(|p| p.name.as_str());
                assert_eq!(found, Some(project.name.as_str()), "{slug}");
                seen += 1;
            }
        }
        assert_eq!(seen, 4, "backwpup, backwpup-pro, imagify, wp-rocket");
    }

    #[test]
    fn plugin_slugs_list_each_declared_slug_once() {
        let registry = ProjectRegistry::with_known_projects();
        let slugs = |name: &str| registry.get(name).unwrap().plugin_slugs();
        assert_eq!(slugs("backwpup"), ["backwpup", "backwpup-pro"]);
        assert_eq!(slugs("wp-rocket"), ["wp-rocket"]);
        assert_eq!(slugs("imagify"), ["imagify"]);
        assert!(
            project("custom", Box::new(TestBuilder))
                .plugin_slugs()
                .is_empty()
        );
    }

    #[test]
    fn project_for_slug_ignores_unknown_slugs() {
        let registry = ProjectRegistry::with_known_projects();
        for slug in [
            "akismet",
            "",
            "BackWPup",
            "backwpup-pro-de",
            "pro-en",
            "free",
            "wp-rocket/",
            "imagify-plugin",
        ] {
            assert!(registry.project_for_slug(slug).is_none(), "{slug:?}");
        }
        assert!(
            ProjectRegistry::new()
                .project_for_slug("backwpup")
                .is_none()
        );
    }

    #[test]
    fn builders_keeping_the_defaults_are_left_out_of_the_hints() {
        let mut registry = ProjectRegistry::new();
        registry.register(project("custom", Box::new(TestBuilder)));
        assert_eq!(registry.plugin_hints().unwrap(), PluginHints::default());
        assert!(registry.project_for_slug("custom").is_none());

        let mut registry = ProjectRegistry::with_known_projects();
        registry.register(project("custom", Box::new(TestBuilder)));
        let hints = registry.plugin_hints().unwrap();
        let names: Vec<_> = hints.projects().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["backwpup", "imagify", "wp-rocket"]);
    }

    #[test]
    fn custom_projects_join_the_hints_in_name_order() {
        let mut registry = ProjectRegistry::with_known_projects();
        registry.register(project("akismet", Box::new(SlugBuilder("akismet"))));
        let hints = registry.plugin_hints().unwrap();
        let names: Vec<_> = hints.projects().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["akismet", "backwpup", "imagify", "wp-rocket"]);
        let found = registry
            .project_for_slug("akismet")
            .map(|p| p.name.as_str());
        assert_eq!(found, Some("akismet"));
    }

    #[test]
    fn a_slug_declared_by_two_projects_is_refused() {
        let mut registry = ProjectRegistry::with_known_projects();
        registry.register(project("fork", Box::new(SlugBuilder("backwpup"))));
        let Err(Error::Sites(err)) = registry.plugin_hints() else {
            panic!("two projects declaring one slug must be refused");
        };
        let problems: Vec<String> = err.problems().iter().map(ToString::to_string).collect();
        assert_eq!(
            problems,
            ["projects[1].plugins[0].slug: duplicate slug 'backwpup' \
              (first at projects[0].plugins[0])"]
        );
        // Lookups stay deterministic: the first project by name wins.
        let found = registry
            .project_for_slug("backwpup")
            .map(|p| p.name.as_str());
        assert_eq!(found, Some("backwpup"));
    }

    #[test]
    fn a_partial_slug_declaration_is_refused() {
        let mut registry = ProjectRegistry::new();
        registry.register(project("partial", Box::new(PartialBuilder)));
        let result = registry.plugin_hints();
        assert!(matches!(result, Err(Error::Project(_))), "{result:?}");
        let found = registry
            .project_for_slug("partial")
            .map(|p| p.name.as_str());
        assert_eq!(found, Some("partial"));
    }

    #[test]
    fn declarations_breaking_a_hint_rule_are_refused_with_every_problem() {
        let mut registry = ProjectRegistry::with_known_projects();
        registry.register(project("misdeclared", Box::new(MisdeclaredBuilder)));
        let Err(Error::Sites(err)) = registry.plugin_hints() else {
            panic!("markerless same-slug variants and a one-slug family must be refused");
        };
        assert_eq!(err.code(), apvm_sites::Code::InvalidInput);
        let problems: Vec<String> = err.problems().iter().map(ToString::to_string).collect();
        // `misdeclared` is projects[2]: backwpup, imagify, misdeclared, wp-rocket.
        assert_eq!(
            problems,
            [
                "projects[2].family: a family needs at least two slugs",
                "projects[2].plugins[0].variants[0].marker: is required: the slug has \
                 several variants to tell apart",
                "projects[2].plugins[0].variants[1].marker: is required: the slug has \
                 several variants to tell apart",
            ]
        );
        assert!(
            Error::from(err)
                .to_string()
                .starts_with("Sites error: invalid plugin hints: projects[2].family")
        );
    }
}
