//! The zip's directory, read straight from its bytes, so that every reader
//! WordPress may use sees the same entries as the inspection (plan §6.10,
//! §10 T2).
//!
//! WordPress 7.1.3 `unzip_file()` opens a zip with `ZipArchive` and
//! `ZipArchive::CHECKCONS`; when libzip refuses it (duplicate names, a local
//! header that differs from its directory record, …) it falls back to
//! PclZip. PclZip reads the zip differently from libzip and the zip crate
//! (verified against libzip 1.11.4 and PclZip in WordPress 7.1.3):
//!
//! - it takes the **first** end-of-central-directory signature in the last
//!   64 KiB, where the others take the last valid one, so a zip can hold a
//!   second, hidden directory that only PclZip sees;
//! - it ignores Unicode path extra fields (`0x7075`), which rename an entry
//!   for libzip and the zip crate;
//! - it treats an entry with the DOS folder attribute (`0x10`) as a folder,
//!   whatever its name.
//!
//! [`Layout::read`] therefore accepts only zips in which those readings
//! agree: exactly one end record, at the end of the file (its comment ends
//! the file), a central directory that ends right where the end records
//! start and holds exactly the records they declare, and no two entries
//! with the same name (libzip refuses those, the zip crate keeps only the
//! last). `Archive` then checks that the zip crate read this same
//! directory, without renaming or folder-marked files. It also caps the
//! entry count before the zip crate allocates anything per entry.

use std::collections::HashSet;

use super::PackageDefect;

/// End of central directory record signature (`PK\5\6`).
const END_SIGNATURE: [u8; 4] = *b"PK\x05\x06";

/// ZIP64 end of central directory locator signature (`PK\6\7`).
const LOCATOR_SIGNATURE: [u8; 4] = *b"PK\x06\x07";

/// ZIP64 end of central directory record signature (`PK\6\6`).
const END64_SIGNATURE: [u8; 4] = *b"PK\x06\x06";

/// Central directory file header signature (`PK\1\2`).
const RECORD_SIGNATURE: [u8; 4] = *b"PK\x01\x02";

/// Fixed size of the end of central directory record.
const END_BYTES: usize = 22;

/// Size of the ZIP64 locator.
const LOCATOR_BYTES: usize = 20;

/// Fixed size of the ZIP64 end record.
const END64_BYTES: usize = 56;

/// Fixed size of a central directory file header.
const RECORD_BYTES: usize = 46;

/// How far from the end the readers look for the end record: libzip's
/// buffer (longest comment + end record + ZIP64 locator), which also covers
/// PclZip's 65 557 bytes.
const TAIL_BYTES: usize = 0xFFFF + END_BYTES + LOCATOR_BYTES;

/// The DOS "directory" bit of a record's external attributes.
const DOS_DIRECTORY: u32 = 0x10;

/// The central directory, as its bytes declare it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Offset of the central directory in the file.
    pub(crate) start: u64,
    /// The directory records, in order.
    pub(crate) records: Vec<Record>,
}

/// One central directory record: what the zip crate's view is checked
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// Offset of the record in the file.
    pub(crate) position: u64,
    /// The name, as stored.
    pub(crate) name: Vec<u8>,
    /// External attributes (DOS bits low, Unix mode high).
    pub(crate) external: u32,
}

impl Record {
    /// Whether a file (its name has no trailing `/`) carries the DOS folder
    /// attribute, which makes PclZip create a folder in its place.
    pub(crate) fn is_file_marked_as_folder(&self) -> bool {
        self.external & DOS_DIRECTORY != 0 && !self.name.ends_with(b"/")
    }
}

/// What the end records say about the central directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Directory {
    /// Number of records.
    count: u64,
    /// Size in bytes.
    size: u64,
    /// Offset in the file.
    offset: u64,
    /// Where it must end: the ZIP64 end record, or the end record.
    end: usize,
}

