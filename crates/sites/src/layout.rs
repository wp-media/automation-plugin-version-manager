//! Names of the files and directories kept for apvm-sites inside a front
//! end's apvm directory (the CLI and the Node bindings use `~/.apvm`).
//!
//! Names only: each front end decides the location, so the library
//! hard-codes no path (plan §8.1). Sharing the names keeps every front end
//! on the same files.

/// The inventory file: `~/.apvm/inventory.json`.
pub const INVENTORY_FILE_NAME: &str = "inventory.json";

/// The directory of OpenSSH multiplexing sockets: `~/.apvm/ssh-mux`. Kept
/// short because a socket path must fit in about 100 bytes.
pub const SSH_CONTROL_DIR_NAME: &str = "ssh-mux";

/// The run-log directory, as path components: `~/.apvm/logs/sites`.
pub const SITES_LOG_DIR: [&str; 2] = ["logs", "sites"];

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `name` is one plain path component on every platform.
    fn is_plain_component(name: &str) -> bool {
        !name.is_empty()
            && !matches!(name, "." | "..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    }

    #[test]
    fn names_are_plain_components() {
        let names = [INVENTORY_FILE_NAME, SSH_CONTROL_DIR_NAME]
            .into_iter()
            .chain(SITES_LOG_DIR);
        for name in names {
            assert!(is_plain_component(name), "{name:?}");
        }
    }

    #[test]
    fn the_control_dir_name_is_short() {
        // `<home>/.apvm/<dir>/<40-hex %C>` must stay well under the ~100-byte
        // socket path limit for ordinary home directories.
        assert!(SSH_CONTROL_DIR_NAME.len() <= 8);
    }
}
