//! The inventory file through the public API, across processes: concurrent
//! `update`s from several processes all land, a tracked save refuses to
//! overwrite another process's edit, and `env_override` reads
//! `APVM_INVENTORY` from the real environment.
//!
//! Child processes re-run this test binary with [`CHILD_ENV`] set, so they
//! execute [`child_process_worker`]; run normally, that test does nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use apvm_sites::inventory::io::{self, Fingerprint, INVENTORY_ENV};
use apvm_sites::{Code, Inventory, Server};

/// Tells a child process what to do: `update` or `env`.
const CHILD_ENV: &str = "APVM_SITES_IO_TEST_CHILD";
/// The inventory path a child updates.
const PATH_ENV: &str = "APVM_SITES_IO_TEST_PATH";
/// The server id prefix a child uses.
const PREFIX_ENV: &str = "APVM_SITES_IO_TEST_PREFIX";
/// Updates each child makes.
const UPDATES_PER_CHILD: usize = 10;
/// Marks the line a child prints for the parent.
const REPORT: &str = "env_override=";

/// A fresh temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    /// Create a unique directory under the system temp dir.
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("apvm-io-it-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("create the temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// This test binary, running only [`child_process_worker`], with `envs`
/// set and `APVM_INVENTORY` removed unless `envs` sets it.
fn child(envs: &[(&str, &str)]) -> Command {
    let exe = std::env::current_exe().expect("the test binary");
    let mut command = Command::new(exe);
    command
        .args([
            "child_process_worker",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove(INVENTORY_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    command
}

/// Assert a child succeeded, showing its output otherwise.
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "child failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What the child does when [`CHILD_ENV`] is set; nothing otherwise.
#[test]
fn child_process_worker() {
    let Some(task) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    match task.to_str() {
        Some("update") => child_updates(),
        Some("env") => println!("{REPORT}{:?}", io::env_override()),
        other => panic!("unknown child task {other:?}"),
    }
}

/// Add [`UPDATES_PER_CHILD`] servers, one `update` each.
fn child_updates() {
    let path = PathBuf::from(std::env::var_os(PATH_ENV).expect("the path"));
    let prefix = std::env::var(PREFIX_ENV).expect("the prefix");
    for i in 0..UPDATES_PER_CHILD {
        io::update(&path, |inventory| {
            // Widen the read-modify-write window, so unlocked updates
            // would overwrite each other.
            std::thread::sleep(std::time::Duration::from_millis(3));
            inventory.add_server(Server::new(format!("{prefix}-{i}"), "203.0.113.10"))
        })
        .expect("update");
    }
}

#[test]
fn concurrent_updates_from_processes_all_land() {
    const CHILDREN: usize = 4;
    let dir = TempDir::new("processes");
    let path = dir.0.join("inventory.json");
    let path_text = path.to_str().expect("a UTF-8 temp path");
    let prefixes: Vec<String> = (0..CHILDREN).map(|c| format!("p{c}")).collect();
    let running: Vec<_> = prefixes
        .iter()
        .map(|prefix| {
            child(&[
                (CHILD_ENV, "update"),
                (PATH_ENV, path_text),
                (PREFIX_ENV, prefix),
            ])
            .spawn()
            .expect("spawn a child")
        })
        .collect();
    for process in running {
        assert_success(&process.wait_with_output().expect("wait for a child"));
    }

    let inventory = io::load(&path).expect("load");
    assert_eq!(inventory.servers().len(), CHILDREN * UPDATES_PER_CHILD);
    for prefix in &prefixes {
        for i in 0..UPDATES_PER_CHILD {
            let id = format!("{prefix}-{i}");
            assert!(inventory.server(&id).is_some(), "{id} was lost");
        }
    }
}

/// What `env_override` returns in a child with `APVM_INVENTORY` = `value`
/// (`None`: unset), as the child prints it.
fn env_override_in_child(value: Option<&str>) -> String {
    let mut envs = vec![(CHILD_ENV, "env")];
    if let Some(value) = value {
        envs.push((INVENTORY_ENV, value));
    }
    let output = child(&envs).output().expect("run a child");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // libtest prints `test child_process_worker ... ` before it, on the same
    // line.
    stdout
        .lines()
        .find_map(|line| line.split_once(REPORT))
        .map(|(_, report)| report.to_string())
        .unwrap_or_else(|| panic!("no report in:\n{stdout}"))
}

#[test]
fn the_env_override_reads_apvm_inventory() {
    assert_eq!(env_override_in_child(None), "None");
    assert_eq!(env_override_in_child(Some("")), "None");
    let path = Path::new("/srv/apvm/inventory.json");
    assert_eq!(
        env_override_in_child(path.to_str()),
        format!("{:?}", Some(path.to_path_buf()))
    );
}

#[test]
fn a_saved_inventory_loads_back_and_errors_are_located() {
    let dir = TempDir::new("api");
    let path = dir.0.join("apvm").join("inventory.json");
    assert_eq!(io::load(&path).expect("missing = empty"), Inventory::new());

    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new("eu-1", "203.0.113.10"))
        .expect("valid");
    io::save(&path, &inventory).expect("save");
    assert_eq!(io::load(&path).expect("load"), inventory);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    std::fs::write(
        &path,
        r#"{"version":1,"groups":[{"id":"qa","sites":["shop"]}]}"#,
    )
    .expect("write");
    let err = io::load(&path).expect_err("unknown site");
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(err.problems()[0].location, "groups[0].sites[0]");
    assert!(
        err.to_string().starts_with(&path.display().to_string()),
        "{err}"
    );
}

/// Run one child that adds [`UPDATES_PER_CHILD`] servers named
/// `<prefix>-<n>` to the inventory at `path`, and wait for it.
fn update_in_child(path: &Path, prefix: &str) {
    let path_text = path.to_str().expect("a UTF-8 temp path");
    let output = child(&[
        (CHILD_ENV, "update"),
        (PATH_ENV, path_text),
        (PREFIX_ENV, prefix),
    ])
    .output()
    .expect("run a child");
    assert_success(&output);
}

#[test]
fn a_tracked_save_never_overwrites_another_process_edit() {
    let dir = TempDir::new("tracked");
    let path = dir.0.join("inventory.json");
    io::save(&path, &Inventory::new()).expect("save");
    let (mut mine, loaded) = io::load_tracked(&path).expect("load");
    mine.add_server(Server::new("mine", "203.0.113.20"))
        .expect("valid");

    // Another process edits the file between this load and this save.
    update_in_child(&path, "theirs");
    let theirs = std::fs::read(&path).expect("read");
    let err = io::save_if_unchanged(&path, &mine, &loaded).expect_err("a conflict");
    assert_eq!(err.code(), Code::InventoryChanged);
    assert_eq!(
        std::fs::read(&path).expect("read"),
        theirs,
        "their edit was lost"
    );

    // Load again, reapply and save: both edits land, and the returned
    // fingerprint chains into the next save.
    let (mut fresh, reloaded) = io::load_tracked(&path).expect("reload");
    fresh
        .add_server(Server::new("mine", "203.0.113.20"))
        .expect("valid");
    let saved = io::save_if_unchanged(&path, &fresh, &reloaded).expect("save");
    let on_disk = io::load(&path).expect("load");
    assert_eq!(on_disk.servers().len(), UPDATES_PER_CHILD + 1);
    assert!(on_disk.server("mine").is_some());

    // A second child edits again; the fingerprint from the save is stale.
    update_in_child(&path, "again");
    let err = io::save_if_unchanged(&path, &fresh, &saved).expect_err("a conflict");
    assert_eq!(err.code(), Code::InventoryChanged);
    assert_eq!(
        io::load(&path).expect("load").servers().len(),
        2 * UPDATES_PER_CHILD + 1
    );
}

#[test]
fn an_empty_inventory_expecting_no_file_never_wipes_one() {
    let dir = TempDir::new("missing");
    let path = dir.0.join("inventory.json");
    update_in_child(&path, "theirs");
    let err = io::save_if_unchanged(&path, &Inventory::new(), &Fingerprint::missing())
        .expect_err("the file exists");
    assert_eq!(err.code(), Code::InventoryChanged);
    assert_eq!(
        io::load(&path).expect("load").servers().len(),
        UPDATES_PER_CHILD
    );
}