impl Layout {
    /// Read and check the directory of the zip in `bytes`.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The whole zip file
    /// * `max_entries` - The entry limit, enforced before any record is read
    ///
    /// # Errors
    ///
    /// [`PackageDefect::NotZip`] when there is no readable directory,
    /// [`PackageDefect::TooManyEntries`], [`PackageDefect::Ambiguous`] when
    /// zip readers could disagree on the entries, and
    /// [`PackageDefect::DuplicateEntry`].
    pub(crate) fn read(bytes: &[u8], max_entries: usize) -> Result<Self, PackageDefect> {
        let end = end_record(bytes)?;
        let directory = directory(bytes, end)?;
        let count = usize::try_from(directory.count).unwrap_or(usize::MAX);
        if count > max_entries {
            return Err(PackageDefect::TooManyEntries {
                count,
                max: max_entries,
            });
        }
        let records = records(bytes, &directory, count)?;
        if let Some(name) = first_duplicate(&records) {
            return Err(PackageDefect::DuplicateEntry {
                entry: String::from_utf8_lossy(name).into_owned(),
            });
        }
        Ok(Self {
            start: directory.offset,
            records,
        })
    }
}

/// The offset of the only end record, which must close the file.
fn end_record(bytes: &[u8]) -> Result<usize, PackageDefect> {
    let tail_start = bytes.len().saturating_sub(TAIL_BYTES);
    let found: Vec<usize> = bytes[tail_start..]
        .windows(END_SIGNATURE.len())
        .enumerate()
        .filter(|(_, window)| *window == END_SIGNATURE)
        .map(|(at, _)| tail_start + at)
        .collect();
    let [end] = found.as_slice() else {
        if found.is_empty() {
            return Err(not_zip("it has no end of central directory record"));
        }
        return Err(ambiguous(format!(
            "it has {} end of central directory records",
            found.len()
        )));
    };
    let comment = u16_at(bytes, end + 20)
        .ok_or_else(|| not_zip("its end of central directory record is cut short"))?;
    if end + END_BYTES + usize::from(comment) != bytes.len() {
        return Err(ambiguous(
            "its archive comment does not end the file (bytes are missing or follow it)",
        ));
    }
    Ok(*end)
}

/// The central directory the end record at `end` declares, through the
/// ZIP64 records when a locator precedes it.
fn directory(bytes: &[u8], end: usize) -> Result<Directory, PackageDefect> {
    let field16 = |at: usize| u16_at(bytes, end + at).unwrap_or_default();
    let field32 = |at: usize| u32_at(bytes, end + at).unwrap_or_default();
    if field16(4) != 0 || field16(6) != 0 || field16(8) != field16(10) {
        return Err(not_zip("it spans several disks"));
    }
    let short = Directory {
        count: u64::from(field16(10)),
        size: u64::from(field32(12)),
        offset: u64::from(field32(16)),
        end,
    };
    let has_locator = end
        .checked_sub(LOCATOR_BYTES)
        .and_then(|at| bytes.get(at..at + 4))
        .is_some_and(|signature| signature == LOCATOR_SIGNATURE);
    if !has_locator {
        return Ok(short);
    }
    let long = zip64_directory(bytes, end - LOCATOR_BYTES)?;
    agree(&short, &long)?;
    Ok(long)
}

/// The directory declared by the ZIP64 end record that the locator at
/// `locator` points at.
fn zip64_directory(bytes: &[u8], locator: usize) -> Result<Directory, PackageDefect> {
    let at = zip64_record(bytes, locator)?;
    let field64 = |offset: usize| u64_at(bytes, at + offset).unwrap_or_default();
    let disks = (u32_at(bytes, at + 16), u32_at(bytes, at + 20));
    if disks != (Some(0), Some(0)) || field64(24) != field64(32) {
        return Err(not_zip("it spans several disks"));
    }
    Ok(Directory {
        count: field64(32),
        size: field64(40),
        offset: field64(48),
        end: at,
    })
}

