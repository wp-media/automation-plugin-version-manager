//! Reading a plugin zip in memory, with hard limits (plan §6.10, §10 T2).
//!
//! Nothing is ever extracted to disk. Before the zip crate parses
//! anything, [`Layout::read`] reads the directory from the bytes (entry
//! count, a single unambiguous directory, no duplicate names). Then two
//! passes:
//!
//! 1. [`Archive::entries`] reads the central directory: the same records
//!    the layout found (no renamed or folder-marked files), safe names, no
//!    encryption, supported compression (Stored or Deflated, what every
//!    WordPress unzip path handles), and the declared uncompressed total.
//! 2. [`Archive::verify`] decompresses one entry completely, feeding its
//!    bytes to a [`Probe`]. It fails when the entry holds more or fewer
//!    bytes than declared or its CRC-32 is wrong, so the declared sizes the
//!    limits were checked on are the sizes actually read, and a corrupt zip
//!    fails here rather than half-way through a remote install.
//!
//! Every byte the zip reader takes from the file counts against a read
//! budget ([`read_budget`]). An honest zip is read about twice (directory,
//! local headers, data); entries that share their data, or a file full of
//! fake end-of-directory records (each of which the reader tries, scanning
//! back through the file), would make it read the same bytes over and over.
//! Those are zip-bomb shapes, refused with [`PackageDefect::ExcessiveReads`].

use std::cell::Cell;
use std::collections::HashSet;
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom};
use std::rc::Rc;

use zip::read::ZipFile;
use zip::result::ZipError;
use zip::{CompressionMethod, ZipArchive};

use super::layout::{Layout, Record, ambiguous};
use super::rules::{self, Entry};
use super::{Limits, PackageDefect};
use crate::problem::{printable, quoted};

/// Size of the buffer entries are decompressed through.
const CHUNK_BYTES: usize = 64 * 1024;

/// How many times its size an inspection may read a zip.
const READ_BUDGET_FACTOR: u64 = 4;

/// Reads allowed on top of [`READ_BUDGET_FACTOR`], for tiny zips.
const READ_BUDGET_SLACK: u64 = 1024 * 1024;

/// The most bytes an inspection may read from a zip of `size` bytes.
pub(crate) fn read_budget(size: u64) -> u64 {
    size.saturating_mul(READ_BUDGET_FACTOR)
        .saturating_add(READ_BUDGET_SLACK)
}

/// A zip held in memory.
pub(crate) struct Archive {
    /// The parsed archive over the file's bytes.
    zip: ZipArchive<Budgeted>,
    /// The bytes the reader may still read (shared with it).
    left: Rc<Cell<u64>>,
    /// The read budget it started with.
    budget: u64,
    /// The limits to enforce.
    limits: Limits,
    /// The directory as the file's bytes declare it.
    layout: Layout,
    /// Reused decompression buffer.
    buffer: Vec<u8>,
}

impl Archive {
    /// Parse the central directory of `bytes`.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The whole zip file
    /// * `limits` - The limits to enforce
    ///
    /// # Errors
    ///
    /// [`PackageDefect::NotZip`] when it is not a readable zip archive,
    /// the [`Layout::read`] defects, and [`PackageDefect::ExcessiveReads`]
    /// when finding its directory reads too much.
    pub(crate) fn open(bytes: Vec<u8>, limits: Limits) -> Result<Self, PackageDefect> {
        let layout = Layout::read(&bytes, limits.entries)?;
        let budget = read_budget(bytes.len() as u64);
        let reader = Budgeted::new(bytes, budget);
        let left = reader.left();
        let zip = ZipArchive::new(reader)
            .map_err(|err| over_budget(&left, budget).unwrap_or_else(|| not_zip(&err)))?;
        Ok(Self {
            zip,
            left,
            budget,
            limits,
            layout,
            buffer: vec![0; CHUNK_BYTES],
        })
    }

