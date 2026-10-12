//! Local inspection of a WordPress plugin zip before it is uploaded
//! (plan §6.10).
//!
//! [`PluginPackage::inspect`] reads a zip in memory, never extracting it,
//! and returns what an install needs: the slug (its single top-level
//! folder), the main file and its headers, the variant (from
//! [`PluginHints`] markers), the SHA-256 and size of the file, and warnings.
//! A zip that breaks a rule is refused with [`Error::PackageInvalid`] and a
//! [`PackageDefect`] saying which rule.
//!
//! | Rule | Detail |
//! |---|---|
//! | File | a regular file named `*.zip`, at most [`MAX_PACKAGE_BYTES`] |
//! | Limits | at most [`MAX_ENTRIES`] entries; at most [`MAX_UNCOMPRESSED_BYTES`] uncompressed, declared and actually read; reading at most 4 times the file size plus 1 MiB (entries sharing data, fake directory records) |
//! | Directory | one that every reader WordPress may use (libzip, then PclZip) reads the same way: a single end record closing the file, a central directory that ends at it and holds exactly the declared records, no Unicode path field renaming an entry, no file with the DOS folder attribute, no two entries with the same name |
//! | Entries | Stored or Deflated, not encrypted, intact (size and CRC-32 checked by decompressing every entry) |
//! | Paths | relative and canonical: no `..` (nor `../` inside a name), `.` or empty component, no leading `/`, NUL or backslash; symbolic links are warned about |
//! | Structure | exactly one top-level item, a folder whose name is a valid slug; `__MACOSX/` is ignored, as WordPress ignores it; any top-level file (`.DS_Store` too) is refused |
//! | Plugin | a `<slug>/*.php` with a `Plugin Name` header in its first 8 KiB, read as WordPress reads it ([`PluginHeaders`]); the main file is `<slug>/<slug>.php` when it has one, else the first in byte order; several give a warning |
//!
//! Inspection is CPU- and I/O-bound (hashing, decompression): call it from a
//! blocking context (`tokio::task::spawn_blocking` in async code).

mod archive;
mod headers;
mod layout;
mod rules;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub use headers::{HEADER_BYTES, PluginHeaders};

use crate::error::{Error, Result};
use crate::hints::PluginHints;
use crate::problem::quoted;
use archive::{Archive, Probe};
use rules::{Entry, MainFile};

/// Largest zip file accepted: 256 MiB.
pub const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;

/// Most entries (files and folders) a zip may hold.
pub const MAX_ENTRIES: usize = 100_000;

/// Largest total uncompressed size: 1 GiB.
pub const MAX_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

/// How many names a message lists before summarizing the rest.
const LISTED_NAMES: usize = 5;

/// The limits an inspection enforces; tests lower them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Largest zip file, in bytes.
    pub(crate) file_bytes: u64,
    /// Most entries.
    pub(crate) entries: usize,
    /// Largest total uncompressed size, in bytes.
    pub(crate) uncompressed_bytes: u64,
}

impl Limits {
    /// The limits of plan §6.10.
    pub(crate) const PLAN: Self = Self {
        file_bytes: MAX_PACKAGE_BYTES,
        entries: MAX_ENTRIES,
        uncompressed_bytes: MAX_UNCOMPRESSED_BYTES,
    };
}

/// An inspected plugin zip, ready to be staged and installed.
///
/// Only [`PluginPackage::inspect`] makes one, so its values always passed
/// every rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPackage {
    /// The zip file, as given.
    path: PathBuf,
    /// Size of the zip file in bytes.
    size: u64,
    /// SHA-256 of the zip file, lowercase hex.
    sha256: String,
    /// Total uncompressed size of the entries, in bytes.
    uncompressed_size: u64,
    /// The plugin slug: the zip's top-level folder.
    slug: String,
    /// The main file, relative to the plugins directory (`<slug>/<file>.php`).
    main_file: String,
    /// The main file's headers.
    headers: PluginHeaders,
    /// The variant, from the hints' markers.
    variant: VariantDetection,
    /// What is worth telling the user, in a stable order.
    warnings: Vec<PackageWarning>,
}

/// Which variant of its slug a package is (plan §6.9).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariantDetection {
    /// The hints declare no variants for the slug, or do not know it.
    NotApplicable,
    /// The package is this variant (by its marker, or by its slug when the
    /// slug has a single unmarked variant).
    Detected(String),
    /// The slug has variants, but no single marker matched (none did, or
    /// several did, which a warning reports).
    Undetermined,
}

