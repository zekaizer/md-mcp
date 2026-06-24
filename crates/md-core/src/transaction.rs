//! The multi-file transaction engine.
//!
//! [`Vault::commit_batch`] applies a list of [`Op`]s atomically: a write-ahead
//! undo journal records each step before it mutates, displaced originals are
//! moved aside by rename, and any failure (or a crash, via
//! [`Vault::recover_transactions`] at open) rolls the batch back to no effect
//! ([ADR-0007](../../../docs/adr/0007-multi-file-transaction.md)).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::vault::Vault;

const JOURNAL_DIR: &str = ".md-mcp/journal";
const BACKUP_DIR: &str = ".md-mcp/backup";
const TRASH_DIR: &str = ".md-mcp/trash";

/// One primitive file mutation in a transaction.
#[derive(Clone, Debug)]
pub enum Op {
    /// Create or overwrite a note with new content.
    Write { path: String, content: Vec<u8> },
    /// Move a note or directory to the trash.
    Delete { path: String },
    /// Rename or relocate a note or directory.
    Move { from: String, to: String },
}

/// The recorded result of an applied [`Op`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpOutcome {
    Written { path: String },
    Deleted { path: String, trashed_to: String },
    Moved { from: String, to: String },
}

#[derive(Serialize, Deserialize)]
struct Journal {
    batch_id: String,
    committed: bool,
    undo: Vec<UndoStep>,
}

#[derive(Serialize, Deserialize)]
enum UndoStep {
    DeletePath { path: String },
    RestoreFromBackup { backup: String, path: String },
    RestoreFromTrash { trash: String, path: String },
    ReverseMove { from: String, to: String },
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_batch_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:032x}-{c:x}")
}

fn strip_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

impl Vault {
    /// Apply a batch of ops atomically. On any failure nothing is left changed.
    pub fn commit_batch(&self, ops: &[Op]) -> Result<Vec<OpOutcome>> {
        let batch_id = new_batch_id();
        let journal_path = format!("{JOURNAL_DIR}/{batch_id}.json");
        let mut journal = Journal {
            batch_id: batch_id.clone(),
            committed: false,
            undo: Vec::new(),
        };
        self.write_journal(&journal_path, &journal)?;

        let mut outcomes = Vec::with_capacity(ops.len());
        for (k, op) in ops.iter().enumerate() {
            match self.apply_op(&batch_id, k, op, &mut journal, &journal_path) {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    self.rollback(&journal);
                    self.cleanup(&batch_id, &journal_path);
                    return Err(e);
                }
            }
        }

