//! The Claude Code skill files, embedded into the binary at compile time.
//!
//! Every file under `.claude/skills/apvm-cli/` in this repository is included
//! with [`include_str!`], so `apvm skill install` is fully self-contained: no
//! network access, no on-disk staging area, and the installed skill always
//! matches the binary version exactly (both come from the same commit).
//!
//! # Maintenance
//!
//! When a file is **added to or removed from** `.claude/skills/apvm-cli/`,
//! [`SKILL_FILES`] must be updated by hand — the
//! `embedded_list_matches_repo_directory` test fails otherwise, so drift
//! cannot ship. Content-only edits need no action: `include_str!` files are
//! tracked by Cargo's change detection and re-embedded on rebuild.
//!
//! # Sources
//!
//! - [`include_str!`] embeds a UTF-8 file as a `&'static str`:
//!   <https://doc.rust-lang.org/std/macro.include_str.html>
//! - `CARGO_MANIFEST_DIR` is the absolute path of this crate's directory,
//!   set by Cargo at compile time — used to anchor the include paths at the
//!   workspace root instead of relying on this module's location:
//!   <https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates>

/// One embedded skill file.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    /// Path relative to the skill root, with `/` separators
    /// (e.g. `SKILL.md`, `references/git-refs.md`).
    pub rel_path: &'static str,
    /// The file's contents.
    pub contents: &'static str,
}

/// Repository directory the skill files are embedded from, relative to this
/// crate's manifest directory (`crates/cli`).
#[cfg(test)]
const REPO_SKILL_DIR: &str = "../../.claude/skills/apvm-cli";