/// Something about a package worth telling the user; it does not stop the
/// install.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PackageWarning {
    /// Entries stored as symbolic links. WordPress extracts each as a
    /// regular file that holds the link's target path.
    Symlinks {
        /// The entry names, in archive order.
        entries: Vec<String>,
    },
    /// Several PHP files have a `Plugin Name` header. WordPress lists each
    /// as a separate plugin; apvm uses `main`.
    SeveralPluginFiles {
        /// The main file chosen.
        main: String,
        /// The other files with a header, in byte order.
        others: Vec<String>,
    },
    /// The markers of several variants match, so the variant is
    /// undetermined.
    AmbiguousVariant {
        /// The variants whose marker matched, in hint order.
        variants: Vec<String>,
    },
}

/// Why a zip was refused: the rule of plan §6.10 it breaks. Entry names in
/// messages are quoted and escaped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackageDefect {
    /// The file cannot be read.
    #[error("cannot read it: {0}")]
    Io(#[source] std::io::Error),
    /// The path is not a regular file (a folder, a device, a pipe…).
    #[error("is not a regular file")]
    NotAFile,
    /// The file name does not end in `.zip`.
    #[error("is not named '*.zip'")]
    NotZipName,
    /// The file is larger than the limit.
    #[error("is larger than {max} bytes")]
    TooLarge {
        /// The limit, in bytes.
        max: u64,
    },
    /// The file is not a zip archive the reader understands (not a zip,
    /// truncated, multi-disk…).
    #[error("is not a readable zip archive: {0}")]
    NotZip(String),
    /// More entries than the limit (counted from the end record, before the
    /// entries are parsed).
    #[error("has {count} entries; at most {max} are allowed")]
    TooManyEntries {
        /// The number of entries.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// Inspecting it needs far more reading than its size: entries that
    /// share their data, or many fake end-of-directory records, make the
    /// same bytes be read again and again (a zip-bomb shape).
    #[error(
        "needs more than {max} bytes of reading to inspect: its entries or directory \
         records overlap or repeat (a zip bomb shape)"
    )]
    ExcessiveReads {
        /// The read budget, in bytes.
        max: u64,
    },
    /// The entries add up to more than the uncompressed limit.
    #[error("unpacks to more than {max} bytes")]
    TooLargeUncompressed {
        /// The limit, in bytes.
        max: u64,
    },
    /// Zip readers could disagree on what the zip holds. WordPress reads it
    /// with libzip, or with PclZip when libzip refuses it, and PclZip finds
    /// the end record and entry names differently (see `layout`).
    #[error(
        "is ambiguous: {detail}; the zip readers WordPress uses could see other files than \
         the ones inspected"
    )]
    Ambiguous {
        /// What is ambiguous.
        detail: String,
    },
    /// Two entries have the same name. libzip refuses such a zip and
    /// WordPress then extracts it with PclZip; the zip crate sees only the
    /// last copy, so the limits and checks would miss the others.
    #[error("has the entry {} more than once", quoted(entry))]
    DuplicateEntry {
        /// The entry name.
        entry: String,
    },
    /// An entry name is not a safe relative path.
    #[error("entry {} {reason}", quoted(entry))]
    UnsafePath {
        /// The entry name.
        entry: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// An entry is encrypted.
    #[error("entry {} is encrypted", quoted(entry))]
    Encrypted {
        /// The entry name.
        entry: String,
    },
    /// An entry uses a compression method other than Stored or Deflated.
    #[error(
        "entry {} uses the {} compression method; only Stored and Deflated are supported",
        quoted(entry),
        quoted(method)
    )]
    UnsupportedCompression {
        /// The entry name.
        entry: String,
        /// The method's name.
        method: String,
    },
    /// An entry's data is damaged or does not match its declared size.
    #[error("entry {} is damaged: {detail}", quoted(entry))]
    CorruptEntry {
        /// The entry name.
        entry: String,
        /// What the reader found.
        detail: String,
    },
    /// No entry outside `__MACOSX/`.
    #[error("is empty: it has no plugin folder")]
    Empty,
    /// A file at the top level, next to the plugin folder. WordPress would
    /// then install everything into a folder named after the uploaded zip.
    #[error(
        "has the file {} at the top level; a plugin zip holds only the plugin folder",
        quoted(entry)
    )]
    FileAtRoot {
        /// The entry name.
        entry: String,
    },
    /// Several top-level folders.
    #[error(
        "has {} top-level folders ({}); a plugin zip holds exactly one",
        roots.len(),
        list(roots)
    )]
    SeveralRoots {
        /// The folder names, in byte order.
        roots: Vec<String>,
    },
    /// The top-level folder's name is not a valid plugin slug.
    #[error(
        "top-level folder {} is not a valid plugin slug: {reason}",
        quoted(slug)
    )]
    InvalidSlug {
        /// The folder name.
        slug: String,
        /// Why the slug is refused.
        reason: String,
    },
    /// No PHP file directly in the plugin folder has a `Plugin Name`
    /// header in its first 8 KiB.
    #[error(
        "has no PHP file directly in {} with a 'Plugin Name' header in its first 8 KiB",
        quoted(slug)
    )]
    NoPluginFile {
        /// The plugin folder.
        slug: String,
    },
}