    /// Every entry, after the central-directory checks (pass 1).
    ///
    /// # Errors
    ///
    /// [`PackageDefect::TooManyEntries`], [`PackageDefect::Ambiguous`],
    /// [`PackageDefect::DuplicateEntry`], [`PackageDefect::UnsafePath`],
    /// [`PackageDefect::Encrypted`], [`PackageDefect::UnsupportedCompression`],
    /// [`PackageDefect::TooLargeUncompressed`], [`PackageDefect::ExcessiveReads`],
    /// or [`PackageDefect::NotZip`] when an entry's local header cannot be
    /// read.
    pub(crate) fn entries(&mut self) -> Result<Vec<Entry>, PackageDefect> {
        if self.zip.len() > self.limits.entries {
            return Err(PackageDefect::TooManyEntries {
                count: self.zip.len(),
                max: self.limits.entries,
            });
        }
        self.same_directory()?;
        let mut entries = Vec::with_capacity(self.zip.len());
        let mut names = HashSet::with_capacity(self.zip.len());
        let mut total: u64 = 0;
        for index in 0..self.zip.len() {
            let entry = self.entry(index)?;
            // Different stored bytes can decode to the same name (a legacy
            // code page and UTF-8): one name, two files.
            if !names.insert(entry.name.clone()) {
                return Err(PackageDefect::DuplicateEntry { entry: entry.name });
            }
            total = total.saturating_add(entry.size);
            if total > self.limits.uncompressed_bytes {
                return Err(PackageDefect::TooLargeUncompressed {
                    max: self.limits.uncompressed_bytes,
                });
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Whether the zip crate read the directory [`Layout::read`] found: if
    /// it fell back to another end record, readers disagree on the entries.
    fn same_directory(&self) -> Result<(), PackageDefect> {
        let same = self.zip.offset() == 0
            && self.zip.central_directory_start() == self.layout.start
            && self.zip.len() == self.layout.records.len();
        if same {
            return Ok(());
        }
        Err(ambiguous(
            "zip readers find different central directories in it",
        ))
    }

    /// One entry's metadata, checked: the directory record the layout
    /// found, safe name, not encrypted, supported compression. Reads its
    /// local header, no file data.
    fn entry(&mut self, index: usize) -> Result<Entry, PackageDefect> {
        let (left, budget) = (Rc::clone(&self.left), self.budget);
        let file = self
            .zip
            .by_index_raw(index)
            .map_err(|err| over_budget(&left, budget).unwrap_or_else(|| not_zip(&err)))?;
        let name = libzip_name(file.name_raw(), file.name());
        let record = self.layout.records.get(index);
        same_record(record, file.central_header_start(), file.name_raw(), &name)?;
        if let Some(defect) = refusal(&file, &name) {
            return Err(defect);
        }
        Ok(Entry {
            index,
            is_dir: file.is_dir(),
            is_symlink: file.is_symlink(),
            size: file.size(),
            name,
        })
    }

    /// Decompress `entry` completely, feeding its bytes to `probe` (pass 2).
    ///
    /// # Errors
    ///
    /// [`PackageDefect::CorruptEntry`] when the data cannot be decompressed,
    /// is longer or shorter than declared, or fails its CRC-32;
    /// [`PackageDefect::ExcessiveReads`] when the read budget runs out.
    pub(crate) fn verify(&mut self, entry: &Entry, probe: &mut Probe) -> Result<(), PackageDefect> {
        let (left, budget) = (Rc::clone(&self.left), self.budget);
        let result = match self.zip.by_index(entry.index) {
            Ok(file) => drain(file, entry.size, &mut self.buffer, probe),
            Err(err) => Err(err.to_string()),
        };
        result.map_err(|detail| {
            over_budget(&left, budget).unwrap_or_else(|| PackageDefect::CorruptEntry {
                entry: entry.name.clone(),
                detail: printable(&detail),
            })
        })
    }

    /// Bytes read from the zip so far (tests size the budget with it).
    #[cfg(test)]
    pub(crate) fn bytes_read(&self) -> u64 {
        self.budget - self.left.get()
    }
}

/// Why `file` (named `name`) is refused, if it is: an unsafe path,
/// encryption, or a compression method other than Stored and Deflated.
fn refusal(file: &ZipFile<'_, Budgeted>, name: &str) -> Option<PackageDefect> {
    let entry = name.to_string();
    let unsafe_reason = rules::path_problem(name).or_else(|| {
        file.enclosed_name()
            .is_none()
            .then_some("escapes its folder")
    });
    if let Some(reason) = unsafe_reason {
        return Some(PackageDefect::UnsafePath { entry, reason });
    }
    if file.encrypted() {
        return Some(PackageDefect::Encrypted { entry });
    }
    let method = file.compression();
    let supported = method == CompressionMethod::STORE || method == CompressionMethod::DEFLATE;
    (!supported).then(|| PackageDefect::UnsupportedCompression {
        entry,
        method: method.to_string(),
    })
}

/// An entry name as WordPress's libzip reports it (`ZIP_FL_ENC_GUESS`), so
/// it matches the file WordPress writes: stored bytes that are valid UTF-8
/// are read as UTF-8 even without the UTF-8 flag (macOS `zip` and `ditto`
/// write names that way); other names as the zip crate decoded them (CP437
/// without the flag, invalid sequences replaced with it).
///
/// # Arguments
///
/// * `raw` - The stored name
/// * `decoded` - The zip crate's decoding of it
fn libzip_name(raw: &[u8], decoded: &str) -> String {
    std::str::from_utf8(raw).map_or_else(|_| decoded.to_string(), str::to_string)
}

/// Whether the zip crate's entry is the directory record `record` (same
/// position and stored name) and PclZip reads it the same way.
///
/// # Arguments
///
/// * `record` - The record at the entry's index, as [`Layout::read`] found it
/// * `position` - Where the zip crate read the entry's record
/// * `raw_name` - The name the zip crate uses, as bytes
/// * `name` - The same name, decoded, for messages
///
/// # Errors
///
/// [`PackageDefect::Ambiguous`] when a Unicode path extra field renamed the
/// entry (PclZip ignores those), when the file carries the DOS folder
/// attribute (PclZip creates a folder), or when the records differ.
fn same_record(
    record: Option<&Record>,
    position: u64,
    raw_name: &[u8],
    name: &str,
) -> Result<(), PackageDefect> {
    let Some(record) = record.filter(|record| record.position == position) else {
        return Err(ambiguous(
            "zip readers find different central directories in it",
        ));
    };
    if record.name != raw_name {
        return Err(ambiguous(format!(
            "entry {} gets its name from a Unicode path field, which PclZip ignores",
            quoted(name)
        )));
    }
    if record.is_file_marked_as_folder() {
        return Err(ambiguous(format!(
            "entry {} is a file with the folder attribute, which PclZip extracts as a folder",
            quoted(name)
        )));
    }
    Ok(())
}

/// Read all of an entry's data through `buffer` into `probe`, checking it
/// holds exactly `size` bytes.
///
/// # Returns
///
/// What is wrong with the data, if anything.
fn drain(file: impl Read, size: u64, buffer: &mut [u8], probe: &mut Probe) -> Result<(), String> {
    // One byte past the declared size is enough to detect a lie.
    let mut data = file.take(size.saturating_add(1));
    let mut read: u64 = 0;
    loop {
        let count = match data.read(buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.to_string()),
        };
        read = read.saturating_add(count as u64);
        if read > size {
            return Err(format!("holds more data than its declared {size} bytes"));
        }
        probe.feed(&buffer[..count]);
    }
    if read < size {
        return Err(format!(
            "holds {read} bytes, fewer than its declared {size}"
        ));
    }
    Ok(())
}

/// [`PackageDefect::ExcessiveReads`] when the read budget is spent, which
/// is then the cause of whatever error the reader reported.
fn over_budget(left: &Cell<u64>, budget: u64) -> Option<PackageDefect> {
    (left.get() == 0).then_some(PackageDefect::ExcessiveReads { max: budget })
}

/// [`PackageDefect::NotZip`] for an error of the zip reader.
fn not_zip(err: &ZipError) -> PackageDefect {
    PackageDefect::NotZip(printable(&err.to_string()))
}

/// The in-memory zip, readable until the shared budget is spent.
pub(crate) struct Budgeted {
    /// The zip's bytes.
    inner: Cursor<Vec<u8>>,
    /// Bytes that may still be read.
    left: Rc<Cell<u64>>,
}

impl Budgeted {
    /// `bytes`, of which at most `budget` bytes may be read in all.
    pub(crate) fn new(bytes: Vec<u8>, budget: u64) -> Self {
        Self {
            inner: Cursor::new(bytes),
            left: Rc::new(Cell::new(budget)),
        }
    }

    /// The budget left, shared: it keeps counting as the reader is used.
    pub(crate) fn left(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.left)
    }
}

impl Read for Budgeted {
    /// Read at most what the budget has left; fail once it is spent.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.left.get();
        if left == 0 && !buf.is_empty() {
            return Err(io::Error::other("the read budget is spent"));
        }
        let allowed = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        let count = self.inner.read(&mut buf[..allowed])?;
        self.left.set(left.saturating_sub(count as u64));
        Ok(count)
    }
}