        journal.committed = true;
        self.write_journal(&journal_path, &journal)?;
        self.cleanup(&batch_id, &journal_path);
        Ok(outcomes)
    }

    /// Roll back any transaction a crash left incomplete. Called at open.
    pub(crate) fn recover_transactions(&self) -> Result<()> {
        let Ok(entries) = self.dir().read_dir(JOURNAL_DIR) else {
            return Ok(());
        };
        let mut journal_paths = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.ends_with(".json")
            {
                journal_paths.push(format!("{JOURNAL_DIR}/{name}"));
            }
        }
        for jpath in journal_paths {
            let Ok(bytes) = self.dir().read(&jpath) else {
                continue;
            };
            let Ok(journal) = serde_json::from_slice::<Journal>(&bytes) else {
                continue;
            };
            if !journal.committed {
                self.rollback(&journal);
            }
            self.cleanup(&journal.batch_id, &jpath);
        }
        Ok(())
    }

    fn apply_op(
        &self,
        batch_id: &str,
        k: usize,
        op: &Op,
        journal: &mut Journal,
        journal_path: &str,
    ) -> Result<OpOutcome> {
        match op {
            Op::Write { path, content } => {
                let path = strip_slash(path);
                if self.dir().exists(path) {
                    let backup = format!("{BACKUP_DIR}/{batch_id}/{k}");
                    journal.undo.push(UndoStep::RestoreFromBackup {
                        backup: backup.clone(),
                        path: path.to_string(),
                    });
                    self.write_journal(journal_path, journal)?;
                    self.move_path(path, &backup)?;
                } else {
                    journal.undo.push(UndoStep::DeletePath {
                        path: path.to_string(),
                    });
                    self.write_journal(journal_path, journal)?;
                }
                self.write_atomic(path, content)?;
                Ok(OpOutcome::Written {
                    path: path.to_string(),
                })
            }
            Op::Delete { path } => {
                let path = strip_slash(path);
                if !self.dir().exists(path) {
                    return Err(Error::not_found(format!(
                        "cannot delete missing path: {path}"
                    )));
                }
                let trash = self.unique_trash_path(path);
                journal.undo.push(UndoStep::RestoreFromTrash {
                    trash: trash.clone(),
                    path: path.to_string(),
                });
                self.write_journal(journal_path, journal)?;
                self.move_path(path, &trash)?;
                Ok(OpOutcome::Deleted {
                    path: path.to_string(),
                    trashed_to: trash,
                })
            }
            Op::Move { from, to } => {
                let from = strip_slash(from);
                let to = strip_slash(to);
                if self.dir().exists(to) {
                    let backup = format!("{BACKUP_DIR}/{batch_id}/{k}");
                    journal.undo.push(UndoStep::RestoreFromBackup {
                        backup: backup.clone(),
                        path: to.to_string(),
                    });
                    self.write_journal(journal_path, journal)?;
                    self.move_path(to, &backup)?;
                }
                journal.undo.push(UndoStep::ReverseMove {
                    from: from.to_string(),
                    to: to.to_string(),
                });
                self.write_journal(journal_path, journal)?;
                self.move_path(from, to)?;
                Ok(OpOutcome::Moved {
                    from: from.to_string(),
                    to: to.to_string(),
                })
            }
        }
    }

    fn write_journal(&self, journal_path: &str, journal: &Journal) -> Result<()> {
        let bytes = serde_json::to_vec(journal)
            .map_err(|e| Error::io(format!("serialize journal: {e}")))?;
        self.write_atomic(journal_path, &bytes)
    }

    fn ensure_parent(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            self.dir()
                .create_dir_all(parent)
                .map_err(|e| Error::io(format!("mkdir {}: {e}", parent.display())))?;
        }
        Ok(())
    }

    fn move_path(&self, from: &str, to: &str) -> Result<()> {
        self.ensure_parent(to)?;
        self.dir()
            .rename(from, self.dir(), to)
            .map_err(|e| Error::io(format!("rename {from} -> {to}: {e}")))
    }

    fn unique_trash_path(&self, path: &str) -> String {
        let base = format!("{TRASH_DIR}/{path}");
        if !self.dir().exists(&base) {
            return base;
        }
        (1..)
            .map(|n| format!("{base}.{n}"))
            .find(|c| !self.dir().exists(c))
            .unwrap_or_else(|| format!("{base}.x"))
    }

    fn remove_path(&self, path: &str) {
        match self.dir().symlink_metadata(path) {
            Ok(m) if m.is_dir() => {
                let _ = self.dir().remove_dir_all(path);
            }
            Ok(_) => {
                let _ = self.dir().remove_file(path);
            }
            Err(_) => {}
        }
    }

    fn rollback(&self, journal: &Journal) {
        for step in journal.undo.iter().rev() {
            match step {
                UndoStep::DeletePath { path } => self.remove_path(path),
                UndoStep::RestoreFromBackup { backup, path }
                | UndoStep::RestoreFromTrash {
                    trash: backup,
                    path,
                } => {
                    if self.dir().exists(backup) {
                        let _ = self.ensure_parent(path);
                        let _ = self.dir().rename(backup, self.dir(), path);
                    }
                }
                UndoStep::ReverseMove { from, to } => {
                    if self.dir().exists(to) && !self.dir().exists(from) {
                        let _ = self.ensure_parent(from);
                        let _ = self.dir().rename(to, self.dir(), from);
                    }
                }
            }
        }
    }

    fn cleanup(&self, batch_id: &str, journal_path: &str) {
        let _ = self
            .dir()
            .remove_dir_all(format!("{BACKUP_DIR}/{batch_id}"));
        let _ = self.dir().remove_file(journal_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        (dir, vault)
    }

    #[test]
    fn commit_writes_deletes_and_moves_atomically() {
        let (_d, vault) = temp_vault();
        vault.write_atomic("old.md", b"old content").unwrap();
        vault.write_atomic("gone.md", b"bye").unwrap();

        let outcomes = vault
            .commit_batch(&[
                Op::Write {
                    path: "old.md".into(),
                    content: b"new content".to_vec(),
                },
                Op::Write {
                    path: "fresh.md".into(),
                    content: b"created".to_vec(),
                },
                Op::Delete {
                    path: "gone.md".into(),
                },
                Op::Move {
                    from: "fresh.md".into(),
                    to: "moved.md".into(),
                },
            ])
            .unwrap();

        assert_eq!(vault.read_note("old.md").unwrap(), "new content");
        assert_eq!(vault.read_note("moved.md").unwrap(), "created");
        assert!(!vault.exists("gone.md").unwrap());
        assert!(!vault.exists("fresh.md").unwrap());
        // delete outcome reports the trash location.
        assert!(
            matches!(&outcomes[2], OpOutcome::Deleted { trashed_to, .. } if trashed_to.starts_with(".md-mcp/trash/"))
        );
        // journal + backups cleaned up.
        assert!(
            vault
                .dir()
                .read_dir(JOURNAL_DIR)
                .map(|e| e.count())
                .unwrap_or(0)
                == 0
        );
    }

    #[test]
    fn a_failing_op_rolls_back_earlier_ops() {
        let (_d, vault) = temp_vault();
        vault.write_atomic("a.md", b"original").unwrap();

        // Second op fails (move from a missing source); the first write must undo.
        let err = vault
            .commit_batch(&[
                Op::Write {
                    path: "a.md".into(),
                    content: b"changed".to_vec(),
                },
                Op::Move {
                    from: "missing.md".into(),
                    to: "x.md".into(),
                },
            ])
            .unwrap_err();
        assert_eq!(err.code, crate::error::Code::Io);

        // a.md is back to its original content; nothing partially applied.
        assert_eq!(vault.read_note("a.md").unwrap(), "original");
        assert!(!vault.exists("x.md").unwrap());
    }

    #[test]
    fn create_op_rolls_back_by_deletion() {
        let (_d, vault) = temp_vault();
        let err = vault
            .commit_batch(&[
                Op::Write {
                    path: "created.md".into(),
                    content: b"x".to_vec(),
                },
                Op::Delete {
                    path: "missing.md".into(),
                },
            ])
            .unwrap_err();
        assert_eq!(err.code, crate::error::Code::NotFound);
        assert!(!vault.exists("created.md").unwrap());
    }

    #[test]
    fn recovery_rolls_back_an_incomplete_journal() {
        let dir = tempfile::tempdir().unwrap();
        {
            let vault = Vault::open(dir.path()).unwrap();
            vault
                .write_atomic("victim.md", b"should be removed")
                .unwrap();
        }
        // Plant an incomplete (uncommitted) journal whose undo deletes victim.md.
        let jdir = dir.path().join(".md-mcp/journal");
        std::fs::create_dir_all(&jdir).unwrap();
        std::fs::write(
            jdir.join("planted.json"),
            br#"{"batch_id":"planted","committed":false,"undo":[{"DeletePath":{"path":"victim.md"}}]}"#,
        )
        .unwrap();

        // Reopening triggers recovery.
        let vault = Vault::open(dir.path()).unwrap();
        assert!(
            !vault.exists("victim.md").unwrap(),
            "recovery should have rolled back"
        );
        assert!(!dir.path().join(".md-mcp/journal/planted.json").exists());
    }

    #[test]
    fn recovery_leaves_a_committed_journal_applied() {
        let dir = tempfile::tempdir().unwrap();
        {
            let vault = Vault::open(dir.path()).unwrap();
            vault.write_atomic("kept.md", b"survivor").unwrap();
        }
        let jdir = dir.path().join(".md-mcp/journal");
        std::fs::create_dir_all(&jdir).unwrap();
        // A committed journal must NOT be rolled back, only cleaned up.
        std::fs::write(
            jdir.join("done.json"),
            br#"{"batch_id":"done","committed":true,"undo":[{"DeletePath":{"path":"kept.md"}}]}"#,
        )
        .unwrap();

        let vault = Vault::open(dir.path()).unwrap();
        assert_eq!(vault.read_note("kept.md").unwrap(), "survivor");
        assert!(!dir.path().join(".md-mcp/journal/done.json").exists());
    }
}