impl PluginPackage {
    /// Inspect a plugin zip (plan §6.10).
    ///
    /// # Arguments
    ///
    /// * `path` - The zip file
    /// * `hints` - Plugin hints, for variant markers
    ///
    /// # Errors
    ///
    /// [`Error::PackageInvalid`] (code `package_invalid`), naming the file and
    /// the rule it breaks ([`PackageDefect`]).
    pub fn inspect(path: impl AsRef<Path>, hints: &PluginHints) -> Result<Self> {
        Self::inspect_with(path.as_ref(), hints, Limits::PLAN)
    }

    /// [`PluginPackage::inspect`] with explicit limits.
    pub(crate) fn inspect_with(path: &Path, hints: &PluginHints, limits: Limits) -> Result<Self> {
        Self::try_inspect(path, hints, limits).map_err(|defect| Error::PackageInvalid {
            path: path.display().to_string(),
            defect,
        })
    }

    /// The inspection steps, failing with the rule broken.
    fn try_inspect(
        path: &Path,
        hints: &PluginHints,
        limits: Limits,
    ) -> std::result::Result<Self, PackageDefect> {
        let bytes = read_zip_file(path, limits.file_bytes)?;
        Self::inspect_bytes(path, bytes, hints, limits)
    }

    /// Inspect the zip file `path` holds, already read as `bytes`.
    fn inspect_bytes(
        path: &Path,
        bytes: Vec<u8>,
        hints: &PluginHints,
        limits: Limits,
    ) -> std::result::Result<Self, PackageDefect> {
        let size = bytes.len() as u64;
        let sha256 = to_hex(&Sha256::digest(&bytes));
        let mut archive = Archive::open(bytes, limits)?;
        let entries = archive.entries()?;
        let slug = rules::plugin_root(&entries)?;
        let scan = scan(&mut archive, &entries, &slug, hints)?;
        let (main_file, headers, several) = scan.main.finish(&slug)?;
        let hint = hints.plugin(&slug);
        let (variant, ambiguous) = rules::detect_variant(hint, &scan.matched);
        let warnings = symlink_warning(&entries)
            .into_iter()
            .chain(several)
            .chain(ambiguous)
            .collect();
        Ok(Self {
            path: path.to_path_buf(),
            size,
            sha256,
            uncompressed_size: entries.iter().map(|entry| entry.size).sum(),
            slug,
            main_file,
            headers,
            variant,
            warnings,
        })
    }

    /// The zip file, as given to [`PluginPackage::inspect`].
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Size of the zip file in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// SHA-256 of the zip file: 64 lowercase hex digits, the format of
    /// `sha256sum`.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Total uncompressed size of the entries, in bytes (for the remote
    /// space check, plan §6.6).
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// The plugin slug: the zip's top-level folder, which WordPress installs
    /// as `wp-content/plugins/<slug>`.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The main file relative to the plugins directory, as WP-CLI's `file`
    /// field shows it (`backwpup-pro/backwpup.php`). Only the slug part is
    /// allowlisted: the file name may hold any character a zip name can, so
    /// escape it before printing.
    pub fn main_file(&self) -> &str {
        &self.main_file
    }

    /// The main file's headers.
    pub fn headers(&self) -> &PluginHeaders {
        &self.headers
    }

    /// The variant, from the hints' markers.
    pub fn variant(&self) -> &VariantDetection {
        &self.variant
    }

    /// Warnings, in a stable order: symbolic links, several plugin files,
    /// ambiguous variant.
    pub fn warnings(&self) -> &[PackageWarning] {
        &self.warnings
    }
}

/// What pass 2 learned: the main file so far and the variants whose marker
/// matched.
struct Scan {
    /// Chooses the main file.
    main: MainFile,
    /// Indexes (in the slug's hint variants) of the markers found.
    matched: BTreeSet<usize>,
}

