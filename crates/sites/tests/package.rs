//! Package inspection through the public API (plan §6.10): a BackWPup Pro
//! zip shaped like the real artifacts of plan §3.1, and a refused zip.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use apvm_sites::{
    Code, Error, FamilyTraits, PackageDefect, PluginHint, PluginHints, PluginPackage, ProjectHints,
    VariantDetection, VariantHint, VariantMarker,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// BackWPup's marker file (plan §3.1).
const LICENSE: &str = "inc/Pro/License/License.php";

/// BackWPup's hints, as apvm-core declares them (plan §6.9).
fn backwpup_hints() -> PluginHints {
    let marked = |name: &str, needle: &str| VariantHint {
        name: name.into(),
        marker: Some(VariantMarker {
            path: LICENSE.into(),
            needle: needle.into(),
        }),
    };
    PluginHints::new(vec![ProjectHints {
        name: "backwpup".into(),
        plugins: vec![
            PluginHint {
                slug: "backwpup".into(),
                variants: vec![VariantHint {
                    name: "free".into(),
                    marker: None,
                }],
            },
            PluginHint {
                slug: "backwpup-pro".into(),
                variants: vec![
                    marked("pro-de", "https://backwpup.de/"),
                    marked("pro-en", "https://backwpup.com/"),
                ],
            },
        ],
        family: Some(FamilyTraits {
            conflicts_when_active: true,
            shared_data: true,
        }),
    }])
    .expect("BackWPup's hints are valid")
}

/// Write a Deflated zip of `files` into `dir`.
fn write_zip(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::DEFLATE);
    for (name, data) in files {
        writer.start_file(*name, options).expect("start entry");
        writer.write_all(data.as_bytes()).expect("write entry");
    }
    let bytes = writer.finish().expect("finish zip").into_inner();
    let path = dir.join("backwpup-pro.zip");
    std::fs::write(&path, bytes).expect("write zip");
    path
}

#[test]
fn a_backwpup_pro_en_build_is_inspected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let main = "<?php\n/**\n * Plugin Name: BackWPup Pro\n * Version: 5.7.6\n \
                * Requires at least: 6.2\n * Requires PHP: 7.4\n * Network: true\n \
                * Text Domain: backwpup\n */\n";
    let license = "<?php\nclass License {\n\tconst WC_API_URL = 'https://backwpup.com/';\n}\n";
    let path = write_zip(
        dir.path(),
        &[
            ("backwpup-pro/backwpup.php", main),
            ("backwpup-pro/uninstall.php", "<?php // cleanup\n"),
            (&format!("backwpup-pro/{LICENSE}"), license),
        ],
    );

    let package = PluginPackage::inspect(&path, &backwpup_hints()).expect("a valid package");
    assert_eq!(package.slug(), "backwpup-pro");
    assert_eq!(package.main_file(), "backwpup-pro/backwpup.php");
    let headers = package.headers();
    assert_eq!(headers.name, "BackWPup Pro");
    assert_eq!(headers.version.as_deref(), Some("5.7.6"));
    assert_eq!(headers.requires_wp.as_deref(), Some("6.2"));
    assert_eq!(headers.requires_php.as_deref(), Some("7.4"));
    assert!(headers.network);
    assert_eq!(headers.text_domain.as_deref(), Some("backwpup"));
    assert!(headers.requires_plugins.is_empty());
    assert_eq!(
        package.variant(),
        &VariantDetection::Detected("pro-en".into())
    );
    assert!(package.warnings().is_empty());
    let size = std::fs::metadata(&path).expect("zip metadata").len();
    assert_eq!(package.size(), size);
    assert_eq!(package.sha256().len(), 64);
    let uncompressed = main.len() + "<?php // cleanup\n".len() + license.len();
    assert_eq!(package.uncompressed_size(), uncompressed as u64);
}

#[test]
fn a_zip_with_two_plugins_is_refused_with_package_invalid() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write_zip(
        dir.path(),
        &[
            ("backwpup/backwpup.php", "<?php /* Plugin Name: BackWPup */"),
            (
                "backwpup-pro/backwpup.php",
                "<?php /* Plugin Name: BackWPup Pro */",
            ),
        ],
    );
    let error = PluginPackage::inspect(&path, &backwpup_hints()).expect_err("two roots");
    assert_eq!(error.code(), Code::PackageInvalid);
    assert_eq!(error.code().as_str(), "package_invalid");
    let Error::PackageInvalid { defect, .. } = &error else {
        panic!("expected PackageInvalid, got {error:?}");
    };
    assert!(matches!(defect, PackageDefect::SeveralRoots { roots } if roots.len() == 2));
}
