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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

#[tool_router(router = sync_router, vis = "pub(crate)")]
impl MdServer {
    /// Synchronize the vault with its git remote.
    #[tool(
        description = "Synchronize the vault with its git remote: commit local changes, rebase onto the fetched upstream, and push. Returns {status, pulled, pushed, conflicts}. status:\"conflict\" means the rebase was aborted and the vault left unchanged — the listed paths need manual/agent resolution. Errors are reserved for git execution failures."
    )]
    pub async fn sync_vault(&self) -> Result<Json<SyncVaultResponse>, ErrorData> {
        let Some(git) = self.git_sync() else {
            return Err(ErrorData::internal_error("git sync is not enabled", None));
        };
        let internal = |e: String| ErrorData::internal_error(e, None);

        let mut pulled = 0usize;
        let mut last_push_err = String::new();
        for attempt in 0..2 {
            let Some(old_upstream) = git.upstream_tip().await else {
                return Err(internal(
                    "current branch has no upstream; set one (e.g. git push -u origin <branch>) to sync".into(),
                ));
            };
            git.fetch().await.map_err(internal)?;
            let new_upstream = git
                .upstream_tip()
                .await
                .unwrap_or_else(|| old_upstream.clone());

            // Local-only section: exclusive with reads/commits in-process
            // (write guard) and with cooperating external tools (flock).
            {
                let _guard = self.lock().write().await;
                let _flock = self
                    .vault()
                    .exclusive_lock()
                    .map_err(|e| internal(e.to_string()))?;
                git.commit_all("mcp(sync): checkpoint")
                    .await
                    .map_err(internal)?;
                match git.rebase_onto_upstream().await.map_err(internal)? {
                    Rebase::Ok => {}
                    Rebase::Conflict(conflicts) => {
                        return Ok(Json(SyncVaultResponse {
                            status: "conflict".into(),
                            pulled,
                            pushed: 0,
                            conflicts,
                        }));
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
                return Ok(Json(SyncVaultResponse {
                    status: "clean".into(),
                    pulled,
                    pushed: 0,
                    conflicts: vec![],
                }));
            }
            match git.push().await {
                Ok(()) => {
                    return Ok(Json(SyncVaultResponse {
                        status: "clean".into(),
                        pulled,
                        pushed: ahead,
                        conflicts: vec![],
                    }));
                }
                Err(e) if attempt == 0 => {
                    // Likely a non-fast-forward race; refetch and retry once.
                    tracing::info!("push rejected, retrying after refetch: {e}");
                    last_push_err = e;
                }
                Err(e) => return Err(internal(e)),
            }
        }
        Err(internal(format!(
            "push failed after retry: {last_push_err}"
        )))
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
