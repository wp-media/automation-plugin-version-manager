//! How `apvm group` shows the inventory: tables for people, documents for
//! `--json` (records as the inventory stores them).

use apvm_sites::Group;
use serde::Serialize;

use crate::commands::inventory::{Failure, json_document, table};

/// What `list` prints when the inventory has no groups.
const NO_GROUPS: &str = "No groups yet. Add one with: apvm group add <ID> [<SITE>…]\n";

/// How a group without sites shows its sites in `show` (as `server show`
/// does for a server without sites).
const NO_SITES: &str = "none";

/// `group list`: the groups as a table, or as a `--json` document.
///
/// # Arguments
///
/// * `groups` - Every group, in inventory order
/// * `json` - Whether to render JSON
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn list_view(groups: &[Group], json: bool) -> Result<String, Failure> {
    if json {
        return json_document(&ListJson { groups });
    }
    Ok(list_text(groups))
}

/// `group show`: one group, as text or as a `--json` document.
///
/// # Arguments
///
/// * `group` - The group record
/// * `json` - Whether to render JSON
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn show_view(group: &Group, json: bool) -> Result<String, Failure> {
    if json {
        return json_document(&ShowJson { group });
    }
    Ok(show_text(group))
}

/// `--json` body of `group list`.
#[derive(Serialize)]
struct ListJson<'a> {
    /// Every group, as the inventory stores it (`id`, `sites`, and
    /// `notes` when set), in inventory order.
    groups: &'a [Group],
}

/// `--json` body of `group show`.
#[derive(Serialize)]
struct ShowJson<'a> {
    /// The group record, as the inventory stores it.
    group: &'a Group,
}

/// `group list` as a table: id, how many sites, and which (in the
/// group's order; the cell is empty for a group without sites).
fn list_text(groups: &[Group]) -> String {
    if groups.is_empty() {
        return NO_GROUPS.to_string();
    }
    let header = ["ID", "SITES", "MEMBERS"].map(String::from).to_vec();
    let rows = groups.iter().map(|group| {
        vec![
            group.id.clone(),
            group.sites.len().to_string(),
            group.sites.join(", "),
        ]
    });
    table("", &std::iter::once(header).chain(rows).collect::<Vec<_>>())
}

/// `group show` as text: the member sites in the group's order (or
/// [`NO_SITES`]), then the notes when set.
fn show_text(group: &Group) -> String {
    let sites = if group.sites.is_empty() {
        NO_SITES.to_string()
    } else {
        group.sites.join(", ")
    };
    let mut rows = vec![vec!["sites".to_string(), sites]];
    if let Some(notes) = &group.notes {
        rows.push(vec!["notes".to_string(), notes.clone()]);
    }
    format!("Group '{}'\n{}", group.id, table("  ", &rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `qa` (two sites, with notes) and `empty` (no sites, no notes).
    fn sample() -> Vec<Group> {
        let qa = Group {
            notes: Some("release candidates".into()),
            ..Group::new("qa", vec!["shop".into(), "blog".into()])
        };
        vec![qa, Group::new("empty", Vec::new())]
    }

    /// Parse a `--json` document.
    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid JSON")
    }

    #[test]
    fn list_says_how_to_start() {
        assert_eq!(list_view(&[], false).expect("renders"), NO_GROUPS);
        assert_eq!(
            json(&list_view(&[], true).expect("renders")),
            serde_json::json!({"schema": 1, "groups": []})
        );
    }

    #[test]
    fn list_aligns_one_row_per_group() {
        assert_eq!(
            list_view(&sample(), false).expect("renders"),
            "ID     SITES  MEMBERS\n\
             qa     2      shop, blog\n\
             empty  0\n"
        );
    }

    #[test]
    fn list_json_holds_each_sparse_record() {
        assert_eq!(
            json(&list_view(&sample(), true).expect("renders")),
            serde_json::json!({"schema": 1, "groups": [
                {"id": "qa", "sites": ["shop", "blog"], "notes": "release candidates"},
                {"id": "empty", "sites": []}
            ]})
        );
    }

    #[test]
    fn show_lists_the_sites_then_the_notes() {
        let groups = sample();
        assert_eq!(
            show_view(&groups[0], false).expect("renders"),
            "Group 'qa'\n  sites  shop, blog\n  notes  release candidates\n"
        );
        assert_eq!(
            show_view(&groups[1], false).expect("renders"),
            "Group 'empty'\n  sites  none\n"
        );
    }

    #[test]
    fn show_json_holds_the_record() {
        let groups = sample();
        assert_eq!(
            json(&show_view(&groups[1], true).expect("renders")),
            serde_json::json!({"schema": 1, "group": {"id": "empty", "sites": []}})
        );
        let doc = json(&show_view(&groups[0], true).expect("renders"));
        assert_eq!(doc["group"]["notes"], "release candidates");
    }
}
