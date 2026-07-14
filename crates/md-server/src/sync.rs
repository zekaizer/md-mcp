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

/// Exclusion lines keeping non-note state out of the vault repo. `.md-mcp/` is
/// the server's own state. `.*/` mirrors the listing rule ([`listing`]) that
/// hides every dot-*directory* (`.claude/`, `.obsidian/`, …) while keeping
/// dot-*files*, which are legitimate notes: a trailing `/` matches directories
/// only. Without `.*/` the sync sweep's `git add -A` would push those hidden
/// dirs to the remote.
const EXCLUDE_LINES: &[&str] = &[".md-mcp/", ".*/"];

/// Ensure the [`EXCLUDE_LINES`] are present in the vault's git repository, if
/// one exists. Written to the repository's `info/exclude` — per-machine state,
/// unlike the user-owned, replicated `.gitignore`, which is never touched.
/// Only the missing lines are appended, so an existing repo carrying just the
/// legacy `.md-mcp/` line gains `.*/` without churn. A `.git` *file*
/// (worktree/submodule/--separate-git-dir gitfile) is followed to the real git
/// dir — and through `commondir` for linked worktrees — so the lines land
/// where git reads them. Best-effort: failures are logged, never fatal — a
/// missing exclusion degrades sync hygiene, not correctness.
pub fn ensure_git_exclude(vault_root: &Path) {
    let Some(git_dir) = resolve_git_dir(vault_root) else {
        return;
    };
    let exclude = git_dir.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&str> = EXCLUDE_LINES
        .iter()
        .copied()
        .filter(|line| !current.lines().any(|l| l.trim() == *line))
        .collect();
    if missing.is_empty() {
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
        write!(f, "{lead}")?;
        for line in &missing {
            writeln!(f, "{line}")?;
        }
        Ok(())
    });
    match appended {
        Ok(()) => tracing::info!(added = ?missing, "updated .git/info/exclude"),
        Err(e) => tracing::warn!(error = %e, "cannot write .git/info/exclude"),
    }
}

/// The vault's git directory: `.git` itself when a directory, or the target of
/// a `.git` gitfile (`gitdir: <path>`, relative to the vault root or
/// absolute), then through a `commondir` file when present — a linked
/// worktree's excludes live in the main repository's git dir, not the
/// per-worktree one. `None` (not a repo, malformed gitfile, dangling target)
/// means there is nothing to harden.
fn resolve_git_dir(vault_root: &Path) -> Option<PathBuf> {
    let dot_git = vault_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.strip_prefix("gitdir:")?.trim();
    // join() replaces the base when the target is absolute; canonicalize
    // verifies the resolved dir actually exists.
    let git_dir = std::fs::canonicalize(vault_root.join(target)).ok()?;
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(common) => std::fs::canonicalize(git_dir.join(common.trim())).ok(),
        Err(_) => Some(git_dir),
    }
}

