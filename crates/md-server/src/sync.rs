//! Git integration ([ADR-0016](../../../docs/adr/0016-git-sync-integration.md)).
//!
//! Coexistence hardening runs whenever the vault is a git repository; the sync
//! driver itself is opt-in (`MD_GIT_SYNC=1`).

use std::io::Write;
use std::path::Path;

/// The exclusion line keeping the server's internal state out of the repo.
const EXCLUDE_LINE: &str = ".md-mcp/";

/// Ensure `.md-mcp/` is excluded from the vault's git repository, if one
/// exists. Written to `.git/info/exclude` — per-machine state, unlike the
/// user-owned, replicated `.gitignore`, which is never touched. A `.git`
/// *file* (worktree/submodule gitfile) is left alone: resolving the real git
/// dir is not worth it for a hardening step. Best-effort: failures are logged,
/// never fatal — a missing exclusion degrades sync hygiene, not correctness.
pub fn ensure_git_exclude(vault_root: &Path) {
    let git_dir = vault_root.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    let exclude = git_dir.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == EXCLUDE_LINE) {
        return;
    }
    let appended = std::fs::create_dir_all(git_dir.join("info")).and_then(|()| {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude)?;
        // Guard against a final line without a newline in an existing file.
        let lead = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        writeln!(f, "{lead}{EXCLUDE_LINE}")
    });
    match appended {
        Ok(()) => tracing::info!("added {EXCLUDE_LINE} to .git/info/exclude"),
        Err(e) => tracing::warn!("cannot write .git/info/exclude: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_exclusion_once_and_only_in_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // Not a repo: nothing appears.
        ensure_git_exclude(dir.path());
        assert!(!dir.path().join(".git").exists());

        // A repo without info/exclude: the file is created with the line.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        ensure_git_exclude(dir.path());
        let exclude = dir.path().join(".git/info/exclude");
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(text, ".md-mcp/\n");

        // Idempotent: a second run appends nothing.
        ensure_git_exclude(dir.path());
        assert_eq!(std::fs::read_to_string(&exclude).unwrap(), ".md-mcp/\n");
    }

    #[test]
    fn appends_after_existing_content_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        let exclude = dir.path().join(".git/info/exclude");
        std::fs::write(&exclude, "*.tmp").unwrap(); // no trailing newline
        ensure_git_exclude(dir.path());
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            "*.tmp\n.md-mcp/\n"
        );
    }

    #[test]
    fn gitfile_worktree_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: ../elsewhere\n").unwrap();
        ensure_git_exclude(dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".git")).unwrap(),
            "gitdir: ../elsewhere\n"
        );
    }
}
