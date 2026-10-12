//! Tests of the inventory file layer: loading, atomic saves, the lock,
//! concurrent updates, refused symbolic links and the env override.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use super::*;
use crate::error::Code;
use crate::inventory::{INVENTORY_VERSION, Server, Site};

/// A fresh temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    /// Create a unique directory under the system temp dir. The name is
    /// short, so a Unix socket path inside it fits the ~104-byte limit.
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("apvm-io-{}-{n}", std::process::id()));
        // A leftover of an earlier run that had the same pid.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    /// The inventory path inside it.
    fn inventory(&self) -> PathBuf {
        self.0.join("inventory.json")
    }

    /// The names in the directory, sorted.
    fn names(&self) -> Vec<String> {
        names_in(&self.0)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Restore access first, so tests that lock down the dir clean up.
        #[cfg(unix)]
        set_mode(&self.0, 0o700);
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// An inventory with one server.
fn one_server(id: &str) -> Inventory {
    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new(id, "203.0.113.10"))
        .unwrap();
    inventory
}

/// A document with a newer format version (read-only here).
fn newer_document() -> String {
    format!(
        r#"{{"version":{},"servers":[{{"id":"eu-1","host":"h"}}]}}"#,
        INVENTORY_VERSION + 1
    )
}

/// Add server `id` through [`update`].
fn add_server(path: &Path, id: &str) -> Result<()> {
    update(path, |inventory| inventory.add_server(Server::new(id, "h")))
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

// ---- env override ----

#[test]
fn the_env_override_needs_a_non_empty_value() {
    assert_eq!(non_empty_path(None), None);
    assert_eq!(non_empty_path(Some(OsString::new())), None);
    assert_eq!(
        non_empty_path(Some(OsString::from("/tmp/inv.json"))),
        Some(PathBuf::from("/tmp/inv.json"))
    );
    // Kept as given: relative paths are the front end's to resolve.
    assert_eq!(
        non_empty_path(Some(OsString::from("inv.json"))),
        Some(PathBuf::from("inv.json"))
    );
    assert_eq!(INVENTORY_ENV, "APVM_INVENTORY");
}

// ---- load ----

#[test]
fn a_missing_file_is_an_empty_inventory_and_nothing_is_created() {
    let dir = TempDir::new();
    assert_eq!(load(&dir.inventory()).unwrap(), Inventory::new());
    // A missing directory above it too.
    let deep = dir.0.join("a").join("b").join("inventory.json");
    assert_eq!(load(&deep).unwrap(), Inventory::new());
    assert_eq!(dir.names(), Vec::<String>::new());
}

#[test]
fn saved_inventories_load_back() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let inventory = one_server("eu-1");
    save(&path, &inventory).unwrap();
    assert_eq!(load(&path).unwrap(), inventory);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        inventory.to_json().unwrap()
    );
    // Only the inventory and its lock file remain: no temp file.
    assert_eq!(dir.names(), ["inventory.json", "inventory.json.lock"]);
}

#[test]
fn content_errors_carry_the_path_and_their_locations() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(
        &path,
        r#"{"version":1,"servers":[{"id":"eu-1","host":"h"}],
            "sites":[{"id":"shop","server":"eu-2","path":"/var/www"}]}"#,
    )
    .unwrap();
    let err = load(&path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(
        err.problems(),
        [Problem::new("sites[0].server", "unknown server 'eu-2'")]
    );
    assert_eq!(
        err.to_string(),
        format!(
            "{}: invalid inventory: sites[0].server: unknown server 'eu-2'",
            path.display()
        )
    );
    let Error::InventoryFile { path: at, source } = &err else {
        panic!("expected InventoryFile, got {err:?}");
    };
    assert_eq!(at, &path);
    assert!(matches!(**source, Error::InventoryInvalid { .. }));
}

#[test]
fn syntax_errors_carry_the_path_line_and_column() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, "{\"version\": 1,\n  \"servers\": [,]}").unwrap();
    let err = load(&path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryInvalid);
    let message = err.to_string();
    assert!(
        message.starts_with(&path.display().to_string()),
        "{message}"
    );
    assert!(message.contains("line 2 column"), "{message}");
}