/// The most recent push/sync failure, kept for surfacing on write responses
/// (ADR-0019). Cleared by any successful sync or push.
#[derive(Debug)]
pub(crate) struct SyncHealth {
    /// When the current failure streak started (kept across repeats).
    pub since: std::time::Instant,
    /// Local commits not on the upstream at the last failed attempt.
    pub ahead: u64,
    /// One-line reason (first line of the git error, or the conflict list).
    pub reason: String,
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
        // A restructure can leave a batch path that git cannot stage: a
        // directory whose notes were relocated out earlier is empty-in-git and
        // gone from the worktree, so `git add -- <that path>` fails with
        // "pathspec did not match any files" and aborts the *whole* batch. Keep
        // only paths git can stage — present in the worktree (creates/edits) or
        // tracked in the index (deletions) — so one vanished path never sinks a
        // real change in the same batch.
        let mut addable: Vec<&str> = Vec::new();
        for p in paths {
            let in_worktree = self.root.join(p).exists();
            let tracked = in_worktree
                || !self
                    .run(&["ls-files", "--", p])
                    .await
                    .unwrap_or_default()
                    .trim()
                    .is_empty();
            if tracked {
                addable.push(p.as_str());
            }
        }
        if addable.is_empty() {
            return Ok(false);
        }
        let mut args = vec!["add", "--"];
        args.extend(addable.iter().copied());
        self.run(&args).await?;
        // `--cached -- <paths>` scopes the emptiness check to the batch.
        let mut check = vec!["diff", "--cached", "--quiet", "--"];
        check.extend(addable.iter().copied());
        if self.output(&check).await?.status.success() {
            return Ok(false); // nothing staged for these paths
        }
        let mut commit = vec!["commit", "-m", message, "--"];
        commit.extend(addable.iter().copied());
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

// --- automation tasks (ADR-0018) --------------------------------------------

/// Debounced auto-push: waits until `debounce` passes with no new commit
/// signal, then pushes. A rejected push falls back to one full sync (whose
/// conflicts are logged and left local — an explicit `sync_vault` reports
/// them to an agent).
///
/// A failed attempt (push and fallback sync both failing) arms a retry timer —
/// `retry_initial` doubling up to `retry_cap` — so a transient outage heals
/// without waiting for the next write (ADR-0019). A rebase conflict is *not*
/// retried: it is deterministic until the local or remote history changes, and
/// the next commit signal or interval sync re-attempts anyway. A fresh commit
/// signal resets the backoff.
pub(crate) async fn auto_push_task(
    server: crate::MdServer,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    debounce: std::time::Duration,
    retry_initial: std::time::Duration,
    retry_cap: std::time::Duration,
) {
    let mut backoff = retry_initial;
    let mut retry_at: Option<tokio::time::Instant> = None;
    loop {
        // Wait for a commit signal, or for the pending retry to come due.
        let signaled = match retry_at {
            Some(at) => tokio::select! {
                received = rx.recv() => match received {
                    Some(()) => true,
                    None => return,
                },
                () = tokio::time::sleep_until(at) => false,
            },
            None => match rx.recv().await {
                Some(()) => true,
                None => return,
            },
        };
        if signaled {
            // Debounce: restart the quiet window on every further signal.
            loop {
                match tokio::time::timeout(debounce, rx.recv()).await {
                    Ok(Some(())) => {}
                    Ok(None) => return,
                    Err(_) => break, // window elapsed quietly
                }
            }
            backoff = retry_initial; // fresh activity resets the backoff
        }
        retry_at = None;

        let Some(git) = server.git_sync() else { return };
        if git.ahead_count().await == 0 {
            server.report_sync_ok();
            continue;
        }
        if git.push().await.is_ok() {
            server.report_sync_ok();
            continue;
        }
        match server.run_sync().await {
            Ok(r) if r.status == "conflict" => {
                tracing::warn!(
                    conflicts = ?r.conflicts,
                    "auto-push: rebase conflict left local; resolve via sync_vault"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in_secs = backoff.as_secs(),
                    "auto-push failed; retry armed"
                );
                retry_at = Some(tokio::time::Instant::now() + backoff);
                backoff = (backoff * 2).min(retry_cap);
            }
        }
    }
}

/// Periodic full sync, pulling remote changes without agent involvement.
pub(crate) async fn interval_sync_task(server: crate::MdServer, every: std::time::Duration) {
    loop {
        tokio::time::sleep(every).await;
        log_background_sync("interval sync", server.run_sync().await);
    }
}

fn log_background_sync(what: &str, result: Result<crate::tools_sync::SyncVaultResponse, String>) {
    match result {
        Ok(r) if r.status == "conflict" => {
            tracing::warn!(
                task = what,
                conflicts = ?r.conflicts,
                "background sync: rebase conflict left local; resolve via sync_vault"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(task = what, error = %e, "background sync failed"),
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

        // A repo without info/exclude: the file is created with both lines.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        ensure_git_exclude(dir.path());
        let exclude = dir.path().join(".git/info/exclude");
        let text = std::fs::read_to_string(&exclude).unwrap();
        assert_eq!(text, ".md-mcp/\n.*/\n");

        // Idempotent: a second run appends nothing.
        ensure_git_exclude(dir.path());
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            ".md-mcp/\n.*/\n"
        );
    }

    #[test]
    fn migrates_a_legacy_exclude_by_appending_only_the_missing_line() {
        // An older repo already carries the legacy `.md-mcp/` line; the sweep
        // guard `.*/` must be added without duplicating what is present.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        let exclude = dir.path().join(".git/info/exclude");
        std::fs::write(&exclude, ".md-mcp/\n").unwrap();
        ensure_git_exclude(dir.path());
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            ".md-mcp/\n.*/\n"
        );
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
            "*.tmp\n.md-mcp/\n.*/\n"
        );
    }