/// The offset of the ZIP64 end record the locator at `locator` points at,
/// checked: one disk, and the record ends right at the locator.
fn zip64_record(bytes: &[u8], locator: usize) -> Result<usize, PackageDefect> {
    let damaged = || not_zip("its ZIP64 end records are damaged");
    let at = u64_at(bytes, locator + 8)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(damaged)?;
    if u32_at(bytes, locator + 4) != Some(0) || u32_at(bytes, locator + 16) != Some(1) {
        return Err(not_zip("it spans several disks"));
    }
    // `at` comes from the file: check it fits before any arithmetic on it.
    let fits = at
        .checked_add(END64_BYTES)
        .is_some_and(|record_end| record_end <= locator);
    if !fits || bytes.get(at..at + 4) != Some(&END64_SIGNATURE[..]) {
        return Err(damaged());
    }
    let declared = u64_at(bytes, at + 4).and_then(|size| size.checked_add(12));
    if declared != u64::try_from(locator - at).ok() {
        return Err(ambiguous(
            "its ZIP64 end record does not end at its locator",
        ));
    }
    Ok(at)
}

/// libzip's rule: each 32-bit field of the end record is either saturated
/// (the ZIP64 value applies) or equal to the ZIP64 value, so PclZip, which
/// reads only the 32-bit record, cannot see another directory.
fn agree(short: &Directory, long: &Directory) -> Result<(), PackageDefect> {
    let consistent = |value: u64, saturated: u64, wide: u64| value == saturated || value == wide;
    if consistent(short.count, 0xFFFF, long.count)
        && consistent(short.size, 0xFFFF_FFFF, long.size)
        && consistent(short.offset, 0xFFFF_FFFF, long.offset)
    {
        return Ok(());
    }
    Err(ambiguous(
        "its end record and its ZIP64 end record declare different directories",
    ))
}

/// The `count` records of `directory`, which must fill it exactly.
fn records(
    bytes: &[u8],
    directory: &Directory,
    count: usize,
) -> Result<Vec<Record>, PackageDefect> {
    // libzip (CHECKCONS) requires offset + size == end record position.
    let start = usize::try_from(directory.offset).ok();
    let end = start
        .zip(usize::try_from(directory.size).ok())
        .and_then(|(start, size)| start.checked_add(size));
    let (Some(mut at), Some(end)) = (start, end.filter(|end| *end == directory.end)) else {
        return Err(ambiguous(
            "its central directory does not end at its end record",
        ));
    };
    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        let (record, next) = record(bytes, at, end)
            .ok_or_else(|| not_zip(&format!("its central directory record {index} is damaged")))?;
        records.push(record);
        at = next;
    }
    if at != end {
        return Err(ambiguous(
            "its central directory holds more than the records its end record declares",
        ));
    }
    Ok(records)
}

/// The record at `at`, which must fit before `end`, and where the next one
/// starts.
fn record(bytes: &[u8], at: usize, end: usize) -> Option<(Record, usize)> {
    if bytes.get(at..at + 4)? != RECORD_SIGNATURE {
        return None;
    }
    let length = |offset: usize| u16_at(bytes, at + offset).map(usize::from);
    let name_start = at + RECORD_BYTES;
    let name_end = name_start + length(28)?;
    let next = name_end + length(30)? + length(32)?;
    if next > end {
        return None;
    }
    let record = Record {
        position: u64::try_from(at).ok()?,
        name: bytes.get(name_start..name_end)?.to_vec(),
        external: u32_at(bytes, at + 38)?,
    };
    Some((record, next))
}

/// The first name stored twice, if any.
fn first_duplicate(records: &[Record]) -> Option<&[u8]> {
    let mut seen = HashSet::with_capacity(records.len());
    records
        .iter()
        .map(|record| record.name.as_slice())
        .find(|name| !seen.insert(*name))
}

/// The little-endian `u16` at `at`.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

/// The little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// The little-endian `u64` at `at`.
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// [`PackageDefect::NotZip`] with `detail`.
fn not_zip(detail: &str) -> PackageDefect {
    PackageDefect::NotZip(detail.to_string())
}

/// [`PackageDefect::Ambiguous`] with `detail`.
pub(crate) fn ambiguous(detail: impl Into<String>) -> PackageDefect {
    PackageDefect::Ambiguous {
        detail: detail.into(),
    }
}