#[test]
fn an_empty_file_is_invalid_not_empty() {
    // `version` is required: an empty file is a damaged inventory.
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, "").unwrap();
    let err = load(&path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert!(err.to_string().contains("EOF"), "{err}");
}

#[test]
fn a_newer_unreadable_document_keeps_its_code() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let text = format!(r#"{{"version":{},"future":true}}"#, INVENTORY_VERSION + 1);
    fs::write(&path, text).unwrap();
    assert_eq!(load(&path).unwrap_err().code(), Code::InventoryUnsupported);
}

#[test]
fn non_utf8_content_is_invalid_with_its_offset() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, b"{\"version\": 1, \"x\": \"\xff\"}").unwrap();
    let err = load(&path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(
        err.problems(),
        [Problem::new(
            "",
            "not UTF-8 text (invalid byte at offset 21)"
        )]
    );
}

#[test]
fn a_byte_order_mark_in_the_file_is_ignored() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, "\u{feff}{\"version\": 1}").unwrap();
    assert_eq!(load(&path).unwrap(), Inventory::new());
}

#[test]
fn files_larger_than_the_cap_are_refused() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let file = File::create(&path).unwrap();
    file.set_len(MAX_INVENTORY_BYTES + 1).unwrap();
    let err = load(&path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(
        err.problems(),
        [Problem::new("", "the file is larger than 16 MiB")]
    );
}

#[test]
fn a_file_of_exactly_the_cap_is_read() {
    // The cap is inclusive: the content is read (and then fails to parse
    // as JSON, being all zero bytes), not refused for its size.
    let dir = TempDir::new();
    let path = dir.inventory();
    File::create(&path)
        .unwrap()
        .set_len(MAX_INVENTORY_BYTES)
        .unwrap();
    let err = load(&path).unwrap_err();
    assert!(matches!(
        &err,
        Error::InventoryFile { source, .. } if matches!(**source, Error::InventoryJson { .. })
    ));
}

#[test]
fn a_directory_at_the_path_is_refused() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::create_dir(&path).unwrap();
    for err in [
        load(&path).unwrap_err(),
        save(&path, &Inventory::new()).unwrap_err(),
        add_server(&path, "eu-1").unwrap_err(),
    ] {
        assert_eq!(err.code(), Code::InventoryIo);
        assert!(
            matches!(&err, Error::InventoryNotAFile { found: FileKind::Directory, path: at } if at == &path),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "refusing {}: it is a directory, not a regular file",
                path.display()
            )
        );
    }
    assert!(path.is_dir());
}

