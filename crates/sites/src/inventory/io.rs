//! Reading and writing the inventory file (plan §7 "File handling", D15).
//!
//! - [`load`]: a missing file is an empty inventory. Content problems carry
//!   the file's path and their own locations (`sites[1].server`).
//! - [`save`] and [`update`] write under an exclusive lock on
//!   `<file>.lock` (`inventory.json.lock`). The new document goes to a
//!   private temp file next to the inventory (`0600` on Unix from its
//!   creation), is synced to disk, then renamed over the inventory, and the
//!   directory is synced. A crash or a failed write leaves the previous
//!   file whole: never a partial one.
//! - [`update`] is the read-modify-write edit: load, change and save all
//!   happen under the lock, so concurrent edits (threads or processes) all
//!   land.
//! - **Symbolic links are refused**: the inventory file and its lock file
//!   must be regular files (or missing), so apvm never reads, locks or
//!   replaces a file elsewhere. Only the last component is checked:
//!   directories above it may be links (macOS's `/tmp` is one).
//! - [`env_override`]: the `APVM_INVENTORY` override, for front ends.
//!
//! # Locking
//!
//! The lock is `std::fs::File::lock` (`flock` on Unix, `LockFileEx` on
//! Windows): advisory, and only as reliable as the filesystem on network
//! mounts. Readers take no lock: a save replaces the file in one rename, so
//! a reader sees the old or the new document, never a mix. The lock file is
//! left in place: removing it while another process waits on it would let
//! two writers in, and Windows cannot remove a locked file.
//!
//! # Example
//!
//! ```
//! use apvm_sites::inventory::io;
//! use apvm_sites::{Inventory, Server};
//!
//! # fn main() -> apvm_sites::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("apvm-sites-io-doc-{}", std::process::id()));
//! # std::fs::remove_dir_all(&dir).ok(); // a leftover of a run with this pid
//! let path = dir.join("inventory.json");
//! assert_eq!(io::load(&path)?, Inventory::new()); // missing = empty
//!
//! io::update(&path, |inventory| inventory.add_server(Server::new("eu-1", "203.0.113.10")))?;
//! assert!(io::load(&path)?.server("eu-1").is_some());
//! # std::fs::remove_dir_all(&dir).ok();
//! # Ok(())
//! # }
//! ```

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::Inventory;
use crate::error::{Error, Result};
use crate::problem::Problem;

/// The environment variable that points front ends at another inventory
/// file (tests, CI, a second set of servers). See [`env_override`].
pub const INVENTORY_ENV: &str = "APVM_INVENTORY";

/// The largest inventory file [`load`] reads. Inventories are a few
/// kilobytes; the cap keeps a wrong path from exhausting memory.
pub const MAX_INVENTORY_BYTES: u64 = 16 * 1024 * 1024;

/// How often opening a file is retried when it is replaced or removed
/// while it is opened (a concurrent save renames over it).
const OPEN_ATTEMPTS: usize = 8;

/// Suffix of the lock file's name: `inventory.json.lock`.
const LOCK_SUFFIX: &str = ".lock";

/// Context of errors reading the inventory.
const READ_CONTEXT: &str = "cannot read the inventory";

/// Context of errors writing the inventory.
const WRITE_CONTEXT: &str = "cannot write the inventory";

/// Context of errors opening or creating the lock file.
const LOCK_OPEN_CONTEXT: &str = "cannot open the inventory lock file";

/// Context of errors taking the lock.
const LOCK_CONTEXT: &str = "cannot lock the inventory with";

/// Context of errors creating the inventory's directory.
const DIR_CONTEXT: &str = "cannot create the inventory directory";

/// Why a file kept changing: it was replaced every time it was opened.
const KEPT_CHANGING: &str = "the file kept changing while it was opened";

/// Makes temp file names unique within the process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What is at a path that should be a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileKind {
    /// A symbolic link (on Windows, also a junction), never followed.
    Symlink,
    /// A directory.
    Directory,
    /// Anything else: a socket, a device, a named pipe, …
    Special,
}

