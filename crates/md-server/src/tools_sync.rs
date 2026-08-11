//! The `sync_vault` tool
//! ([ADR-0016](../../../docs/adr/0016-git-sync-integration.md)), registered
//! only when git sync is enabled.
//!
//! Sequence — network I/O never runs under the write guard:
//! fetch (no guard) → guard + flock { sweep commit, rebase onto upstream } →
//! push (no guard). A non-fast-forward push triggers one retry from the fetch.
//! A rebase conflict aborts (restoring the tree) and is reported as a normal
//! result, not an error: it is one of sync's outcomes, resolved by agents or
//! humans, never by leaving conflict markers in a note.

use rmcp::handler::server::wrapper::Json;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::Serialize;

use crate::MdServer;
use crate::sync::Rebase;

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SyncVaultResponse {
    /// `clean` (synced) or `conflict` (rebase aborted; see `conflicts`).
    pub status: String,
    /// Files changed by the pull (0 when nothing new was fetched).
    pub pulled: usize,
    /// Commits pushed to the remote.
    pub pushed: u64,
    /// Conflicted paths when `status` is `conflict`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

#[tool_router(router = sync_router, vis = "pub(crate)")]
impl MdServer {
    /// Synchronize the vault with its git remote.
    #[tool(
        description = "Synchronize the vault with its git remote: commit local changes, rebase onto the fetched upstream, and push. Returns {status, pulled, pushed, conflicts}. status:\"conflict\" means the rebase was aborted and the vault left unchanged — the listed paths need manual/agent resolution. Errors are reserved for git execution failures. Commit model: when auto-commit is enabled each write batch is already committed as `mcp(<tool>)` the moment it lands, so the sweep here picks up only edits made outside mcp — status:\"clean\" with pushed:0 right after your writes means they were committed (and possibly auto-pushed) earlier, not lost.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn sync_vault(&self) -> Result<Json<SyncVaultResponse>, ErrorData> {
        self.run_sync()
            .await
            .map(Json)
            .map_err(|e| ErrorData::internal_error(e, None))
    }
}

impl MdServer {
    /// The full sync sequence — shared by the `sync_vault` tool and the
    /// automation tasks (ADR-0018). `Err` is a git execution failure;
    /// conflicts are a normal result. Every outcome updates the sync health
    /// surfaced on write responses (ADR-0019), so the manual tool, the
    /// interval task, and the auto-push fallback all feed one state.
    pub(crate) async fn run_sync(&self) -> Result<SyncVaultResponse, String> {
        let result = self.run_sync_inner().await;
        if let Some(git) = self.git_sync() {
            match &result {
                Ok(r) if r.status == "conflict" => {
                    let ahead = git.ahead_count().await;
                    self.report_sync_failure(
                        ahead,
                        format!(
                            "rebase conflict in {}; resolve via sync_vault",
                            r.conflicts.join(", ")
                        ),
                    );
                }
                Ok(r) => {
                    // One structured line per sync that moved anything, so the
                    // shipped log timeline shows every pull/push (ADR-0021).
                    if r.pulled > 0 || r.pushed > 0 {
                        tracing::info!(pulled = r.pulled, pushed = r.pushed, "sync applied");
                    }
                    self.report_sync_ok();
                }
                Err(e) => {
                    let ahead = git.ahead_count().await;
                    let first_line = e.lines().next().unwrap_or(e).to_string();
                    self.report_sync_failure(ahead, first_line);
                }
            }
        }
        result
    }