impl Seek for Budgeted {
    /// Seeking reads nothing, so it is free.
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

/// What to keep from an entry's bytes while it is verified: its start (for
/// plugin headers) and whether it contains marker needles.
#[derive(Debug, Default)]
pub(crate) struct Probe {
    /// How many leading bytes to keep.
    head_limit: usize,
    /// The leading bytes kept so far.
    head: Vec<u8>,
    /// Needles to look for, with the caller's tag for each.
    needles: Vec<(usize, String)>,
    /// Tags of the needles found.
    found: Vec<usize>,
    /// The last bytes of the previous chunk, so a needle split across two
    /// chunks is still found.
    carry: Vec<u8>,
}

impl Probe {
    /// A probe that keeps the first `head_limit` bytes and looks for each
    /// needle.
    ///
    /// # Arguments
    ///
    /// * `head_limit` - Leading bytes to keep (0 for none)
    /// * `needles` - `(tag, needle)` pairs; needles are ASCII and non-empty
    pub(crate) fn new(head_limit: usize, needles: Vec<(usize, String)>) -> Self {
        Self {
            head_limit,
            needles,
            ..Self::default()
        }
    }

    /// The leading bytes kept.
    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }

    /// The tags of the needles found.
    pub(crate) fn found(&self) -> &[usize] {
        &self.found
    }

