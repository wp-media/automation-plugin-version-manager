//! Builder declarations → [`apvm_sites::ProjectHints`] (plan §6.9).
//!
//! Builders declare, per variant, the WordPress plugin slug a build installs
//! to and the marker that tells same-slug variants apart, plus the family
//! traits of a multi-slug project ([`Builder::plugin_slug`],
//! [`Builder::variant_marker`], [`Builder::family_traits`]). This module
//! gathers one project's declarations; [`PluginHints::new`] then validates
//! them all together.
//!
//! [`PluginHints::new`]: apvm_sites::PluginHints::new

use apvm_sites::{PluginHint, ProjectHints, VariantHint};

use crate::build::plugins::Builder;
use crate::error::{Error, Result};

/// One slug declaration: the variant asked about (`None` for a builder
/// without variants) and the slug the builder declares for it.
type Declaration = (Option<&'static str>, Option<&'static str>);

/// The hints `builder` declares for the project `name`.
///
/// A builder without variants declares one slug (`plugin_slug(None)`). A
/// builder with variants declares a slug per variant; variants sharing a
/// slug are grouped under it, in variant order, each with its marker.
///
/// # Arguments
///
/// * `name` - The project name, as registered
/// * `builder` - The project's builder
///
/// # Returns
///
/// The project's hints, or `Ok(None)` when the builder declares no slug at
/// all (it keeps the trait's defaults), so the project is left out.
///
/// # Errors
///
/// [`Error::Project`] when some variants declare a slug and others do not:
/// a variant without a slug could never be found on a site.
pub(crate) fn project_hints(name: &str, builder: &dyn Builder) -> Result<Option<ProjectHints>> {
    let declarations = declarations(builder);
    if declarations.iter().all(|(_, slug)| slug.is_none()) {
        return Ok(None);
    }
    // Only variants can be undeclared here: a builder without variants has
    // a single declaration, and it is set.
    let undeclared: Vec<&str> = declarations
        .iter()
        .filter(|(_, slug)| slug.is_none())
        .filter_map(|(variant, _)| *variant)
        .collect();
    if !undeclared.is_empty() {
        return Err(Error::Project(format!(
            "project '{name}' declares no plugin slug for variant(s) {}, while its other \
             variants declare one",
            undeclared.join(", ")
        )));
    }
    Ok(Some(ProjectHints {
        name: name.to_string(),
        plugins: group_by_slug(&declarations, builder),
        family: builder.family_traits(),
    }))
}

/// Every slug `builder` declares, in variant order, each once.
///
/// # Arguments
///
/// * `builder` - The project's builder
///
/// # Returns
///
/// The declared slugs; empty when the builder declares none.
pub(crate) fn declared_slugs(builder: &dyn Builder) -> Vec<&'static str> {
    let mut slugs = Vec::new();
    for (_, slug) in declarations(builder) {
        if let Some(slug) = slug
            && !slugs.contains(&slug)
        {
            slugs.push(slug);
        }
    }
    slugs
}

/// What `builder` declares for each variant, in variant order, or for
/// `None` when its [`Builder::variants`] is empty. The single place that
/// decides which variant ids are asked, so [`project_hints`] and
/// [`declared_slugs`] always agree.
///
/// # Arguments
///
/// * `builder` - The project's builder
fn declarations(builder: &dyn Builder) -> Vec<Declaration> {
    let variants = builder.variants();
    if variants.is_empty() {
        return vec![(None, builder.plugin_slug(None))];
    }
    variants
        .iter()
        .map(|variant| (Some(variant.id), builder.plugin_slug(Some(variant.id))))
        .collect()
}

/// One plugin hint per distinct declared slug, in first-seen order, each
/// listing the variants (with their markers) that install to it; no
/// variants for a builder without them.
///
/// # Arguments
///
/// * `declarations` - The builder's declarations, in variant order
/// * `builder` - The builder that declares the variants' markers
fn group_by_slug(declarations: &[Declaration], builder: &dyn Builder) -> Vec<PluginHint> {
    let mut plugins: Vec<PluginHint> = Vec::new();
    for &(variant, slug) in declarations {
        let Some(slug) = slug else { continue };
        // None for a builder without variants.
        let variant = variant.map(|id| VariantHint {
            name: id.to_string(),
            marker: builder.variant_marker(id),
        });
        match plugins.iter_mut().find(|plugin| plugin.slug == slug) {
            Some(existing) => existing.variants.extend(variant),
            None => plugins.push(plugin(slug, variant.into_iter().collect())),
        }
    }
    plugins
}