    async fn run_sync_inner(&self) -> Result<SyncVaultResponse, String> {
        let Some(git) = self.git_sync() else {
            return Err("git sync is not enabled".into());
        };

        let mut pulled = 0usize;
        let mut last_push_err = String::new();
        for attempt in 0..2 {
            let Some(old_upstream) = git.upstream_tip().await else {
                return Err(
                    "current branch has no upstream; set one (e.g. git push -u origin <branch>) to sync"
                        .into(),
                );
            };
            git.fetch().await?;
            let new_upstream = git
                .upstream_tip()
                .await
                .unwrap_or_else(|| old_upstream.clone());

            // Local-only section: exclusive with reads/commits in-process
            // (write guard) and with cooperating external tools (flock).
            {
                let _guard = self.lock().write().await;
                let _flock = self.vault().exclusive_lock().map_err(|e| e.to_string())?;
                git.commit_all("mcp(sync): checkpoint").await?;
                match git.rebase_onto_upstream().await? {
                    Rebase::Ok => {}
                    Rebase::Conflict(conflicts) => {
                        return Ok(SyncVaultResponse {
                            status: "conflict".into(),
                            pulled,
                            pushed: 0,
                            conflicts,
                        });
                    }
                }
            }

            // The rebase applied the fetched changes: publish them to the
            // event journal so the stream stays a complete account (ADR-0017).
            if new_upstream != old_upstream {
                let ops = git.changed_between(&old_upstream, &new_upstream).await;
                pulled += ops.len();
                self.emit_event("sync_vault", None, &ops);
            }

            let ahead = git.ahead_count().await;
            if ahead == 0 {
                return Ok(SyncVaultResponse {
                    status: "clean".into(),
                    pulled,
                    pushed: 0,
                    conflicts: vec![],
                });
            }
            match git.push().await {
                Ok(()) => {
                    return Ok(SyncVaultResponse {
                        status: "clean".into(),
                        pulled,
                        pushed: ahead,
                        conflicts: vec![],
                    });
                }
                Err(e) if attempt == 0 => {
                    // Likely a non-fast-forward race; refetch and retry once.
                    tracing::info!(error = %e, "push rejected; retrying after refetch");
                    last_push_err = e;
                }
                Err(e) => return Err(e),
            }
        }
        Err(format!("push failed after retry: {last_push_err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use md_core::Vault;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::Path;
    use std::process::Command;

    use crate::sync::GitSync;
    use crate::tools_write::{CreateNotesRequest, NoteInput};

    #[test]
    fn condensed_sync_response_satisfies_its_schema() {
        crate::envelope::assert_condensed_satisfies_schema(
            rmcp::schemars::schema_for!(SyncVaultResponse),
            SyncVaultResponse {
                status: "clean".into(),
                pulled: 0,
                pushed: 0,
                conflicts: vec![],
            },
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A vault repo tracking a bare remote, plus a second clone of the remote.
    fn repo_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        let vault = root.path().join("vault");
        let clone = root.path().join("clone");
        git(root.path(), &["init", "--bare", "-b", "main", "remote.git"]);
        git(root.path(), &["init", "-b", "main", "vault"]);
        git(&vault, &["config", "user.name", "test"]);
        git(&vault, &["config", "user.email", "test@example.com"]);
        std::fs::write(vault.join("seed.md"), "# Seed\n").unwrap();
        git(&vault, &["add", "-A"]);
        git(&vault, &["commit", "-m", "seed"]);
        git(&vault, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&vault, &["push", "-u", "origin", "main"]);
        git(
            root.path(),
            &["clone", "-b", "main", bare.to_str().unwrap(), "clone"],
        );
        git(&clone, &["config", "user.name", "other"]);
        git(&clone, &["config", "user.email", "other@example.com"]);
        (root, vault, clone)
    }

    async fn server_with_sync(vault_dir: &Path) -> MdServer {
        let vault = Vault::open(vault_dir).unwrap();
        let git = GitSync::preflight(vault_dir).await.unwrap();
        MdServer::new(vault).with_git_sync(git)
    }

    #[tokio::test]
    async fn preflight_refuses_a_vault_subdirectory() {
        let (_root, vault, _clone) = repo_fixture();
        let sub = vault.join("notes");
        std::fs::create_dir_all(&sub).unwrap();
        let err = GitSync::preflight(&sub).await.unwrap_err();
        assert!(err.contains("toplevel"), "got: {err}");
        assert!(GitSync::preflight(&vault).await.is_ok());
    }

    #[tokio::test]
    async fn sync_vault_commits_and_pushes_local_writes() {
        let (_root, vault_dir, clone) = repo_fixture();
        let s = server_with_sync(&vault_dir).await;
        s.create_notes(Parameters(CreateNotesRequest {
            notes: vec![NoteInput {
                path: "new.md".into(),
                content: "# New\n".into(),
                frontmatter: None,
            }],
            overwrite: false,
        }))
        .await
        .unwrap();

        let r = s.sync_vault().await.unwrap().0;
        assert_eq!(r.status, "clean");
        assert_eq!(r.pushed, 1, "one sweep commit pushed");
        assert_eq!(r.pulled, 0);

        // The other clone sees the note after pulling.
        git(&clone, &["pull"]);
        assert!(clone.join("new.md").exists());
    }

    #[tokio::test]
    async fn sync_vault_pulls_remote_changes() {
        let (_root, vault_dir, clone) = repo_fixture();
        std::fs::write(clone.join("remote-note.md"), "# Remote\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-m", "remote note"]);
        git(&clone, &["push"]);

        let s = server_with_sync(&vault_dir).await;
        let r = s.sync_vault().await.unwrap().0;
        assert_eq!(r.status, "clean");
        assert_eq!(r.pulled, 1);
        assert!(vault_dir.join("remote-note.md").exists());
    }

    #[tokio::test]
    async fn sync_vault_reports_conflicts_and_leaves_the_tree_clean() {
        let (_root, vault_dir, clone) = repo_fixture();
        // Both sides change seed.md differently.
        std::fs::write(clone.join("seed.md"), "# Seed\nremote line\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-m", "remote edit"]);
        git(&clone, &["push"]);
        std::fs::write(vault_dir.join("seed.md"), "# Seed\nlocal line\n").unwrap();

        let s = server_with_sync(&vault_dir).await;
        let r = s.sync_vault().await.unwrap().0;
        assert_eq!(r.status, "conflict");
        assert_eq!(r.conflicts, vec!["seed.md".to_string()]);
        // The rebase was aborted: local content intact, no markers, no
        // rebase-in-progress state.
        let text = std::fs::read_to_string(vault_dir.join("seed.md")).unwrap();
        assert_eq!(text, "# Seed\nlocal line\n");
        assert!(!vault_dir.join(".git/rebase-merge").exists());
        assert!(!vault_dir.join(".git/rebase-apply").exists());
    }

    #[tokio::test]
    async fn auto_commit_is_per_batch_and_path_scoped() {
        let (_root, vault_dir, _clone) = repo_fixture();
        // A concurrent external edit that must never be swept into an
        // mcp-attributed commit (ADR-0018).
        std::fs::write(vault_dir.join("external.md"), "# External\n").unwrap();

        let s = server_with_sync(&vault_dir).await.with_auto_commit();
        s.create_notes(Parameters(CreateNotesRequest {
            notes: vec![
                NoteInput {
                    path: "one.md".into(),
                    content: "# One\n".into(),
                    frontmatter: None,
                },
                NoteInput {
                    path: "two.md".into(),
                    content: "# Two\n".into(),
                    frontmatter: None,
                },
            ],
            overwrite: false,
        }))
        .await
        .unwrap();

        let out = Command::new("git")
            .args(["log", "--format=%s", "--name-only", "-1"])
            .current_dir(&vault_dir)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(log.contains("mcp(create_notes): 2 notes"), "got: {log}");
        assert!(log.contains("one.md") && log.contains("two.md"));
        assert!(
            !log.contains("external.md"),
            "external edit swept into mcp commit: {log}"
        );

        // The external edit is still dirty — left for sync's sweep commit.
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&vault_dir)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&status.stdout).to_string();
        assert!(status.contains("external.md"), "got: {status}");
    }