impl std::fmt::Display for FileKind {
    /// `a symbolic link`, `a directory` or `a special file`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Symlink => "a symbolic link",
            Self::Directory => "a directory",
            Self::Special => "a special file",
        })
    }
}

/// The inventory path from [`INVENTORY_ENV`] (`APVM_INVENTORY`), when it is
/// set and not empty. Front ends use it in place of their default location
/// (plan §8.1), like apvm-core's `config_io::cache_dir_env_override`.
///
/// An unset **or empty** variable yields `None`: an accidental
/// `APVM_INVENTORY=` must not silently move the inventory to the current
/// directory. The path is returned as given (a relative one is relative to
/// the current directory).
pub fn env_override() -> Option<PathBuf> {
    non_empty_path(std::env::var_os(INVENTORY_ENV))
}

/// Pure core of [`env_override`]: `value` as a path, unless unset or empty.
fn non_empty_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Load the inventory file. A missing file (or a missing directory above
/// it) is an empty inventory; nothing is created. Takes no lock (see the
/// module docs).
///
/// # Arguments
///
/// * `path` - The inventory file
///
/// # Errors
///
/// - [`Error::InventoryNotAFile`]: the path is a symbolic link (even a
///   dangling one), a directory or a special file.
/// - [`Error::InventoryIo`]: it cannot be read, or the path names no file
///   (empty, `/`, `..`, or ending with a separator or `/.`).
/// - [`Error::InventoryFile`]: its content is refused, wrapping the error of
///   [`Inventory::from_json`]; content that is not UTF-8 or is larger than
///   [`MAX_INVENTORY_BYTES`] is [`Error::InventoryInvalid`] there.
pub fn load(path: &Path) -> Result<Inventory> {
    // Checked here too, so `load` never reads as empty a path `save` refuses.
    file_name(path)?;
    let mut open = |path: &Path| open_options(false).open(path);
    let Some(file) = open_existing(path, READ_CONTEXT, &mut open)? else {
        return Ok(Inventory::new());
    };
    let bytes = read_capped(file, path)?;
    parse(&bytes).map_err(|source| Error::InventoryFile {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Save the inventory, replacing the file atomically under the lock.
/// Missing directories above it are created (`0700` on Unix).
///
/// Replaces the whole document: to change what is on disk, prefer
/// [`update`], which cannot lose another process's edit.
///
/// # Arguments
///
/// * `path` - The inventory file
/// * `inventory` - What to save
///
/// # Errors
///
/// - [`Error::InventoryReadOnly`]: the inventory has a newer format version
///   (nothing is touched).
/// - [`Error::InventoryFile`] wrapping [`Error::InventoryInvalid`]: the
///   document is larger than [`MAX_INVENTORY_BYTES`], so [`load`] would
///   refuse it (nothing is touched).
/// - [`Error::InventoryNotAFile`]: the path or the lock file is a symbolic
///   link, a directory or a special file.
/// - [`Error::InventoryIo`]: a directory, the lock or the file cannot be
///   created, taken or written, or the path names no file. The previous
///   file is left whole.
pub fn save(path: &Path, inventory: &Inventory) -> Result<()> {
    let text = inventory.to_json()?;
    check_size(text.len(), path)?;
    let _lock = InventoryLock::acquire(path)?;
    write_atomic(path, &text)
}

/// Change the inventory file: load it, apply `change` and save the result,
/// all under the lock, so concurrent updates from threads or processes all
/// land. The file is written only when `change` succeeds and changed
/// something (a missing file stays missing after a no-op).
///
/// Keep `change` quick: other writers wait for it. It must not call
/// [`save`] or [`update`] on the same path, which would wait for the lock
/// it holds.
///
/// # Arguments
///
/// * `path` - The inventory file
/// * `change` - Edits the loaded inventory; its error aborts the update
///
/// # Returns
///
/// What `change` returned.
///
/// # Errors
///
/// `change`'s error (nothing is written); [`Error::InventoryReadOnly`]
/// when the file has a newer format version and `change` changed it, even
/// by replacing the whole inventory; otherwise the errors of [`load`] and
/// [`save`], converted into `E`.
///
/// # Example
///
/// ```no_run
/// use apvm_sites::inventory::io;
/// use apvm_sites::{Server, ServerRemoval};
/// # use std::path::Path;
///
/// # fn main() -> apvm_sites::Result<()> {
/// let path = Path::new("/home/me/.apvm/inventory.json");
/// let removal: ServerRemoval = io::update(path, |inventory| inventory.remove_server("eu-1", true))?;
/// println!("removed {} site(s)", removal.sites.len());
/// # Ok(())
/// # }
/// ```
pub fn update<T, E>(
    path: &Path,
    change: impl FnOnce(&mut Inventory) -> std::result::Result<T, E>,
) -> std::result::Result<T, E>
where
    E: From<Error>,
{
    let _lock = InventoryLock::acquire(path)?;
    let original = load(path)?;
    let mut inventory = original.clone();
    let value = change(&mut inventory)?;
    if inventory != original {
        // The edit methods refuse a read-only inventory, but `change` may
        // replace it whole (`*inventory = …`): that must not overwrite a
        // newer document with an older format.
        original.ensure_writable()?;
        let text = inventory.to_json()?;
        check_size(text.len(), path)?;
        write_atomic(path, &text)?;
    }
    Ok(value)
}

/// Read at most [`MAX_INVENTORY_BYTES`] of the inventory.
///
/// # Errors
///
/// [`Error::InventoryIo`] when reading fails; [`Error::InventoryFile`]
/// wrapping [`Error::InventoryInvalid`] when the file is larger.
fn read_capped(file: File, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_INVENTORY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(READ_CONTEXT, path, source))?;
    check_size(bytes.len(), path)?;
    Ok(bytes)
}

/// Refuse a document of `len` bytes larger than [`MAX_INVENTORY_BYTES`]:
/// [`load`] would not read it, and [`save`] must never write what `load`
/// refuses.
///
/// # Errors
///
/// [`Error::InventoryFile`] wrapping [`Error::InventoryInvalid`].
fn check_size(len: usize, path: &Path) -> Result<()> {
    if u64::try_from(len).map_or(true, |len| len > MAX_INVENTORY_BYTES) {
        let limit = MAX_INVENTORY_BYTES / (1024 * 1024);
        return Err(Error::InventoryFile {
            path: path.to_path_buf(),
            source: Box::new(content_error(format!(
                "the file is larger than {limit} MiB"
            ))),
        });
    }
    Ok(())
}

/// Parse the file's bytes as an inventory document.
///
/// # Errors
///
/// [`Error::InventoryInvalid`] when they are not UTF-8; otherwise the
/// errors of [`Inventory::from_json`].
fn parse(bytes: &[u8]) -> Result<Inventory> {
    let text = std::str::from_utf8(bytes).map_err(|err| {
        content_error(format!(
            "not UTF-8 text (invalid byte at offset {})",
            err.valid_up_to()
        ))
    })?;
    Inventory::from_json(text)
}

/// [`Error::InventoryInvalid`] for a problem with the file as a whole.
fn content_error(message: String) -> Error {
    Error::InventoryInvalid {
        problems: vec![Problem::new("", message)],
    }
}

/// An exclusive lock on the inventory's lock file, released on drop.
#[derive(Debug)]
struct InventoryLock {
    /// The open lock file holding the lock.
    file: File,
}

impl InventoryLock {
    /// Block until the inventory's lock is held, creating its directory and
    /// lock file (`0600` on Unix) when missing.
    ///
    /// # Arguments
    ///
    /// * `path` - The inventory file (not the lock file)
    ///
    /// # Errors
    ///
    /// [`Error::InventoryNotAFile`] when the lock file is a symbolic link, a
    /// directory or a special file; [`Error::InventoryIo`] when the path
    /// names no file, or the directory, the lock file or the lock cannot be
    /// created or taken.
    fn acquire(path: &Path) -> Result<Self> {
        let lock_path = sibling(path, LOCK_SUFFIX)?;
        create_parent(path)?;
        let file = open_lock_file(&lock_path)?;
        file.lock()
            .map_err(|source| io_error(LOCK_CONTEXT, &lock_path, source))?;
        Ok(Self { file })
    }
}

impl Drop for InventoryLock {
    /// Release the lock (closing the file would too; this makes it prompt
    /// and explicit).
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Open the lock file, creating it when missing. An existing file is
/// checked like the inventory; creation never follows a symbolic link
/// (`O_CREAT | O_EXCL`), so a link that appears in between is refused on
/// the next look.
///
/// # Errors
///
/// [`Error::InventoryNotAFile`] or [`Error::InventoryIo`].
fn open_lock_file(lock_path: &Path) -> Result<File> {
    let mut open = |path: &Path| open_options(true).open(path);
    for _ in 0..OPEN_ATTEMPTS {
        if let Some(file) = open_existing(lock_path, LOCK_OPEN_CONTEXT, &mut open)? {
            return Ok(file);
        }
        match create_private(lock_path) {
            Ok(file) => return Ok(file),
            // Created by another writer in between: open theirs.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(io_error(LOCK_OPEN_CONTEXT, lock_path, err)),
        }
    }
    Err(io_error(
        LOCK_OPEN_CONTEXT,
        lock_path,
        io::Error::other(KEPT_CHANGING),
    ))
}

/// Opens a path; tests replace it to change the path while it is opened.
type Opener<'a> = &'a mut dyn FnMut(&Path) -> io::Result<File>;

/// Open an existing regular file without following a symbolic link at
/// `path`, retrying when it is replaced while being opened.
///
/// On Unix the path is inspected (`lstat`), opened, and the opened file
/// must be the inspected one (same device and inode), so a link swapped in
/// between is caught. On Windows the open itself does not follow links.
///
/// # Arguments
///
/// * `path` - The file
/// * `context` - What is being done, for errors
/// * `open` - Opens the path
///
/// # Returns
///
/// `None` when nothing is at `path` (or a directory above it is missing).
///
/// # Errors
///
/// [`Error::InventoryNotAFile`] when `path` is not a regular file;
/// [`Error::InventoryIo`] when it cannot be inspected or opened, or keeps
/// being replaced.
fn open_existing(path: &Path, context: &'static str, open: Opener<'_>) -> Result<Option<File>> {
    for _ in 0..OPEN_ATTEMPTS {
        let Some(before) = regular_file_metadata(path, context)? else {
            return Ok(None);
        };
        if let Some(file) = open_matching(path, &before, context, open)? {
            return Ok(Some(file));
        }
    }
    Err(io_error(context, path, io::Error::other(KEPT_CHANGING)))
}

/// Open `path` and keep the file only when it is the entry `before`
/// describes.
///
/// # Returns
///
/// `None` when the path was removed or replaced since `before` was read.
///
/// # Errors
///
/// [`Error::InventoryIo`] when opening or inspecting fails otherwise.
fn open_matching(
    path: &Path,
    before: &Metadata,
    context: &'static str,
    open: Opener<'_>,
) -> Result<Option<File>> {
    let file = match open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(context, path, err)),
    };
    let opened = file
        .metadata()
        .map_err(|source| io_error(context, path, source))?;
    Ok(same_entry(before, &opened).then_some(file))
}

/// The metadata of the regular file at `path`, without following a link.
///
/// # Returns
///
/// `None` when nothing is there.
///
/// # Errors
///
/// [`Error::InventoryNotAFile`] when something other than a regular file is
/// there; [`Error::InventoryIo`] when it cannot be inspected.
fn regular_file_metadata(path: &Path, context: &'static str) -> Result<Option<Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(context, path, err)),
    };
    match file_kind(&metadata) {
        Some(found) => Err(Error::InventoryNotAFile {
            path: path.to_path_buf(),
            found,
        }),
        None => Ok(Some(metadata)),
    }
}

/// What a non-regular entry is, from its own (not followed) metadata.
///
/// # Returns
///
/// `None` for a regular file.
fn file_kind(metadata: &Metadata) -> Option<FileKind> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        Some(FileKind::Symlink)
    } else if file_type.is_dir() {
        Some(FileKind::Directory)
    } else if file_type.is_file() {
        None
    } else {
        Some(FileKind::Special)
    }
}

