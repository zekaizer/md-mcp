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

/// A committed batch: its id and the per-op outcomes, in op order. The id is
/// the batch's identity in the event journal (ADR-0017).
#[derive(Clone, Debug)]
pub struct CommitReceipt {
    pub batch_id: String,
    pub outcomes: Vec<OpOutcome>,
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
    pub fn commit_batch(&self, ops: &[Op]) -> Result<CommitReceipt> {
        // Agent ops may never target the internal `.md-mcp/` state directory.
        for op in ops {
            let paths: Vec<&str> = match op {
                Op::Write { path, .. } | Op::Delete { path } => vec![path.as_str()],
                Op::Move { from, to } => vec![from.as_str(), to.as_str()],
            };
            for p in paths {
                if Self::is_internal_path(p) {
                    return Err(Error::traversal(format!(
                        "cannot target a protected directory: {p}"
                    )));
                }
            }
        }

        // Resolve op paths against the on-disk tree (NFC component matching),
        // so an NFC-spelled batch operates on notes stored in NFD. A trailing
        // '/' (directory convention) is preserved for the outcome echo.
        let resolve = |p: &String| -> Result<String> {
            let resolved = self.resolve_rel(strip_slash(p))?;
            Ok(if p.ends_with('/') {
                format!("{resolved}/")
            } else {
                resolved
            })
        };
        let ops: Vec<Op> = ops
            .iter()
            .map(|op| {
                Ok(match op {
                    Op::Write { path, content } => Op::Write {
                        path: resolve(path)?,
                        content: content.clone(),
                    },
                    Op::Delete { path } => Op::Delete {
                        path: resolve(path)?,
                    },
                    Op::Move { from, to } => {
                        let rfrom = resolve(from)?;
                        let mut rto = resolve(to)?;
                        // Same file, different requested spelling (e.g. an
                        // NFD-stored name renamed to its NFC form): resolving
                        // the leaf would collapse the move into a no-op, so
                        // keep the requested leaf and resolve only the parent.
                        if strip_slash(&rto) == strip_slash(&rfrom)
                            && strip_slash(to) != strip_slash(&rfrom)
                        {
                            let stripped = strip_slash(to);
                            let joined = match stripped.rfind('/') {
                                Some(i) => format!(
                                    "{}/{}",
                                    self.resolve_rel(&stripped[..i])?,
                                    &stripped[i + 1..]
                                ),
                                None => stripped.to_string(),
                            };
                            rto = if to.ends_with('/') {
                                format!("{joined}/")
                            } else {
                                joined
                            };
                        }
                        Op::Move {
                            from: rfrom,
                            to: rto,
                        }
                    }
                })
            })
            .collect::<Result<_>>()?;
        let ops = &ops[..];

        // Cross-process exclusion (ADR-0016): a cooperating external tool
        // holding the same flock never observes a mid-batch tree.
        let _flock = self.exclusive_lock()?;

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
                    // Only clear the journal/backups if the undo fully succeeded;
                    // otherwise leave them for recovery to retry.
                    if self.rollback(&journal) {
                        self.cleanup(&batch_id, &journal_path);
                    }
                    return Err(e);
                }
            }
        }

        // The commit point: flip the durable flag. If even that write fails, the
        // batch is uncommitted, so roll it back rather than leaving it applied.
        journal.committed = true;
        match self.write_journal(&journal_path, &journal) {
            Ok(()) => {
                self.cleanup(&batch_id, &journal_path);
                Ok(CommitReceipt { batch_id, outcomes })
            }
            Err(e) => {
                if self.rollback(&journal) {
                    self.cleanup(&batch_id, &journal_path);
                }
                Err(e)
            }
        }
    }

    /// Roll back any transaction a crash left incomplete. Called at open.
    pub(crate) fn recover_transactions(&self) -> Result<()> {
        let Ok(entries) = self.dir().read_dir(JOURNAL_DIR) else {
            return Ok(());
        };
        // Journals exist, so .md-mcp/ does too: recovery mutates the tree and
        // takes the same cross-process lock as a commit. (Skipped above for a
        // fresh vault, so a plain open never creates .md-mcp/.)
        let _flock = self.exclusive_lock()?;
        let mut journal_paths = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.ends_with(".json")
            {
                journal_paths.push(format!("{JOURNAL_DIR}/{name}"));
            }
        }
        let mut all_recovered = true;
        for jpath in journal_paths {
            let Ok(bytes) = self.dir().read(&jpath) else {
                continue;
            };
            let Ok(journal) = serde_json::from_slice::<Journal>(&bytes) else {
                continue;
            };
            if journal.committed {
                // Crash between commit and cleanup: just finish cleaning up.
                self.cleanup(&journal.batch_id, &jpath);
            } else if self.rollback(&journal) {
                self.cleanup(&journal.batch_id, &jpath);
            } else {
                // Incomplete rollback: keep the journal + backups for the next
                // attempt rather than opening a half-rolled-back vault as healthy.
                all_recovered = false;
            }
        }
        if all_recovered {
            Ok(())
        } else {
            Err(Error::io(
                "transaction recovery incomplete; some rollbacks could not finish",
            ))
        }
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
                // Defense-in-depth: a same-path move would back the target up
                // (removing the source) and then fail with a raw ENOENT.
                if from == to {
                    return Err(Error::conflict(format!(
                        "move source and destination are the same path: {from}"
                    )));
                }
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
            .map_err(|e| Error::io(format!("rename {from} -> {to}: {e}")))?;
        // Make both ends of the rename durable (the entry vanished from `from`
        // and appeared under `to`).
        self.fsync_parent(to);
        self.fsync_parent(from);
        Ok(())
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

    fn remove_path(&self, path: &str) -> std::io::Result<()> {
        match self.dir().symlink_metadata(path) {
            Ok(m) if m.is_dir() => self.dir().remove_dir_all(path),
            Ok(_) => self.dir().remove_file(path),
            Err(_) => Ok(()), // already gone
        }
    }

    /// Replay undo steps in reverse. Returns `true` only if every step that had
    /// work to do succeeded; on any failure the backups/journal are kept so the
    /// next open can retry rather than losing the original.
    fn rollback(&self, journal: &Journal) -> bool {
        let mut ok = true;
        for step in journal.undo.iter().rev() {
            let step_ok = match step {
                UndoStep::DeletePath { path } => self.remove_path(path).is_ok(),
                UndoStep::RestoreFromBackup { backup, path }
                | UndoStep::RestoreFromTrash {
                    trash: backup,
                    path,
                } => {
                    if self.dir().exists(backup) {
                        self.ensure_parent(path).is_ok()
                            && self.dir().rename(backup, self.dir(), path).is_ok()
                    } else {
                        true // nothing to restore (mutation never happened)
                    }
                }
                UndoStep::ReverseMove { from, to } => {
                    if self.dir().exists(to) && !self.dir().exists(from) {
                        self.ensure_parent(from).is_ok()
                            && self.dir().rename(to, self.dir(), from).is_ok()
                    } else {
                        true // the move did not happen, or was already reversed
                    }
                }
            };
            ok &= step_ok;
        }
        ok
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
    fn move_to_an_nfc_respelling_of_the_same_file_renames_it() {
        // An NFD-stored name renamed to its NFC form must change the on-disk
        // bytes instead of collapsing into a same-path move.
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let nfd = "\u{1112}\u{1161}\u{11ab}.md"; // 한.md in NFD
        let nfc = "\u{d55c}.md"; // 한.md in NFC
        assert_ne!(nfd, nfc);
        vault.write_atomic(nfd, b"content").unwrap();

        let outcomes = vault
            .commit_batch(&[Op::Move {
                from: nfd.to_string(),
                to: nfc.to_string(),
            }])
            .unwrap();
        assert!(matches!(&outcomes.outcomes[0], OpOutcome::Moved { to, .. } if to == nfc));
        // The on-disk byte spelling actually changed.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".md"))
            .collect();
        assert_eq!(names, vec![nfc.to_string()]);
        assert_eq!(vault.read_note(nfc).unwrap(), "content");
    }

    #[test]
    fn same_path_move_is_a_clean_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_atomic("a.md", b"x").unwrap();
        let e = vault
            .commit_batch(&[Op::Move {
                from: "a.md".to_string(),
                to: "a.md".to_string(),
            }])
            .unwrap_err();
        assert_eq!(e.code, crate::error::Code::Conflict);
        // Rolled back to no effect: the note is untouched.
        assert_eq!(vault.read_note("a.md").unwrap(), "x");
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
            matches!(&outcomes.outcomes[2], OpOutcome::Deleted { trashed_to, .. } if trashed_to.starts_with(".md-mcp/trash/"))
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
    fn commit_batch_refuses_internal_state_paths() {
        let (_d, vault) = temp_vault();
        vault.write_atomic("a.md", b"x").unwrap();
        // Provoke the internal dir.
        vault
            .commit_batch(&[Op::Delete {
                path: "a.md".into(),
            }])
            .unwrap();
        vault.write_atomic("b.md", b"y").unwrap();

        for op in [
            Op::Write {
                path: ".md-mcp/journal/x.json".into(),
                content: b"!".to_vec(),
            },
            Op::Delete {
                path: ".md-mcp/trash/a.md".into(),
            },
            Op::Move {
                from: "b.md".into(),
                to: ".md-mcp/evil.md".into(),
            },
        ] {
            let e = vault.commit_batch(&[op]).unwrap_err();
            assert_eq!(e.code, crate::error::Code::Traversal);
        }
        // b.md was not moved into the internal dir.
        assert!(vault.exists("b.md").unwrap());
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
    fn incomplete_rollback_keeps_journal_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        {
            let vault = Vault::open(dir.path()).unwrap();
            vault
                .write_atomic(".md-mcp/backup/x/0", b"original")
                .unwrap();
        }
        // A non-empty directory at the restore target blocks the undo rename.
        std::fs::create_dir_all(dir.path().join("target.md")).unwrap();
        std::fs::write(dir.path().join("target.md/inner"), b"x").unwrap();
        let jdir = dir.path().join(".md-mcp/journal");
        std::fs::create_dir_all(&jdir).unwrap();
        std::fs::write(
            jdir.join("stuck.json"),
            br#"{"batch_id":"stuck","committed":false,"undo":[{"RestoreFromBackup":{"backup":".md-mcp/backup/x/0","path":"target.md"}}]}"#,
        )
        .unwrap();

        // Recovery cannot finish the restore: open errors, journal + backup survive.
        assert!(Vault::open(dir.path()).is_err());
        assert!(dir.path().join(".md-mcp/journal/stuck.json").exists());
        assert!(dir.path().join(".md-mcp/backup/x/0").exists());
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