/// Decompress every file entry (pass 2), reading plugin headers from the
/// candidates and looking for variant markers. `__MACOSX/` entries are
/// verified too, so a damaged zip is refused as a whole.
fn scan(
    archive: &mut Archive,
    entries: &[Entry],
    slug: &str,
    hints: &PluginHints,
) -> std::result::Result<Scan, PackageDefect> {
    let targets = rules::marker_targets(hints.plugin(slug), slug);
    let mut found = Scan {
        main: MainFile::new(slug),
        matched: BTreeSet::new(),
    };
    for entry in entries.iter().filter(|entry| !entry.is_dir) {
        let candidate = rules::may_hold_headers(entry, slug);
        let needles = targets
            .iter()
            .filter(|(_, name, _)| *name == entry.name)
            .map(|(index, _, marker)| (*index, marker.needle.clone()))
            .collect();
        let mut probe = Probe::new(if candidate { HEADER_BYTES } else { 0 }, needles);
        archive.verify(entry, &mut probe)?;
        found.matched.extend(probe.found());
        if let Some(headers) = candidate
            .then(|| PluginHeaders::parse(probe.head()))
            .flatten()
        {
            found.main.offer(&entry.name, headers);
        }
    }
    Ok(found)
}

/// The warning for symbolic-link entries, if there are any.
fn symlink_warning(entries: &[Entry]) -> Option<PackageWarning> {
    let links: Vec<String> = entries
        .iter()
        .filter(|entry| entry.is_symlink)
        .map(|entry| entry.name.clone())
        .collect();
    (!links.is_empty()).then_some(PackageWarning::Symlinks { entries: links })
}

/// Read the whole zip file into memory, after checking it is a regular
/// `*.zip` file within the size limit.
///
/// Inspection and hashing then see the same bytes, even if the file changes
/// meanwhile. The limit is enforced on the bytes read, not only on the size
/// the file system reports.
fn read_zip_file(path: &Path, max: u64) -> std::result::Result<Vec<u8>, PackageDefect> {
    let is_zip = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
    if !is_zip {
        return Err(PackageDefect::NotZipName);
    }
    // Checked before opening: opening a FIFO for reading would block.
    if !std::fs::metadata(path)
        .map_err(PackageDefect::Io)?
        .is_file()
    {
        return Err(PackageDefect::NotAFile);
    }
    let file = File::open(path).map_err(PackageDefect::Io)?;
    let metadata = file.metadata().map_err(PackageDefect::Io)?;
    if !metadata.is_file() {
        return Err(PackageDefect::NotAFile);
    }
    if metadata.len() > max {
        return Err(PackageDefect::TooLarge { max });
    }
    // The reported length only sizes the buffer (it is at most `max`).
    let capacity = usize::try_from(metadata.len()).unwrap_or_default();
    read_capped(file, max, capacity)
}

/// Every byte of `reader`, refusing more than `max`.
///
/// # Arguments
///
/// * `reader` - The source
/// * `max` - The most bytes accepted
/// * `capacity` - The expected size, to allocate once
fn read_capped(
    reader: impl Read,
    max: u64,
    capacity: usize,
) -> std::result::Result<Vec<u8>, PackageDefect> {
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(PackageDefect::Io)?;
    if bytes.len() as u64 > max {
        return Err(PackageDefect::TooLarge { max });
    }
    Ok(bytes)
}

/// `bytes` as lowercase hex.
fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .map(|nibble| char::from(DIGITS[usize::from(nibble)]))
        .collect()
}

/// Names for a message: quoted, the first few, then a count of the rest.
fn list(names: &[String]) -> String {
    let shown: Vec<String> = names
        .iter()
        .take(LISTED_NAMES)
        .map(|name| quoted(name))
        .collect();
    let rest = names.len().saturating_sub(LISTED_NAMES);
    if rest == 0 {
        return shown.join(", ");
    }
    format!("{}, and {rest} more", shown.join(", "))
}

impl std::fmt::Display for PackageWarning {
    /// A one-line description; entry names are quoted and escaped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlinks { entries } => write!(
                f,
                "{} symbolic link(s) ({}): WordPress extracts each as a regular file holding \
                 the link's target",
                entries.len(),
                list(entries)
            ),
            Self::SeveralPluginFiles { main, others } => write!(
                f,
                "several PHP files have a 'Plugin Name' header: {} is used as the main file, \
                 and WordPress also lists {} as separate plugin(s)",
                quoted(main),
                list(others)
            ),
            Self::AmbiguousVariant { variants } => write!(
                f,
                "the markers of several variants match ({}), so the variant is undetermined",
                list(variants)
            ),
        }
    }
}

#[cfg(test)]
mod tests;