/// Write `text` to `path` atomically (see [`replace_file`]).
///
/// # Errors
///
/// As [`replace_file`].
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    replace_file(path, |file| file.write_all(text.as_bytes()))
}

/// Replace `path` with a new file: write a private temp file next to it,
/// sync it, rename it over `path`, then sync the directory. The caller
/// holds the lock.
///
/// # Arguments
///
/// * `path` - The file to replace
/// * `write` - Writes the new content (tests inject failures here)
///
/// # Errors
///
/// [`Error::InventoryNotAFile`] when `path` is not a regular file;
/// [`Error::InventoryIo`] when any step fails. The temp file is removed
/// and `path` is left as it was.
fn replace_file(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> Result<()> {
    regular_file_metadata(path, WRITE_CONTEXT)?;
    let (temp, mut file) = TempPath::create(path)?;
    write(&mut file)
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(WRITE_CONTEXT, path, source))?;
    // Closed before the rename: Windows renames an open file less readily.
    drop(file);
    temp.rename_onto(path)
        .map_err(|source| io_error(WRITE_CONTEXT, path, source))?;
    sync_dir(parent_dir(path));
    Ok(())
}

/// A temp file next to the inventory, removed on drop unless it was renamed
/// into place.
#[derive(Debug)]
struct TempPath {
    /// The temp file.
    path: PathBuf,
    /// Whether it was renamed into place (nothing left to remove).
    renamed: bool,
}