/// A plugin hint for `slug` with these variants.
///
/// # Arguments
///
/// * `slug` - The plugin slug
/// * `variants` - The variants installing to it (empty without variants)
fn plugin(slug: &str, variants: Vec<VariantHint>) -> PluginHint {
    PluginHint {
        slug: slug.to_string(),
        variants,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::plugins::{
        BackWPupBuilder, BuildArtifact, BuildVariant, FamilyTraits, VariantMarker,
    };
    use crate::build::progress::BuildStep;

    /// A configurable builder: `variants` ids, the slug of each (`slugs`,
    /// by position; for a builder without variants, `slugs[0]` is the slug
    /// for `None`), markers by variant id, and family traits.
    #[derive(Default)]
    struct Declaring {
        variants: Vec<&'static str>,
        slugs: Vec<Option<&'static str>>,
        markers: Vec<(&'static str, VariantMarker)>,
        family: Option<FamilyTraits>,
    }

    impl Builder for Declaring {
        fn setup_commands(&self) -> Vec<BuildStep> {
            Vec::new()
        }

        fn build_commands(&self, _: &BuildContext, _: &str, _: &[&str]) -> Vec<BuildStep> {
            Vec::new()
        }

        fn artifacts(&self, _: &BuildContext, _: &str, _: &[&str]) -> Result<Vec<BuildArtifact>> {
            Ok(Vec::new())
        }

        fn variants(&self) -> Vec<BuildVariant> {
            self.variants
                .iter()
                .map(|id| BuildVariant {
                    id,
                    name: id,
                    description: id,
                })
                .collect()
        }

        fn plugin_slug(&self, variant: Option<&str>) -> Option<&'static str> {
            let index = match variant {
                None => 0,
                Some(id) => self.variants.iter().position(|v| *v == id)?,
            };
            self.slugs.get(index).copied().flatten()
        }

        fn variant_marker(&self, variant: &str) -> Option<VariantMarker> {
            self.markers
                .iter()
                .find(|(id, _)| *id == variant)
                .map(|(_, marker)| marker.clone())
        }

        fn family_traits(&self) -> Option<FamilyTraits> {
            self.family
        }
    }

    /// A marker in `inc/License.php`.
    fn marker(needle: &str) -> VariantMarker {
        VariantMarker {
            path: "inc/License.php".into(),
            needle: needle.into(),
        }
    }

    #[test]
    fn a_builder_keeping_the_defaults_declares_nothing() {
        let builder = Declaring::default();
        assert_eq!(project_hints("custom", &builder).unwrap(), None);
        assert!(declared_slugs(&builder).is_empty());
        let builder = Declaring {
            variants: vec!["a", "b"],
            ..Declaring::default()
        };
        assert_eq!(project_hints("custom", &builder).unwrap(), None);
        assert!(declared_slugs(&builder).is_empty());
    }

    #[test]
    fn a_builder_without_variants_declares_one_plugin() {
        let builder = Declaring {
            slugs: vec![Some("custom")],
            ..Declaring::default()
        };
        let hints = project_hints("custom", &builder).unwrap().unwrap();
        assert_eq!(hints.name, "custom");
        assert_eq!(hints.plugins, [plugin("custom", Vec::new())]);
        assert_eq!(hints.family, None);
        assert_eq!(declared_slugs(&builder), ["custom"]);
    }

    #[test]
    fn variants_are_grouped_by_slug_in_variant_order() {
        let builder = Declaring {
            variants: vec!["de", "free", "en"],
            slugs: vec![Some("pair-pro"), Some("pair"), Some("pair-pro")],
            markers: vec![("de", marker("de")), ("en", marker("en"))],
            family: Some(FamilyTraits {
                conflicts_when_active: false,
                shared_data: true,
            }),
        };
        let hints = project_hints("pair", &builder).unwrap().unwrap();
        let variant = |name: &str, marker: Option<VariantMarker>| VariantHint {
            name: name.into(),
            marker,
        };
        assert_eq!(
            hints.plugins,
            [
                plugin(
                    "pair-pro",
                    vec![
                        variant("de", Some(marker("de"))),
                        variant("en", Some(marker("en")))
                    ]
                ),
                plugin("pair", vec![variant("free", None)]),
            ]
        );
        assert_eq!(hints.family, builder.family);
        assert_eq!(declared_slugs(&builder), ["pair-pro", "pair"]);
    }

    #[test]
    fn a_partial_slug_declaration_is_an_error() {
        let builder = Declaring {
            variants: vec!["a", "b", "c"],
            slugs: vec![Some("x"), None, None],
            ..Declaring::default()
        };
        let Err(err) = project_hints("custom", &builder) else {
            panic!("a variant without a slug must be refused");
        };
        assert!(matches!(err, Error::Project(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "Project error: project 'custom' declares no plugin slug for variant(s) b, c, \
             while its other variants declare one"
        );
        // The declared ones are still found by slug.
        assert_eq!(declared_slugs(&builder), ["x"]);
    }

    #[test]
    fn backwpup_groups_its_pro_variants_under_one_slug() {
        let hints = project_hints("backwpup", &BackWPupBuilder)
            .unwrap()
            .unwrap();
        let slugs: Vec<_> = hints.slugs().collect();
        assert_eq!(slugs, ["backwpup", "backwpup-pro"]);
        assert_eq!(declared_slugs(&BackWPupBuilder), slugs);
        let names = |slug: &str| -> Vec<String> {
            hints
                .plugin(slug)
                .map(|p| p.variants.iter().map(|v| v.name.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(names("backwpup"), ["free"]);
        assert_eq!(names("backwpup-pro"), ["pro-de", "pro-en"]);
    }

    #[test]
    fn a_single_variant_is_listed_with_its_optional_marker() {
        // `variants()` is not empty, so the variant is asked by id (not
        // `None`) and named in the hints; a lone variant's marker is kept.
        let builder = Declaring {
            variants: vec!["only"],
            slugs: vec![Some("solo")],
            markers: vec![("only", marker("m"))],
            ..Declaring::default()
        };
        let hints = project_hints("solo", &builder).unwrap().unwrap();
        let only = VariantHint {
            name: "only".into(),
            marker: Some(marker("m")),
        };
        assert_eq!(hints.plugins, [plugin("solo", vec![only])]);
        assert_eq!(declared_slugs(&builder), ["solo"]);
    }
}