    #[test]
    fn dangling_or_malformed_gitfile_is_ignored() {
        // Target directory does not exist: nothing to write, gitfile untouched.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: ../elsewhere\n").unwrap();
        ensure_git_exclude(dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".git")).unwrap(),
            "gitdir: ../elsewhere\n"
        );

        // Not a gitfile at all: ignored.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "not a gitfile\n").unwrap();
        ensure_git_exclude(dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".git")).unwrap(),
            "not a gitfile\n"
        );
    }

    #[test]
    fn gitfile_lands_exclude_in_the_separate_git_dir() {
        // `git init --separate-git-dir` layout: `.git` is a file pointing at
        // the real git dir, absolute or relative to the vault root.
        for target in ["absolute", "relative"] {
            let dir = tempfile::tempdir().unwrap();
            let git_dir = dir.path().join("meta/repo.git");
            std::fs::create_dir_all(&git_dir).unwrap();
            let vault = dir.path().join("vault");
            std::fs::create_dir_all(&vault).unwrap();
            let pointer = match target {
                "absolute" => format!("gitdir: {}\n", git_dir.display()),
                _ => "gitdir: ../meta/repo.git\n".to_string(),
            };
            std::fs::write(vault.join(".git"), pointer).unwrap();
            ensure_git_exclude(&vault);
            assert_eq!(
                std::fs::read_to_string(git_dir.join("info/exclude")).unwrap(),
                ".md-mcp/\n.*/\n",
                "{target} gitdir"
            );
        }
    }

    #[test]
    fn linked_worktree_exclude_lands_in_the_common_dir() {
        // A linked worktree's gitdir is `<main>/.git/worktrees/<name>`, whose
        // `commondir` file points back at the main `.git` — where git actually
        // reads info/exclude from.
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("main/.git");
        let wt_git_dir = common.join("worktrees/wt");
        std::fs::create_dir_all(&wt_git_dir).unwrap();
        std::fs::write(wt_git_dir.join("commondir"), "../..\n").unwrap();
        let vault = dir.path().join("wt");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(
            vault.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )
        .unwrap();
        ensure_git_exclude(&vault);
        assert_eq!(
            std::fs::read_to_string(common.join("info/exclude")).unwrap(),
            ".md-mcp/\n.*/\n"
        );
        assert!(
            !wt_git_dir.join("info/exclude").exists(),
            "nothing written into the per-worktree dir"
        );
    }

    /// The sync sweep stages the whole worktree with `git add -A`. It must not
    /// carry dot-directories that the tools already hide from listing (e.g.
    /// `.claude/` local agent state) onto the remote, while dot-*files* — which
    /// are legitimate notes — must still commit.
    #[tokio::test]
    async fn sweep_excludes_hidden_dot_dirs_but_keeps_dot_file_notes() {
        let dir = tempfile::tempdir().unwrap();
        let git = init_repo(dir.path()).await;
        ensure_git_exclude(dir.path());

        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(dir.path().join(".keep.md"), "dot-file note").unwrap();
        std::fs::write(dir.path().join("note.md"), "note").unwrap();

        let created = git.commit_all("mcp(sync): checkpoint").await.unwrap();
        assert!(created, "the real notes make the tree dirty");

        let files = git
            .run(&["log", "-1", "--name-only", "--format="])
            .await
            .unwrap();
        assert!(
            !files.contains(".claude"),
            "hidden dot-dir leaked into the sweep: {files}"
        );
        assert!(files.contains("note.md"), "note not committed: {files}");
        assert!(
            files.contains(".keep.md"),
            "dot-file note must still commit: {files}"
        );
    }

    async fn init_repo(root: &Path) -> GitSync {
        let git = GitSync {
            root: root.to_path_buf(),
        };
        git.run(&["init", "-q"]).await.unwrap();
        git.run(&["config", "user.email", "t@t"]).await.unwrap();
        git.run(&["config", "user.name", "t"]).await.unwrap();
        git
    }

    /// A restructure can leave a batch path that git cannot stage: a directory
    /// whose notes were relocated out earlier is empty-in-git and absent from
    /// the worktree, so `git add -- <that path>` fails with "pathspec did not
    /// match any files" and would abort the whole batch. The real deletion in
    /// the same batch must still commit; the vanished path is simply skipped.
    #[tokio::test]
    async fn commit_paths_skips_a_vanished_pathspec() {
        let dir = tempfile::tempdir().unwrap();
        let git = init_repo(dir.path()).await;
        std::fs::write(dir.path().join("keep.md"), "keep").unwrap();
        std::fs::write(dir.path().join("gone.md"), "gone").unwrap();
        git.run(&["add", "-A"]).await.unwrap();
        git.run(&["commit", "-qm", "init"]).await.unwrap();

        // Delete gone.md and also target `ghost` — a path git no longer tracks.
        std::fs::remove_file(dir.path().join("gone.md")).unwrap();
        let created = git
            .commit_paths(
                "mcp(delete_notes): 2 notes",
                &["gone.md".into(), "ghost".into()],
            )
            .await
            .expect("a vanished pathspec must not fail the whole commit");

        assert!(created, "the real deletion should still be committed");
        let log = git
            .run(&["log", "-1", "--name-status", "--format="])
            .await
            .unwrap();
        assert!(
            log.contains("gone.md"),
            "deletion not committed; log: {log}"
        );
    }

    /// When every path in the batch has vanished, there is simply nothing to
    /// commit — a no-op, not an error.
    #[tokio::test]
    async fn commit_paths_is_a_noop_when_all_paths_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let git = init_repo(dir.path()).await;
        std::fs::write(dir.path().join("keep.md"), "keep").unwrap();
        git.run(&["add", "-A"]).await.unwrap();
        git.run(&["commit", "-qm", "init"]).await.unwrap();

        let created = git
            .commit_paths("mcp(delete_notes): 1 notes", &["ghost".into()])
            .await
            .expect("an all-vanished batch must not error");
        assert!(!created, "nothing stage-able means no commit");
    }
}
