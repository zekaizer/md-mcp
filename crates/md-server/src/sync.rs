//! Git integration ([ADR-0016](../../../docs/adr/0016-git-sync-integration.md)).
//!
//! Coexistence hardening runs whenever the vault is a git repository; the sync
//! driver itself is opt-in (`MD_GIT_SYNC=1`). The driver shells out to the
//! system `git` binary — exact merge semantics and ambient credential handling
//! for zero dependencies — with prompts disabled (`GIT_TERMINAL_PROMPT=0`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

use crate::events::EventOp;

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

/// How a rebase onto the fetched upstream ended.
#[derive(Debug)]
pub enum Rebase {
    /// Applied cleanly (or there was nothing to rebase onto).
    Ok,
    /// Aborted on conflict; the working tree is back to its pre-rebase state.
    Conflict(Vec<String>),
}

/// The git driver: a vault-rooted handle running the system `git` binary.
#[derive(Debug)]
pub struct GitSync {
    root: PathBuf,
}

impl GitSync {
    /// Validate the ADR-0016 preconditions and build the driver: the `git`
    /// binary must run, the vault must be a repository, and the vault root
    /// must be the repository toplevel (a vault that is a *subdirectory* of a
    /// repo is refused: a pull would change files outside the vault).
    pub async fn preflight(vault_root: &Path) -> Result<Self, String> {
        let sync = Self {
            root: vault_root.to_path_buf(),
        };
        let toplevel = sync
            .run(&["rev-parse", "--show-toplevel"])
            .await
            .map_err(|e| format!("git sync disabled: {e}"))?;
        let toplevel = std::fs::canonicalize(toplevel.trim())
            .map_err(|e| format!("git sync disabled: cannot resolve toplevel: {e}"))?;
        let root = std::fs::canonicalize(vault_root)
            .map_err(|e| format!("git sync disabled: cannot resolve vault root: {e}"))?;
        if toplevel != root {
            return Err(format!(
                "git sync disabled: vault root {} is not the repository toplevel {}",
                root.display(),
                toplevel.display()
            ));
        }
        Ok(sync)
    }

    /// Run git with `args`; success returns stdout, failure the stderr text.
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.output(args).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&"?"),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    async fn output(&self, args: &[&str]) -> Result<Output, String> {
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|e| format!("cannot run git: {e}"))
    }

    /// The upstream tip (`@{u}`), if the current branch has one.
    pub async fn upstream_tip(&self) -> Option<String> {
        self.run(&["rev-parse", "@{u}"])
            .await
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Fetch the current branch's remote.
    pub async fn fetch(&self) -> Result<(), String> {
        self.run(&["fetch"]).await.map(|_| ())
    }

    /// Push the current branch. `Err` carries stderr; the caller decides
    /// whether a non-fast-forward rejection warrants a retry.
    pub async fn push(&self) -> Result<(), String> {
        self.run(&["push"]).await.map(|_| ())
    }

    /// Commits on HEAD not yet on the upstream.
    pub async fn ahead_count(&self) -> u64 {
        self.run(&["rev-list", "--count", "@{u}..HEAD"])
            .await
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Stage everything and commit as `message` if the tree is dirty.
    /// Returns whether a commit was created.
    pub async fn commit_all(&self, message: &str) -> Result<bool, String> {
        if self
            .run(&["status", "--porcelain"])
            .await?
            .trim()
            .is_empty()
        {
            return Ok(false);
        }
        self.run(&["add", "-A"]).await?;
        self.run(&["commit", "-m", message]).await?;
        Ok(true)
    }

    /// Stage `paths` and commit as `message` if any of them changed.
    /// Path-scoped (`git add -- <paths>`), never `-A`: a concurrent external
    /// edit must not be swept into an mcp-attributed commit (ADR-0018).
    pub async fn commit_paths(&self, message: &str, paths: &[String]) -> Result<bool, String> {
        if paths.is_empty() {
            return Ok(false);
        }
        let mut args = vec!["add", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).await?;
        // `--cached -- <paths>` scopes the emptiness check to the batch.
        let mut check = vec!["diff", "--cached", "--quiet", "--"];
        check.extend(paths.iter().map(String::as_str));
        if self.output(&check).await?.status.success() {
            return Ok(false); // nothing staged for these paths
        }
        let mut commit = vec!["commit", "-m", message, "--"];
        commit.extend(paths.iter().map(String::as_str));
        self.run(&commit).await?;
        Ok(true)
    }

    /// Rebase HEAD onto the fetched upstream tip. On conflict, collect the
    /// conflicted paths, abort (restoring the pre-rebase state), and report
    /// them; conflict markers never reach a note.
    pub async fn rebase_onto_upstream(&self) -> Result<Rebase, String> {
        let out = self.output(&["rebase", "@{u}"]).await?;
        if out.status.success() {
            return Ok(Rebase::Ok);
        }
        let conflicts = self
            .run(&["diff", "--name-only", "--diff-filter=U"])
            .await
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let _ = self.run(&["rebase", "--abort"]).await;
        Ok(Rebase::Conflict(conflicts))
    }

    /// The changes `new` introduces over `old`, as event ops — used to publish
    /// pulled changes to the event journal (ADR-0017).
    pub async fn changed_between(&self, old: &str, new: &str) -> Vec<EventOp> {
        let range = [old, new];
        let out = self
            .output(&[
                "diff",
                "--name-status",
                "-z",
                "--no-renames",
                &range.join(".."),
            ])
            .await;
        let Ok(out) = out else { return Vec::new() };
        if !out.status.success() {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut fields = text.split('\0').filter(|s| !s.is_empty());
        let mut ops = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            let path = path.to_string();
            match status.chars().next() {
                Some('A') => ops.push(EventOp::Create { path }),
                Some('D') => ops.push(EventOp::Delete { path }),
                Some(_) => ops.push(EventOp::Write { path }),
                None => {}
            }
        }
        ops
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
