//! Removals that ask first (`server remove`, `site remove`): show what
//! would go, ask once (unless `-y`), then remove in one `io::update` that
//! refuses when what would go is no longer what the user confirmed.
//!
//! The question is built from a preview on a plain load (no lock is held
//! while a person reads it), so another process may change the inventory
//! meanwhile; the re-check under the lock turns that race into
//! `inventory_changed` instead of removing something nobody confirmed.

use std::path::Path;

use apvm_sites::Inventory;
use apvm_sites::inventory::io;

use super::{Console, Failure, confirm_change};

/// What a declined confirmation prints (exit 0, like `apvm cache clear`).
pub const ABORTED: &str = "Aborted. Nothing changed.\n";

/// A removal of one record that asks first.
pub trait Removal {
    /// What the library returns when it removes: the record and what went
    /// with it.
    type Removed;
    /// What the question showed, compared again under the lock.
    type Scope: PartialEq;

    /// Remove from `inventory` (the library refuses an unknown id, a
    /// record in use or a read-only inventory, changing nothing).
    ///
    /// # Errors
    ///
    /// The library's refusal.
    fn remove(&self, inventory: &mut Inventory) -> Result<Self::Removed, apvm_sites::Error>;

    /// What `removed` takes away, as the question shows it.
    fn scope(removed: &Self::Removed) -> Self::Scope;

    /// The confirmation question, without the `[y/N]` hint.
    fn question(removed: &Self::Removed) -> String;

    /// Why the removal is refused without a terminal to confirm on.
    fn refusal(&self) -> String;

    /// The failure when what would go changed during the confirmation.
    fn changed(&self) -> Failure;
}

/// The outcome of asking before a removal.
#[derive(Debug, PartialEq, Eq)]
enum Confirmation<S> {
    /// `-y`: nothing was asked.
    Skipped,
    /// The user confirmed this scope.
    Confirmed(S),
    /// The user declined.
    Declined,
}

/// Run `removal`: ask unless `yes` (printing [`ABORTED`] when declined),
/// then remove in one `io::update`, unless what would go differs from what
/// was confirmed.
///
/// # Arguments
///
/// * `path` - The inventory file
/// * `removal` - What to remove
/// * `yes` - Whether `-y`/`--yes` was passed
/// * `console` - Where to ask and print
///
/// # Returns
///
/// What was removed, or `None` when the user declined.
///
/// # Errors
///
/// The library's refusal (before any question when it refuses the
/// preview); those of [`confirm_change`]; the [`Removal::changed`]
/// failure when the scope differs (nothing is written then).
pub fn remove_confirmed<R: Removal>(
    path: &Path,
    removal: &R,
    yes: bool,
    console: &mut Console<'_>,
) -> Result<Option<R::Removed>, Failure> {
    let confirmed = match confirm(path, removal, yes, console)? {
        Confirmation::Declined => {
            console.print(ABORTED);
            return Ok(None);
        }
        Confirmation::Skipped => None,
        Confirmation::Confirmed(scope) => Some(scope),
    };
    io::update(path, |inventory| {
        let removed = removal.remove(inventory)?;
        match &confirmed {
            Some(scope) if *scope != R::scope(&removed) => Err(removal.changed()),
            _ => Ok(removed),
        }
    })
    .map(Some)
}

/// Ask before removing (unless `yes`), showing what would go. A removal the
/// inventory refuses fails before the question.
///
/// # Errors
///
/// The library's refusal, or those of [`confirm_change`].
fn confirm<R: Removal>(
    path: &Path,
    removal: &R,
    yes: bool,
    console: &mut Console<'_>,
) -> Result<Confirmation<R::Scope>, Failure> {
    if yes {
        return Ok(Confirmation::Skipped);
    }
    let preview = removal.remove(&mut io::load(path)?)?;
    let question = R::question(&preview);
    if confirm_change(console, false, &question, || removal.refusal())? {
        Ok(Confirmation::Confirmed(R::scope(&preview)))
    } else {
        Ok(Confirmation::Declined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::inventory::Recorder;
    use apvm_sites::{Server, ServerRemoval};

    /// Removes server `id`; the scope is its host.
    struct ByHost(&'static str);

    impl Removal for ByHost {
        type Removed = ServerRemoval;
        type Scope = String;

        fn remove(&self, inventory: &mut Inventory) -> Result<ServerRemoval, apvm_sites::Error> {
            inventory.remove_server(self.0, false)
        }

        fn scope(removed: &ServerRemoval) -> String {
            removed.server.host.clone()
        }

        fn question(removed: &ServerRemoval) -> String {
            format!("Remove {}?", removed.server.id)
        }

        fn refusal(&self) -> String {
            "pass -y".into()
        }

        fn changed(&self) -> Failure {
            Failure::Changed("changed".into())
        }
    }

    /// An inventory file with server `a` on host `h`.
    fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("inventory.json");
        io::update(&path, |inventory| {
            inventory.add_server(Server::new("a", "h"))
        })
        .expect("seed");
        (dir, path)
    }

    #[test]
    fn yes_removes_without_asking() {
        let (_dir, path) = seeded();
        let mut recorder = Recorder::new();
        let removed = recorder.run(None, |console| {
            remove_confirmed(&path, &ByHost("a"), true, console)
        });
        assert_eq!(
            removed.expect("removed").map(|r| r.server.id),
            Some("a".into())
        );
        assert!(recorder.asked.is_empty());
        assert!(io::load(&path).expect("load").servers().is_empty());
    }

    #[test]
    fn a_declined_question_prints_aborted_and_writes_nothing() {
        let (_dir, path) = seeded();
        let before = std::fs::read(&path).expect("read");
        let mut recorder = Recorder::new();
        let removed = recorder.run(Some(false), |console| {
            remove_confirmed(&path, &ByHost("a"), false, console)
        });
        assert!(removed.expect("declined").is_none());
        assert_eq!(recorder.asked, ["Remove a? [y/N]"]);
        assert_eq!(recorder.out, ABORTED);
        assert_eq!(std::fs::read(&path).expect("read"), before);
    }

    #[test]
    fn a_refused_preview_fails_before_asking() {
        let (_dir, path) = seeded();
        let mut recorder = Recorder::new();
        let result = recorder.run(Some(true), |console| {
            remove_confirmed(&path, &ByHost("nope"), false, console)
        });
        assert_eq!(
            result.map(|_| ()).map_err(|f| f.code()),
            Err("invalid_input")
        );
        assert!(recorder.asked.is_empty());
    }

    #[test]
    fn no_terminal_is_a_confirmation_refusal() {
        let (_dir, path) = seeded();
        let mut recorder = Recorder::new();
        let result = recorder.run(None, |console| {
            remove_confirmed(&path, &ByHost("a"), false, console)
        });
        let failure = result.map(|_| ()).expect_err("refused");
        assert_eq!(failure.code(), "confirmation_required");
        assert_eq!(failure.to_string(), "pass -y");
    }

    #[test]
    fn a_scope_changed_during_the_question_removes_nothing() {
        let (_dir, path) = seeded();
        let mut recorder = Recorder::new();
        let result = recorder.run_asking(
            |_| {
                io::update(&path, |inventory| {
                    inventory.update_server(Server::new("a", "elsewhere"))
                })
                .expect("concurrent edit");
                Ok(Some(true))
            },
            |console| remove_confirmed(&path, &ByHost("a"), false, console),
        );
        assert_eq!(
            result.map(|_| ()).map_err(|f| f.code()),
            Err("inventory_changed")
        );
        assert!(io::load(&path).expect("load").server("a").is_some());
    }
}