impl TempPath {
    /// Create a new private temp file (`0600` on Unix from its creation)
    /// in `target`'s directory, named `.<name>.<pid>.<n>.tmp`.
    ///
    /// # Arguments
    ///
    /// * `target` - The file it will replace
    ///
    /// # Errors
    ///
    /// [`Error::InventoryIo`] when `target` names no file or no temp file
    /// can be created.
    fn create(target: &Path) -> Result<(Self, File)> {
        let name = file_name(target)?;
        Self::create_named(target, || temp_name(name))
    }

    /// Create a new private temp file in `target`'s directory, trying
    /// names from `next_name` until one is free.
    ///
    /// # Arguments
    ///
    /// * `target` - The file it will replace
    /// * `next_name` - Gives the next file name to try
    ///
    /// # Errors
    ///
    /// [`Error::InventoryIo`] when no temp file can be created.
    fn create_named(
        target: &Path,
        mut next_name: impl FnMut() -> OsString,
    ) -> Result<(Self, File)> {
        let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
        for _ in 0..OPEN_ATTEMPTS {
            let path = target.with_file_name(next_name());
            match create_private(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            path,
                            renamed: false,
                        },
                        file,
                    ));
                }
                // A leftover of a crashed process: try the next name.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = err,
                Err(err) => return Err(io_error(WRITE_CONTEXT, target, err)),
            }
        }
        Err(io_error(WRITE_CONTEXT, target, last))
    }

    /// Rename the temp file over `target`.
    ///
    /// # Errors
    ///
    /// The rename's error; the temp file is then removed.
    fn rename_onto(mut self, target: &Path) -> io::Result<()> {
        fs::rename(&self.path, target)?;
        self.renamed = true;
        Ok(())
    }
}