    #[tokio::test]
    async fn auto_push_lands_commits_on_the_remote_after_the_quiet_window() {
        let (root, vault_dir, _clone) = repo_fixture();
        let bare = root.path().join("remote.git");
        let s = server_with_sync(&vault_dir)
            .await
            .with_auto_commit()
            .with_auto_push(std::time::Duration::from_millis(100));
        s.create_notes(Parameters(CreateNotesRequest {
            notes: vec![NoteInput {
                path: "pushed.md".into(),
                content: "# Pushed\n".into(),
                frontmatter: None,
            }],
            overwrite: false,
        }))
        .await
        .unwrap();

        // Poll the bare remote until the auto-commit arrives.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let out = Command::new("git")
                .args(["log", "--format=%s", "main"])
                .current_dir(&bare)
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stdout).to_string();
            if log.contains("mcp(create_notes)") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "auto-push never landed; remote log:\n{log}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn write_responses_surface_sync_warning_while_push_fails() {
        let (root, vault_dir, _clone) = repo_fixture();
        let bare = root.path().join("remote.git");
        let off = root.path().join("remote.git.off");
        let s = server_with_sync(&vault_dir).await.with_auto_commit();

        // The remote vanishes: the explicit sync fails and records ill health.
        std::fs::rename(&bare, &off).unwrap();
        assert!(s.sync_vault().await.is_err());

        let r = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "while-down.md".into(),
                    content: "# Down\n".into(),
                    frontmatter: None,
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        let warning = r
            .sync_warning
            .expect("write must carry sync_warning while sync is failing");
        assert!(warning.contains("not on the remote"), "got: {warning}");