    /// Process the next chunk of the entry.
    fn feed(&mut self, chunk: &[u8]) {
        let wanted = self.head_limit.saturating_sub(self.head.len());
        self.head
            .extend_from_slice(&chunk[..wanted.min(chunk.len())]);
        if self.found.len() < self.needles.len() {
            self.search(chunk);
        }
    }

    /// Look for the needles not found yet in the carried bytes + `chunk`.
    ///
    /// The window is decoded lossily so `str::contains` (linear time) can
    /// search it: ASCII bytes always decode to themselves, so an ASCII
    /// needle matches the text exactly where it matches the bytes.
    fn search(&mut self, chunk: &[u8]) {
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(chunk);
        let text = String::from_utf8_lossy(&window);
        for (tag, needle) in &self.needles {
            if !self.found.contains(tag) && text.contains(needle.as_str()) {
                self.found.push(*tag);
            }
        }
        let longest = self.needles.iter().map(|(_, n)| n.len()).max();
        let keep = longest.unwrap_or(0).saturating_sub(1);
        self.carry = window.split_off(window.len().saturating_sub(keep));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `chunks` to a probe for `needles`, and return the tags found.
    fn found(needles: &[&str], chunks: &[&[u8]]) -> Vec<usize> {
        let tagged = needles
            .iter()
            .enumerate()
            .map(|(tag, needle)| (tag, needle.to_string()))
            .collect();
        let mut probe = Probe::new(0, tagged);
        for chunk in chunks {
            probe.feed(chunk);
        }
        let mut tags = probe.found().to_vec();
        tags.sort_unstable();
        tags
    }

    #[test]
    fn needles_are_found_across_chunk_boundaries() {
        let needle = "https://backwpup.de/";
        let (left, right) = needle.split_at(7);
        assert_eq!(found(&[needle], &[left.as_bytes(), right.as_bytes()]), [0]);
        // Split one byte at a time.
        let bytes: Vec<&[u8]> = needle.as_bytes().chunks(1).collect();
        assert_eq!(found(&[needle], &bytes), [0]);
        assert_eq!(
            found(&[needle], &[b"https://backwpup.com/"]),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn invalid_utf8_around_a_needle_does_not_hide_it() {
        let mut data = vec![0xff, 0xe2, 0x82];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[0xc3]);
        assert_eq!(found(&["needle"], &[&data]), [0]);
        // A multibyte character split across chunks.
        assert_eq!(found(&["ab"], &[b"\xc3", b"\xa9ab"]), [0]);
    }

    #[test]
    fn several_needles_are_tracked_separately() {
        assert_eq!(
            found(&["de/", "com/", "absent"], &[b"x de/ y", b" com/"]),
            [0, 1]
        );
    }

    #[test]
    fn the_head_keeps_only_the_leading_bytes() {
        let mut probe = Probe::new(5, Vec::new());
        probe.feed(b"abc");
        probe.feed(b"defgh");
        probe.feed(b"ij");
        assert_eq!(probe.head(), b"abcde");
        let mut none = Probe::new(0, Vec::new());
        none.feed(b"abc");
        assert!(none.head().is_empty());
    }
}