/// The complete `apvm-cli` skill, as shipped in this binary.
///
/// The first entry is the skill manifest (`SKILL.md`), required by the
/// [Claude Code skill layout](https://code.claude.com/docs/en/skills).
pub const SKILL_FILES: &[EmbeddedFile] = &[
    EmbeddedFile {
        rel_path: "SKILL.md",
        contents: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/skills/apvm-cli/SKILL.md"
        )),
    },
    EmbeddedFile {
        rel_path: "references/git-refs.md",
        contents: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.claude/skills/apvm-cli/references/git-refs.md"
        )),
    },
];

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::fs_ops::{SKILL_MANIFEST, validate_rel_path};
    use super::*;

    /// Absolute path of the repository skill directory the files were
    /// embedded from.
    fn repo_skill_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(REPO_SKILL_DIR)
    }

    /// Recursively list files under `dir` as skill-relative paths with `/`
    /// separators, skipping filesystem junk (`.DS_Store`).
    fn list_repo_files(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("readable directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".DS_Store" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.path().is_dir() {
                list_repo_files(&entry.path(), &rel, out);
            } else {
                out.push(rel);
            }
        }
    }

    #[test]
    fn embedded_list_matches_repo_directory() {
        let mut on_disk = Vec::new();
        list_repo_files(&repo_skill_dir(), "", &mut on_disk);
        on_disk.sort();

        let mut embedded: Vec<String> =
            SKILL_FILES.iter().map(|f| f.rel_path.to_string()).collect();
        embedded.sort();

        assert_eq!(
            embedded, on_disk,
            "SKILL_FILES is out of sync with .claude/skills/apvm-cli/ — \
             update the embedded list in crates/cli/src/commands/skill/embedded.rs"
        );
    }

    #[test]
    fn embedded_contents_match_repo_files() {
        for file in SKILL_FILES {
            let path = repo_skill_dir().join(file.rel_path);
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            assert_eq!(
                file.contents,
                on_disk,
                "embedded '{}' differs from {} — rebuild (cargo tracks include_str! \
                 inputs, so this indicates a stale build)",
                file.rel_path,
                path.display()
            );
        }
    }

    #[test]
    fn embedded_manifest_is_present_and_first() {
        assert!(!SKILL_FILES.is_empty());
        assert_eq!(SKILL_FILES[0].rel_path, SKILL_MANIFEST);
    }

    #[test]
    fn embedded_files_are_non_empty() {
        for file in SKILL_FILES {
            assert!(
                !file.contents.trim().is_empty(),
                "embedded '{}' is empty",
                file.rel_path
            );
        }
    }

    #[test]
    fn embedded_rel_paths_are_safe() {
        for file in SKILL_FILES {
            validate_rel_path(Path::new(file.rel_path))
                .unwrap_or_else(|e| panic!("embedded path '{}' invalid: {e}", file.rel_path));
        }
    }

    #[test]
    fn embedded_rel_paths_are_unique() {
        let mut paths: Vec<&str> = SKILL_FILES.iter().map(|f| f.rel_path).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(
            paths.len(),
            SKILL_FILES.len(),
            "duplicate rel_path in SKILL_FILES"
        );
    }

    #[test]
    fn manifest_has_frontmatter() {
        // A Claude Code SKILL.md starts with a `---` YAML frontmatter fence.
        assert!(
            SKILL_FILES[0].contents.starts_with("---"),
            "SKILL.md does not start with YAML frontmatter"
        );
    }

    #[test]
    fn manifest_mentions_current_version() {
        // The skill documents the binary version it ships with; CLAUDE.md
        // requires the skill to be reconciled on every version bump. This
        // test turns that contract into a hard failure.
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            SKILL_FILES[0].contents.contains(version),
            "SKILL.md does not mention the current version {version} — \
             update .claude/skills/apvm-cli/SKILL.md"
        );
    }

    // ── CLI surface ↔ skill parity ───────────────────────────────────────
    //
    // CLAUDE.md requires the skill to mirror the CLI surface. These tests walk
    // the real clap definition, so adding/renaming a subcommand or flag
    // without documenting it fails here instead of shipping a stale skill.

    /// Every embedded skill file concatenated — a flag may be documented in
    /// a reference file rather than `SKILL.md` itself.
    fn skill_text() -> String {
        SKILL_FILES
            .iter()
            .map(|f| f.contents)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Collect `(command path, command)` for `cmd` and every nested
    /// subcommand, e.g. `"cache gc"`. The root has an empty path.
    fn walk<'a>(cmd: &'a clap::Command, path: &str, out: &mut Vec<(String, &'a clap::Command)>) {
        out.push((path.to_string(), cmd));
        for sub in cmd.get_subcommands() {
            let sub_path = format!("{path} {}", sub.get_name()).trim().to_string();
            walk(sub, &sub_path, out);
        }
    }

    /// Whether the skill mentions the short flag `-c` as a standalone token
    /// (`-y`, `[-y]`, `` `-y` ``), not as part of a longer word.
    fn mentions_short(text: &str, short: char) -> bool {
        mentions_token(text, &format!("-{short}"))
    }

    /// Whether the skill mentions the long flag `--name` as a whole token, so
    /// `--verbose` never counts as documenting `--ver`.
    fn mentions_long(text: &str, long: &str) -> bool {
        mentions_token(text, &format!("--{long}"))
    }

    /// Whether `flag` occurs in `text` with no word character or `-` directly
    /// before or after it.
    fn mentions_token(text: &str, flag: &str) -> bool {
        let is_word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
        text.match_indices(flag).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + flag.len()..].chars().next();
            !before.is_some_and(is_word) && !after.is_some_and(is_word)
        })
    }

    /// One line of a skill file with the markdown headings it sits under.
    struct ScopedLine<'a> {
        /// Headings enclosing the line, outermost first.
        headings: Vec<&'a str>,
        /// The line itself.
        text: &'a str,
    }

    /// Split every embedded skill file into lines tagged with their enclosing
    /// headings. `#` lines inside fenced code blocks are shell comments, not
    /// headings; each file starts with no headings.
    fn scoped_lines() -> Vec<ScopedLine<'static>> {
        SKILL_FILES
            .iter()
            .flat_map(|file| scope_lines(file.contents))
            .collect()
    }

    /// [`scoped_lines`] for one markdown document.
    fn scope_lines(markdown: &str) -> Vec<ScopedLine<'_>> {
        let mut stack: Vec<(usize, &str)> = Vec::new();
        let mut in_fence = false;
        let mut lines = Vec::new();
        for text in markdown.lines() {
            if text.trim_start().starts_with("```") {
                in_fence = !in_fence;
            }
            let level = text.chars().take_while(|&c| c == '#').count();
            if !in_fence && level > 0 && text[level..].starts_with(' ') {
                stack.retain(|&(outer, _)| outer < level);
                stack.push((level, text));
            }
            lines.push(ScopedLine {
                headings: stack.iter().map(|&(_, heading)| heading).collect(),
                text,
            });
        }
        lines
    }

    /// Whether a flag of `apvm <path>` — found in a line by `mentions` — is
    /// documented where that command is: on a line naming the command itself,
    /// or anywhere in a section whose heading names the command or one of its
    /// parents (`### apvm cache` covers `cache clear`). Global flags (`path`
    /// empty) may appear anywhere.
    fn documents_flag(
        lines: &[ScopedLine<'_>],
        path: &str,
        mentions: impl Fn(&str) -> bool,
    ) -> bool {
        let words: Vec<&str> = path.split_whitespace().collect();
        let scopes: Vec<String> = (1..=words.len())
            .map(|n| format!("apvm {}", words[..n].join(" ")))
            .collect();
        let own = format!("apvm {path}");
        lines.iter().any(|line| {
            mentions(line.text)
                && (path.is_empty()
                    || mentions_token(line.text, &own)
                    || line
                        .headings
                        .iter()
                        .any(|heading| scopes.iter().any(|scope| mentions_token(heading, scope))))
        })
    }

    #[test]
    fn scope_lines_tracks_nested_headings_but_not_code_comments() {
        let doc = "# Top\n## `apvm cache`\n### flags\n```sh\n# a comment\nx\n```\n## Other\ny";
        let lines = scope_lines(doc);
        let headings_of = |text: &str| {
            lines
                .iter()
                .find(|l| l.text == text)
                .map(|l| l.headings.clone())
                .unwrap_or_default()
        };
        assert_eq!(headings_of("x"), ["# Top", "## `apvm cache`", "### flags"]);
        assert_eq!(headings_of("y"), ["# Top", "## Other"]);
    }

    #[test]
    fn documents_flag_only_counts_the_flags_own_command() {
        let doc = "\
## `apvm info`
  gulp (auto-install: npm install --global gulp-cli)
## `apvm cache <SUBCMD>`
| `clear` | prompts unless `-y`/`--yes` |
## Examples
apvm cache verify --checksum
apvm build x --verbose
";
        let lines = scope_lines(doc);
        let long = |name: &'static str| move |line: &str| mentions_long(line, name);
        // Another command's section, even one quoting `--global`, is no proof.
        assert!(!documents_flag(&lines, "skill install", long("global")));
        // A parent command's section covers its subcommands...
        assert!(documents_flag(&lines, "cache clear", long("yes")));
        assert!(documents_flag(&lines, "cache clear", |line: &str| {
            mentions_short(line, 'y')
        }));
        // ...but not unrelated commands sharing the flag.
        assert!(!documents_flag(&lines, "uninstall", long("yes")));
        // A line naming the exact command documents it anywhere.
        assert!(documents_flag(&lines, "cache verify", long("checksum")));
        assert!(!documents_flag(&lines, "cache gc", long("checksum")));
        // Global flags may be documented anywhere.
        assert!(documents_flag(&lines, "", long("verbose")));
    }

    #[test]
    fn mentions_short_matches_only_standalone_tokens() {
        assert!(mentions_short("apvm uninstall -y", 'y'));
        assert!(mentions_short("`apvm cache clear [-y]`", 'y'));
        assert!(!mentions_short("a pre-yes word", 'y'));
        assert!(!mentions_short("--yes", 'y'));
    }

    #[test]
    fn mentions_long_matches_only_whole_flags() {
        assert!(mentions_long("apvm build x --ver 5.1.0", "ver"));
        assert!(mentions_long("`--yes`", "yes"));
        assert!(mentions_long("[--global]", "global"));
        assert!(!mentions_long("apvm build x --verbose", "ver"));
        assert!(!mentions_long("--no-cache", "no"));
    }

    #[test]
    fn skill_documents_every_subcommand() {
        use clap::CommandFactory;
        let root = crate::Cli::command();
        let mut commands = Vec::new();
        walk(&root, "", &mut commands);

        let text = skill_text();
        let missing: Vec<String> = commands
            .iter()
            .filter(|(path, _)| !path.is_empty())
            .map(|(path, _)| format!("apvm {path}"))
            .filter(|usage| !text.contains(usage.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "the apvm-cli skill never mentions {missing:?} — update .claude/skills/apvm-cli/"
        );
    }

    #[test]
    fn skill_documents_every_flag() {
        use clap::CommandFactory;
        let root = crate::Cli::command();
        let mut commands = Vec::new();
        walk(&root, "", &mut commands);

        let lines = scoped_lines();
        let mut missing = Vec::new();
        for (path, cmd) in &commands {
            for arg in cmd.get_arguments().filter(|a| !a.is_hide_set()) {
                // The long form is the canonical spelling, so it must be
                // documented whenever it exists (a short `-y` alone hides
                // `--yes` from readers); a short-only flag needs its short
                // form. Positionals have neither and are covered by usage.
                let documented = match (arg.get_long(), arg.get_short()) {
                    (Some(long), _) => {
                        documents_flag(&lines, path, |line| mentions_long(line, long))
                    }
                    (None, Some(short)) => {
                        documents_flag(&lines, path, |line| mentions_short(line, short))
                    }
                    (None, None) => true,
                };
                if !documented {
                    missing.push(format!("apvm {path} {}", arg.get_id()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "the apvm-cli skill never mentions these flags: {missing:?} — \
             update .claude/skills/apvm-cli/"
        );
    }
}