        // The remote returns: a clean sync clears the warning on later writes.
        std::fs::rename(&off, &bare).unwrap();
        assert_eq!(s.sync_vault().await.unwrap().0.status, "clean");
        let r = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "after-recovery.md".into(),
                    content: "# Up\n".into(),
                    frontmatter: None,
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(
            r.sync_warning.is_none(),
            "warning must clear after a clean sync: {:?}",
            r.sync_warning
        );
    }

    #[tokio::test]
    async fn rebase_conflict_surfaces_in_sync_warning() {
        let (_root, vault_dir, clone) = repo_fixture();
        std::fs::write(clone.join("seed.md"), "# Seed\nremote line\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-m", "remote edit"]);
        git(&clone, &["push"]);
        std::fs::write(vault_dir.join("seed.md"), "# Seed\nlocal line\n").unwrap();

        let s = server_with_sync(&vault_dir).await.with_auto_commit();
        assert_eq!(s.sync_vault().await.unwrap().0.status, "conflict");

        let r = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "unrelated.md".into(),
                    content: "# Unrelated\n".into(),
                    frontmatter: None,
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        let warning = r.sync_warning.expect("conflict must surface on writes");
        assert!(
            warning.contains("conflict") && warning.contains("seed.md"),
            "got: {warning}"
        );
    }

    /// Poll the bare remote's log until `needle` shows up (or panic at the
    /// deadline).
    async fn wait_for_remote_commit(bare: &Path, needle: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let out = Command::new("git")
                .args(["log", "--format=%s", "main"])
                .current_dir(bare)
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stdout).to_string();
            if log.contains(needle) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "commit never landed on the remote; log:\n{log}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn auto_commit_message_records_the_condensed_tool_call() {
        let (_root, vault_dir, _clone) = repo_fixture();
        let s = server_with_sync(&vault_dir).await.with_auto_commit();
        let long = "x".repeat(500);
        s.create_notes(Parameters(CreateNotesRequest {
            notes: vec![NoteInput {
                path: "cmd.md".into(),
                content: long.clone(),
                frontmatter: None,
            }],
            overwrite: false,
        }))
        .await
        .unwrap();

        let out = Command::new("git")
            .args(["log", "-1", "--format=%B"])
            .current_dir(&vault_dir)
            .output()
            .unwrap();
        let msg = String::from_utf8_lossy(&out.stdout).to_string();
        // Subject unchanged; the body carries the condensed invocation.
        assert!(
            msg.starts_with("mcp(create_notes): 1 notes\n"),
            "got: {msg}"
        );
        assert!(msg.contains("create_notes {"), "no call in body: {msg}");
        assert!(msg.contains("cmd.md"), "got: {msg}");
        assert!(
            !msg.contains(&long),
            "long content must be truncated: {msg}"
        );
        assert!(msg.contains("chars)"), "truncation marker expected: {msg}");
    }

    #[tokio::test]
    async fn auto_push_retries_after_transient_failure() {
        let (root, vault_dir, _clone) = repo_fixture();
        let bare = root.path().join("remote.git");
        let off = root.path().join("remote.git.off");

        let mut s = server_with_sync(&vault_dir).await.with_auto_commit();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        s.push_tx = Some(tx);
        tokio::spawn(crate::sync::auto_push_task(
            s.clone(),
            rx,
            std::time::Duration::from_millis(40),  // debounce
            std::time::Duration::from_millis(150), // retry initial
            std::time::Duration::from_millis(400), // retry cap
        ));

        // The remote is down when the write lands: the push and its fallback
        // sync both fail.
        std::fs::rename(&bare, &off).unwrap();
        s.create_notes(Parameters(CreateNotesRequest {
            notes: vec![NoteInput {
                path: "retry.md".into(),
                content: "# Retry\n".into(),
                frontmatter: None,
            }],
            overwrite: false,
        }))
        .await
        .unwrap();

        // Wait until the failure is recorded (first attempt happened) …
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.sync_warning().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "first auto-push attempt never failed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // … then bring the remote back: the retry timer (no new writes!) must
        // land the commit and clear the health.
        std::fs::rename(&off, &bare).unwrap();
        wait_for_remote_commit(&bare, "mcp(create_notes)").await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.sync_warning().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "sync health never cleared after the retried push"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn auto_push_drains_a_stranded_backlog_on_startup() {
        let (root, vault_dir, _clone) = repo_fixture();
        let bare = root.path().join("remote.git");

        // A commit stranded by a previous run: ahead of the remote, no writes
        // coming.
        std::fs::write(vault_dir.join("stranded.md"), "# Stranded\n").unwrap();
        git(&vault_dir, &["add", "-A"]);
        git(&vault_dir, &["commit", "-m", "stranded backlog"]);

        let _s = server_with_sync(&vault_dir)
            .await
            .with_auto_commit()
            .with_auto_push(std::time::Duration::from_millis(50));

        wait_for_remote_commit(&bare, "stranded backlog").await;
    }

    #[tokio::test]
    async fn interval_sync_pulls_remote_changes_unattended() {
        let (_root, vault_dir, clone) = repo_fixture();
        std::fs::write(clone.join("from-remote.md"), "# From remote\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-m", "remote note"]);
        git(&clone, &["push"]);

        let _s = server_with_sync(&vault_dir)
            .await
            .with_sync_interval(std::time::Duration::from_millis(100));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !vault_dir.join("from-remote.md").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "interval sync never pulled the remote note"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn sync_vault_tool_is_hidden_without_git_sync() {
        let dir = tempfile::tempdir().unwrap();
        let s = MdServer::new(Vault::open(dir.path()).unwrap());
        let names: Vec<_> = MdServer::base_router()
            .list_all()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(!names.iter().any(|n| n == "sync_vault"));
        let r = s.sync_vault().await;
        assert!(r.is_err(), "direct call without git sync must error");
    }
}