impl Drop for TempPath {
    /// Remove the temp file unless it was renamed into place.
    fn drop(&mut self) {
        if !self.renamed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A temp file name for `name`, unique within this process:
/// `.<name>.<pid>.<n>.tmp`.
fn temp_name(name: &OsStr) -> OsString {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(format!(".{}.{n}.tmp", std::process::id()));
    temp
}

/// `path` with `suffix` appended to its file name (`inventory.json` →
/// `inventory.json.lock`).
///
/// # Errors
///
/// [`Error::InventoryIo`] when `path` names no file.
fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = file_name(path)?.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// The file name `path` ends with.
///
/// # Errors
///
/// [`Error::InventoryIo`] (invalid input) when it ends with no name (an
/// empty path, `/`, `..`), or the name is not its last text: [`Path::file_name`]
/// skips a trailing separator or `.` (`inventory.json/`,
/// `inventory.json/.`), which name a directory.
fn file_name(path: &Path) -> Result<&OsStr> {
    let ends_with = |name: &OsStr| {
        path.as_os_str()
            .as_encoded_bytes()
            .ends_with(name.as_encoded_bytes())
    };
    path.file_name()
        .filter(|name| ends_with(name))
        .ok_or_else(|| {
            let source = io::Error::new(io::ErrorKind::InvalidInput, "the path names no file");
            io_error("invalid inventory path", path, source)
        })
}

/// The directory holding `path` (`.` for a bare file name).
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Create the directories above `path` that are missing (`0700` on Unix;
/// existing ones are left as they are).
///
/// # Errors
///
/// [`Error::InventoryIo`] when one cannot be created.
fn create_parent(path: &Path) -> Result<()> {
    let dir = parent_dir(path);
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    private_dirs(&mut builder);
    builder
        .create(dir)
        .map_err(|source| io_error(DIR_CONTEXT, dir, source))
}

/// Create a new file, failing if anything (a symbolic link included) is
/// already at `path`; `0600` on Unix from its creation.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = open_options(true);
    options.create_new(true);
    private_file(&mut options);
    options.open(path)
}

/// Options to open an existing file for reading (and writing), never
/// following a link at the path on Windows.
fn open_options(write: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    no_follow(&mut options);
    options
}

/// [`Error::InventoryIo`] with its context and path.
fn io_error(context: &'static str, path: &Path, source: io::Error) -> Error {
    Error::InventoryIo {
        context,
        path: path.to_path_buf(),
        source,
    }
}

/// Unix: new files are private to the user.
#[cfg(unix)]
fn private_file(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

/// Elsewhere, files inherit the directory's access rules.
#[cfg(not(unix))]
fn private_file(_options: &mut OpenOptions) {}

/// Unix: new directories are private to the user.
#[cfg(unix)]
fn private_dirs(builder: &mut DirBuilder) {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
}

/// Elsewhere, directories inherit their parent's access rules.
#[cfg(not(unix))]
fn private_dirs(_builder: &mut DirBuilder) {}

/// Windows: open a link itself, not its target (`FILE_FLAG_OPEN_REPARSE_POINT`),
/// so [`same_entry`] sees what the path holds.
#[cfg(windows)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_FLAG_OPEN_REPARSE_POINT` from the Windows SDK.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

/// Elsewhere opens follow links; [`same_entry`] catches one swapped in.
#[cfg(not(windows))]
fn no_follow(_options: &mut OpenOptions) {}

/// Unix: whether the opened file is the regular file `before` described
/// (same device and inode), so the path was not swapped for a link or
/// another file while it was opened.
#[cfg(unix)]
fn same_entry(before: &Metadata, opened: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    opened.is_file() && before.dev() == opened.dev() && before.ino() == opened.ino()
}

/// Elsewhere: whether the opened entry is a regular file. On Windows the
/// open did not follow a link, so a link swapped in opens as a link and is
/// refused here.
#[cfg(not(unix))]
fn same_entry(_before: &Metadata, opened: &Metadata) -> bool {
    opened.is_file()
}

/// Unix: flush the directory so a rename survives a power loss.
/// Best effort: the new file is already in place, and some filesystems
/// cannot sync a directory, so a failure here must not report the save as
/// failed.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    let _ = File::open(dir).and_then(|handle| handle.sync_all());
}

/// Elsewhere directories cannot be synced; the rename is still atomic.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(test)]
mod tests;