#[test]
fn a_file_in_place_of_a_directory_is_an_error_not_missing() {
    let dir = TempDir::new();
    let blocker = dir.0.join("blocker");
    fs::write(&blocker, "").unwrap();
    let path = blocker.join("inventory.json");
    // Windows reports this as "path not found", which reads as missing.
    #[cfg(unix)]
    {
        let err = load(&path).unwrap_err();
        assert_eq!(err.code(), Code::InventoryIo);
        assert!(
            matches!(&err, Error::InventoryIo { path: at, .. } if at == &path),
            "{err:?}"
        );
    }
    // Saving cannot create the directory either.
    let err = save(&path, &Inventory::new()).unwrap_err();
    assert!(
        matches!(
            &err,
            Error::InventoryIo {
                context: DIR_CONTEXT,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn paths_naming_no_file_are_refused() {
    for path in [Path::new("/"), Path::new("..")] {
        let err = save(path, &Inventory::new()).unwrap_err();
        assert_eq!(err.code(), Code::InventoryIo);
        assert!(err.to_string().contains("the path names no file"), "{err}");
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_a_located_io_error() {
    let dir = TempDir::new();
    let path = dir.inventory();
    save(&path, &one_server("eu-1")).unwrap();
    set_mode(&path, 0o000);
    if File::open(&path).is_ok() {
        eprintln!("skipped: permissions are not enforced (running as root?)");
        return;
    }
    let err = load(&path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryIo);
    assert!(
        matches!(&err, Error::InventoryIo { context: READ_CONTEXT, path: at, .. } if at == &path),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with(&format!("cannot read the inventory {}: ", path.display())),
        "{err}"
    );
}

// ---- save: permissions and atomicity ----

#[cfg(unix)]
#[test]
fn saved_files_and_created_directories_are_private() {
    let dir = TempDir::new();
    let path = dir.0.join("apvm").join("nested").join("inventory.json");
    save(&path, &one_server("eu-1")).unwrap();
    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(mode_of(&sibling(&path, LOCK_SUFFIX).unwrap()), 0o600);
    assert_eq!(mode_of(&dir.0.join("apvm")), 0o700);
    assert_eq!(mode_of(&dir.0.join("apvm").join("nested")), 0o700);
}

#[cfg(unix)]
#[test]
fn saving_over_a_readable_file_makes_it_private() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, "{\"version\": 1}").unwrap();
    set_mode(&path, 0o644);
    add_server(&path, "eu-1").unwrap();
    assert_eq!(mode_of(&path), 0o600);
}

#[cfg(unix)]
#[test]
fn existing_directories_keep_their_mode() {
    let dir = TempDir::new();
    set_mode(&dir.0, 0o755);
    save(&dir.inventory(), &Inventory::new()).unwrap();
    assert_eq!(mode_of(&dir.0), 0o755);
}

#[test]
fn a_failed_write_leaves_the_previous_file_whole_and_no_temp_file() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let original = one_server("eu-1");
    save(&path, &original).unwrap();
    let before = fs::read(&path).unwrap();

    let err = replace_file(&path, |file| {
        file.write_all(b"{\"version\": 1, \"serv")?;
        Err(io::Error::other("disk full"))
    })
    .unwrap_err();
    assert!(
        matches!(&err, Error::InventoryIo { context: WRITE_CONTEXT, path: at, .. } if at == &path),
        "{err:?}"
    );
    assert!(err.to_string().ends_with(": disk full"), "{err}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(load(&path).unwrap(), original);
    assert_eq!(dir.names(), ["inventory.json", "inventory.json.lock"]);
}

#[test]
fn a_failed_first_write_creates_no_file() {
    let dir = TempDir::new();
    let path = dir.inventory();
    replace_file(&path, |_| Err(io::Error::other("boom"))).unwrap_err();
    assert_eq!(dir.names(), Vec::<String>::new());
}

#[cfg(unix)]
#[test]
fn an_unwritable_directory_leaves_the_file_as_it_was() {
    let dir = TempDir::new();
    let path = dir.inventory();
    save(&path, &one_server("eu-1")).unwrap();
    let before = fs::read(&path).unwrap();
    set_mode(&dir.0, 0o500);
    if File::create(dir.0.join("probe")).is_ok() {
        eprintln!("skipped: permissions are not enforced (running as root?)");
        return;
    }
    let err = add_server(&path, "us-1").unwrap_err();
    set_mode(&dir.0, 0o700);
    assert!(
        matches!(&err, Error::InventoryIo { context: WRITE_CONTEXT, path: at, .. } if at == &path),
        "{err:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(dir.names(), ["inventory.json", "inventory.json.lock"]);
}

#[test]
fn a_read_only_inventory_is_never_written() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, newer_document()).unwrap();
    let inventory = load(&path).unwrap();
    assert!(inventory.is_read_only());

    let err = save(&path, &inventory).unwrap_err();
    assert_eq!(err.code(), Code::InventoryReadOnly);
    let err = add_server(&path, "us-1").unwrap_err();
    assert_eq!(err.code(), Code::InventoryReadOnly);
    // A no-op update reads it without writing.
    let count = update(&path, |inventory| Ok::<_, Error>(inventory.servers().len())).unwrap();
    assert_eq!(count, 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), newer_document());
}

#[test]
fn an_update_never_replaces_a_read_only_inventory_whole() {
    // The edit methods refuse a read-only inventory, but a change may
    // replace it outright: the newer document must survive that too.
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, newer_document()).unwrap();
    let err = update(&path, |inventory| {
        *inventory = one_server("eu-1");
        Ok::<_, Error>(())
    })
    .unwrap_err();
    assert_eq!(err.code(), Code::InventoryReadOnly);
    assert_eq!(fs::read_to_string(&path).unwrap(), newer_document());
}

#[test]
fn documents_larger_than_the_cap_are_never_written() {
    // `load` would refuse them, leaving an inventory nothing can open.
    let path = Path::new("/x/inventory.json");
    let cap = usize::try_from(MAX_INVENTORY_BYTES).unwrap();
    check_size(cap, path).unwrap();
    let err = check_size(cap + 1, path).unwrap_err();
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(
        err.problems(),
        [Problem::new("", "the file is larger than 16 MiB")]
    );
    assert!(matches!(&err, Error::InventoryFile { path: at, .. } if at == path));
}

#[test]
fn an_oversized_save_or_update_leaves_the_file_as_it_was() {
    let dir = TempDir::new();
    let path = dir.inventory();
    save(&path, &one_server("eu-1")).unwrap();
    let before = fs::read(&path).unwrap();
    // About 17 MiB: 17 400 servers with 1 KiB of notes each, parsed in one
    // go (adding them one by one checks each id against all the others).
    let notes = "n".repeat(1024);
    let servers: Vec<String> = (0..17_400)
        .map(|i| format!(r#"{{"id":"s{i}","host":"h","notes":"{notes}"}}"#))
        .collect();
    let big = Inventory::from_json(&format!(
        r#"{{"version":1,"servers":[{}]}}"#,
        servers.join(",")
    ))
    .unwrap();
    for err in [
        save(&path, &big).unwrap_err(),
        update(&path, |inventory| {
            *inventory = big.clone();
            Ok::<_, Error>(())
        })
        .unwrap_err(),
    ] {
        assert_eq!(
            err.problems(),
            [Problem::new("", "the file is larger than 16 MiB")]
        );
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(dir.names(), ["inventory.json", "inventory.json.lock"]);
}

#[test]
fn a_panicking_change_releases_the_lock_and_writes_nothing() {
    let dir = TempDir::new();
    let path = dir.inventory();
    save(&path, &one_server("eu-1")).unwrap();
    let before = fs::read(&path).unwrap();
    let panicked = thread::spawn({
        let path = path.clone();
        move || {
            let _ = update(&path, |inventory| -> Result<()> {
                inventory.add_server(Server::new("us-1", "h"))?;
                panic!("a bug in the change");
            });
        }
    })
    .join()
    .is_err();
    assert!(panicked);
    assert_eq!(fs::read(&path).unwrap(), before);
    // The lock was released: this would wait forever otherwise.
    add_server(&path, "us-2").unwrap();
    assert_eq!(load(&path).unwrap().servers().len(), 2);
    assert_eq!(dir.names(), ["inventory.json", "inventory.json.lock"]);
}

#[test]
fn paths_ending_with_a_separator_a_dot_or_empty_name_no_file() {
    let dir = TempDir::new();
    let ending = |suffix: &str| {
        let mut path = dir.inventory().into_os_string();
        path.push(suffix);
        PathBuf::from(path)
    };
    let (slashed, dotted) = (ending("/"), ending("/."));
    for path in [slashed.as_path(), dotted.as_path(), Path::new("")] {
        for err in [
            load(path).unwrap_err(),
            save(path, &one_server("eu-1")).unwrap_err(),
            add_server(path, "eu-1").unwrap_err(),
        ] {
            assert_eq!(err.code(), Code::InventoryIo);
            assert!(err.to_string().contains("the path names no file"), "{err}");
        }
    }
    assert_eq!(dir.names(), Vec::<String>::new());
}

#[test]
fn a_lock_file_that_is_a_directory_is_refused() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let lock = sibling(&path, LOCK_SUFFIX).unwrap();
    fs::create_dir(&lock).unwrap();
    for err in [
        save(&path, &one_server("eu-1")).unwrap_err(),
        add_server(&path, "eu-1").unwrap_err(),
    ] {
        assert!(
            matches!(&err, Error::InventoryNotAFile { found: FileKind::Directory, path: at } if at == &lock),
            "{err:?}"
        );
    }
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn an_unopenable_lock_file_is_a_located_error_and_nothing_is_written() {
    let dir = TempDir::new();
    let path = dir.inventory();
    save(&path, &one_server("eu-1")).unwrap();
    let before = fs::read(&path).unwrap();
    let lock = sibling(&path, LOCK_SUFFIX).unwrap();
    set_mode(&lock, 0o400);
    if OpenOptions::new().write(true).open(&lock).is_ok() {
        eprintln!("skipped: permissions are not enforced (running as root?)");
        return;
    }
    let err = add_server(&path, "us-1").unwrap_err();
    assert!(
        matches!(&err, Error::InventoryIo { context: LOCK_OPEN_CONTEXT, path: at, .. } if at == &lock),
        "{err:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn temp_names_are_hidden_and_unique() {
    let a = temp_name(OsStr::new("inventory.json"));
    let b = temp_name(OsStr::new("inventory.json"));
    assert_ne!(a, b);
    let a = a.to_string_lossy().into_owned();
    let prefix = format!(".inventory.json.{}.", std::process::id());
    assert!(a.starts_with(&prefix) && a.ends_with(".tmp"), "{a}");
}

#[test]
fn a_leftover_temp_file_is_skipped_never_overwritten() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let stale = dir.0.join(".stale.tmp");
    fs::write(&stale, "stale").unwrap();
    let mut names = [".stale.tmp", ".fresh.tmp"].into_iter().map(OsString::from);
    let (temp, file) = TempPath::create_named(&path, || names.next().unwrap_or_default()).unwrap();
    assert_eq!(temp.path, dir.0.join(".fresh.tmp"));
    assert_eq!(fs::read_to_string(&stale).unwrap(), "stale");
    drop(file);
    drop(temp);
    // Dropped without a rename: the temp file is gone, the leftover stays.
    assert_eq!(dir.names(), [".stale.tmp"]);

    // Every name taken: an error naming the target.
    let err = TempPath::create_named(&path, || OsString::from(".stale.tmp")).unwrap_err();
    assert!(
        matches!(&err, Error::InventoryIo { context: WRITE_CONTEXT, path: at, source }
            if at == &path && source.kind() == io::ErrorKind::AlreadyExists),
        "{err:?}"
    );
}

#[test]
fn lock_and_parent_names() {
    assert_eq!(
        sibling(Path::new("/x/inventory.json"), LOCK_SUFFIX).unwrap(),
        Path::new("/x/inventory.json.lock")
    );
    assert_eq!(
        sibling(Path::new("inventory.json"), LOCK_SUFFIX).unwrap(),
        Path::new("inventory.json.lock")
    );
    assert_eq!(parent_dir(Path::new("inventory.json")), Path::new("."));
    assert_eq!(parent_dir(Path::new("/x/inventory.json")), Path::new("/x"));
    assert!(sibling(Path::new("/"), LOCK_SUFFIX).is_err());
    // A name is accepted only as the path's last text.
    assert_eq!(
        file_name(Path::new("a/inventory.json")).unwrap(),
        "inventory.json"
    );
    for path in ["", "/", "..", "a/..", "a/b/", "a/b/.", "a/b//"] {
        assert!(file_name(Path::new(path)).is_err(), "{path:?}");
    }
}

#[test]
fn file_kinds_describe_themselves() {
    assert_eq!(FileKind::Symlink.to_string(), "a symbolic link");
    assert_eq!(FileKind::Directory.to_string(), "a directory");
    assert_eq!(FileKind::Special.to_string(), "a special file");
}

// ---- update ----

#[test]
fn update_creates_the_file_and_returns_the_change_result() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let previous = update(&path, |inventory| {
        inventory.add_server(Server::new("eu-1", "h"))?;
        inventory.add_site(Site::new("shop", "eu-1", "/var/www/shop"))?;
        inventory.update_server(Server::new("eu-1", "203.0.113.10"))
    })
    .unwrap();
    assert_eq!(previous, Server::new("eu-1", "h"));
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.server("eu-1").unwrap().host, "203.0.113.10");
    assert!(loaded.site("shop").is_some());
}

#[test]
fn a_failed_change_writes_nothing() {
    let dir = TempDir::new();
    let path = dir.inventory();
    // Nothing on disk: nothing is created, even though the change made
    // edits before failing.
    let err = update(&path, |inventory| {
        inventory.add_server(Server::new("eu-1", "h"))?;
        inventory.add_site(Site::new("shop", "eu-2", "/var/www"))
    })
    .unwrap_err();
    assert_eq!(err.code(), Code::InvalidInput);
    assert_eq!(dir.names(), ["inventory.json.lock"]);

    // An existing file keeps its bytes.
    save(&path, &one_server("eu-1")).unwrap();
    let before = fs::read(&path).unwrap();
    let err = add_server(&path, "eu-1").unwrap_err();
    assert_eq!(err.code(), Code::InvalidInput);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_no_op_update_does_not_write() {
    let dir = TempDir::new();
    let path = dir.inventory();
    update(&path, |_| Ok::<_, Error>(())).unwrap();
    assert!(!path.exists());

    // An existing file is not rewritten: its odd formatting stays.
    fs::write(&path, "{ \"version\": 1 }").unwrap();
    update(&path, |inventory| {
        inventory.add_server(Server::new("eu-1", "h"))?;
        inventory.remove_server("eu-1", false).map(drop)
    })
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{ \"version\": 1 }");
}

#[test]
fn update_refuses_an_invalid_file_without_calling_the_change() {
    let dir = TempDir::new();
    let path = dir.inventory();
    fs::write(&path, "not json").unwrap();
    let mut called = false;
    let err = update(&path, |_| {
        called = true;
        Ok::<_, Error>(())
    })
    .unwrap_err();
    assert!(!called);
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
}

/// A caller's own error type, to show `update` converts into it.
#[derive(Debug, PartialEq)]
enum CallerError {
    Sites(Code),
    Refused,
}

impl From<Error> for CallerError {
    fn from(error: Error) -> Self {
        Self::Sites(error.code())
    }
}

#[test]
fn update_works_with_the_callers_error_type() {
    let dir = TempDir::new();
    let path = dir.inventory();
    let result: std::result::Result<(), CallerError> = update(&path, |_| Err(CallerError::Refused));
    assert_eq!(result, Err(CallerError::Refused));
    fs::write(&path, "[]").unwrap();
    let result: std::result::Result<(), CallerError> = update(&path, |_| Ok(()));
    assert_eq!(result, Err(CallerError::Sites(Code::InventoryInvalid)));
}

// ---- the lock and concurrency ----

#[test]
fn concurrent_updates_from_threads_all_land() {
    const THREADS: usize = 8;
    const EACH: usize = 5;
    let dir = TempDir::new();
    let path = Arc::new(dir.inventory());
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let (path, start) = (Arc::clone(&path), Arc::clone(&start));
            thread::spawn(move || {
                start.wait();
                for i in 0..EACH {
                    update(&path, |inventory| {
                        // Widen the read-modify-write window: without the
                        // lock, these updates would overwrite each other.
                        thread::sleep(Duration::from_millis(2));
                        inventory.add_server(Server::new(format!("t{t}-{i}"), "h"))
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.servers().len(), THREADS * EACH);
    for t in 0..THREADS {
        for i in 0..EACH {
            assert!(loaded.server(&format!("t{t}-{i}")).is_some(), "t{t}-{i}");
        }
    }
    assert_eq!(dir.names(), ["inventory.json", "inventory.json.lock"]);
}

#[test]
fn the_lock_makes_a_second_writer_wait() {
    let dir = TempDir::new();
    let path = Arc::new(dir.inventory());
    let inside = Arc::new(Barrier::new(2));
    let holder = {
        let (path, inside) = (Arc::clone(&path), Arc::clone(&inside));
        thread::spawn(move || {
            update(&path, |inventory| {
                inside.wait();
                thread::sleep(Duration::from_millis(200));
                inventory.add_server(Server::new("first", "h"))
            })
            .unwrap();
        })
    };
    inside.wait();
    // The holder is inside its change: this update must see its result.
    let seen = update(&path, |inventory| {
        let seen = inventory.server("first").is_some();
        inventory.add_server(Server::new("second", "h"))?;
        Ok::<_, Error>(seen)
    })
    .unwrap();
    holder.join().unwrap();
    assert!(
        seen,
        "the second update ran before the first released the lock"
    );
    assert_eq!(load(&path).unwrap().servers().len(), 2);
}

#[test]
fn readers_never_see_a_partial_document() {
    let dir = TempDir::new();
    let path = Arc::new(dir.inventory());
    save(&path, &Inventory::new()).unwrap();
    let writer = {
        let path = Arc::clone(&path);
        thread::spawn(move || {
            for i in 0..40 {
                add_server(&path, &format!("s{i}")).unwrap();
            }
        })
    };
    let mut last = 0;
    while !writer.is_finished() {
        let count = load(&path).unwrap().servers().len();
        assert!(count >= last, "went back from {last} to {count}");
        last = count;
    }
    writer.join().unwrap();
    assert_eq!(load(&path).unwrap().servers().len(), 40);
}

// ---- symbolic links (Unix: Windows needs a privilege to create them) ----

#[cfg(unix)]
mod symlinks {
    use std::os::unix::fs::symlink;

    use super::*;

    /// Assert `err` refuses `path` as a symbolic link.
    fn assert_refused(err: &Error, path: &Path) {
        assert_eq!(err.code(), Code::InventoryIo);
        assert!(
            matches!(err, Error::InventoryNotAFile { found: FileKind::Symlink, path: at } if at == path),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "refusing {}: it is a symbolic link, not a regular file",
                path.display()
            )
        );
    }

    #[test]
    fn a_symlinked_inventory_is_refused_and_its_target_untouched() {
        let dir = TempDir::new();
        let target = dir.0.join("elsewhere.json");
        save(&target, &one_server("eu-1")).unwrap();
        let before = fs::read(&target).unwrap();
        let path = dir.inventory();
        symlink(&target, &path).unwrap();

        assert_refused(&load(&path).unwrap_err(), &path);
        assert_refused(&save(&path, &Inventory::new()).unwrap_err(), &path);
        assert_refused(&add_server(&path, "us-1").unwrap_err(), &path);
        assert_eq!(fs::read(&target).unwrap(), before);
        assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    }

    #[test]
    fn a_dangling_symlink_is_refused_not_read_as_missing() {
        let dir = TempDir::new();
        let target = dir.0.join("missing.json");
        let path = dir.inventory();
        symlink(&target, &path).unwrap();
        assert_refused(&load(&path).unwrap_err(), &path);
        assert_refused(&add_server(&path, "eu-1").unwrap_err(), &path);
        assert!(!target.exists(), "nothing is created through the link");
    }

    #[test]
    fn a_symlinked_lock_file_is_refused_and_nothing_is_created_through_it() {
        let dir = TempDir::new();
        let path = dir.inventory();
        let lock = sibling(&path, LOCK_SUFFIX).unwrap();
        let target = dir.0.join("victim");
        symlink(&target, &lock).unwrap();
        assert_refused(&save(&path, &one_server("eu-1")).unwrap_err(), &lock);
        assert_refused(&add_server(&path, "eu-1").unwrap_err(), &lock);
        assert!(!target.exists());
        assert!(!path.exists());

        // A link to an existing file is refused too; the file is untouched.
        fs::remove_file(&lock).unwrap();
        fs::write(&target, "keep").unwrap();
        symlink(&target, &lock).unwrap();
        assert_refused(&add_server(&path, "eu-1").unwrap_err(), &lock);
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }

    #[test]
    fn a_symlinked_directory_above_the_file_is_followed() {
        // Only the file itself is checked: macOS's /tmp is a link.
        let dir = TempDir::new();
        let real = dir.0.join("real");
        fs::create_dir(&real).unwrap();
        let linked = dir.0.join("linked");
        symlink(&real, &linked).unwrap();
        let path = linked.join("inventory.json");
        add_server(&path, "eu-1").unwrap();
        assert!(load(&path).unwrap().server("eu-1").is_some());
        assert!(real.join("inventory.json").is_file());
    }

    #[test]
    fn a_link_swapped_in_while_opening_is_caught() {
        // The path is a regular file when inspected, then becomes a link
        // before the open: the opened file is not the inspected one, and
        // the next look refuses the link.
        let dir = TempDir::new();
        let path = dir.inventory();
        save(&path, &one_server("eu-1")).unwrap();
        let secret = dir.0.join("secret.json");
        save(&secret, &one_server("secret")).unwrap();
        let mut swapped = false;
        let mut open = |p: &Path| {
            if !swapped {
                swapped = true;
                fs::remove_file(p)?;
                symlink(&secret, p)?;
            }
            File::open(p)
        };
        let err = open_existing(&path, READ_CONTEXT, &mut open).unwrap_err();
        assert_refused(&err, &path);
    }

    #[test]
    fn a_file_replaced_while_opening_is_retried() {
        // A concurrent save renames a new file over the path between the
        // inspection and the open: the new file is opened on the next try.
        let dir = TempDir::new();
        let path = dir.inventory();
        save(&path, &one_server("old")).unwrap();
        let mut replaced = false;
        let mut open = |p: &Path| {
            if !replaced {
                replaced = true;
                let new = dir.0.join("new.json");
                fs::write(&new, one_server("new").to_json().unwrap())?;
                fs::rename(&new, p)?;
            }
            File::open(p)
        };
        let file = open_existing(&path, READ_CONTEXT, &mut open)
            .unwrap()
            .unwrap();
        let bytes = read_capped(file, &path).unwrap();
        assert_eq!(parse(&bytes).unwrap(), one_server("new"));
    }

    #[test]
    fn a_file_that_keeps_changing_is_an_error() {
        let dir = TempDir::new();
        let path = dir.inventory();
        let other = dir.0.join("other.json");
        fs::write(&path, "{}").unwrap();
        fs::write(&other, "{}").unwrap();
        let mut open = |_: &Path| File::open(&other);
        let err = open_existing(&path, READ_CONTEXT, &mut open).unwrap_err();
        assert_eq!(err.code(), Code::InventoryIo);
        assert!(err.to_string().ends_with(KEPT_CHANGING), "{err}");
    }

    #[test]
    fn a_file_removed_while_opening_is_missing() {
        let dir = TempDir::new();
        let path = dir.inventory();
        fs::write(&path, "{}").unwrap();
        let mut open = |p: &Path| {
            fs::remove_file(p)?;
            File::open(p)
        };
        let file = open_existing(&path, READ_CONTEXT, &mut open).unwrap();
        assert!(file.is_none());
    }

    #[test]
    fn same_entry_tells_files_apart() {
        let dir = TempDir::new();
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let meta = |p: &Path| fs::symlink_metadata(p).unwrap();
        let opened = |p: &Path| File::open(p).unwrap().metadata().unwrap();
        assert!(same_entry(&meta(&a), &opened(&a)));
        assert!(!same_entry(&meta(&a), &opened(&b)));
        // A directory is never "the same regular file".
        assert!(!same_entry(&meta(&dir.0), &opened(&dir.0)));
    }

    #[test]
    fn special_files_are_refused() {
        // A Unix socket stands in for any special file (no FIFO without
        // libc); opening a FIFO could block forever, so it is never opened.
        let dir = TempDir::new();
        let path = dir.inventory();
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let err = load(&path).unwrap_err();
        assert!(
            matches!(
                &err,
                Error::InventoryNotAFile {
                    found: FileKind::Special,
                    ..
                }
            ),
            "{err:?}"
        );
    }
}
