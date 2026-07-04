//! The vault: a capability-jailed directory of notes.
//!
//! Every file operation goes through a [`cap_std`] `Dir` opened on the vault
//! root, so a path can never resolve outside it — even through a symlink or to a
//! not-yet-existing target ([ADR-0006](../../../docs/adr/0006-vault-path-jail-and-atomic-write.md)).
//! A lexical pre-check rejects `..`, absolute paths, and the vault root itself
//! with a precise [`Code::Traversal`] error before the `Dir` is touched.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_tempfile::TempFile;

use crate::error::{Code, Error, Result};
use crate::text::nfc;

/// The server's internal state directory, off-limits to agent note operations.
pub(crate) const INTERNAL_DIR: &str = ".md-mcp";

/// The git repository metadata directory. Off-limits like `.md-mcp/`: a note
/// operation writing into `.git/` could corrupt the repository
/// ([ADR-0016](../../../docs/adr/0016-git-sync-integration.md)).
const GIT_DIR: &str = ".git";

/// Directories no agent note operation may target.
const PROTECTED_DIRS: [&str; 2] = [INTERNAL_DIR, GIT_DIR];

/// The cross-process lock file, inside the internal state directory.
const LOCK_FILE: &str = ".md-mcp/lock";

/// Maximum bytes in one path component (file or directory name). Matches the
/// common filesystem `NAME_MAX`, so an over-long name is a validation error
/// rather than a raw `ENAMETOOLONG` at commit time.
const MAX_PATH_COMPONENT_BYTES: usize = 255;

/// A capability-jailed handle to a vault root directory.
pub struct Vault {
    root: Dir,
    root_path: PathBuf,
}

/// RAII guard for the cross-process vault lock; the OS lock is released when
/// the guard (and its file handle) drops.
pub struct VaultLock {
    _file: std::fs::File,
}

impl Vault {
    /// Open the vault at `path`. Fails if it does not exist or is not a
    /// directory. Rolls back any transaction left incomplete by a crash.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let root = Dir::open_ambient_dir(path, ambient_authority())
            .map_err(|e| Error::io(format!("cannot open vault {}: {e}", path.display())))?;
        let vault = Self {
            root,
            root_path: path.to_path_buf(),
        };
        vault.recover_transactions()?;
        Ok(vault)
    }

    /// The filesystem path the vault was opened at (for diagnostics).
    #[must_use]
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// The capability-jailed directory handle (crate-internal).
    pub(crate) fn dir(&self) -> &Dir {
        &self.root
    }

    /// Best-effort fsync of the parent directory of `rel`, so a `rename` that
    /// published `rel` survives a power loss. `rel` is already validated/jailed,
    /// so opening it under the (trusted) vault root is safe.
    pub(crate) fn fsync_parent(&self, rel: &str) {
        let parent = Path::new(rel)
            .parent()
            .filter(|p| !p.as_os_str().is_empty());
        let dir = parent.map_or_else(|| self.root_path.clone(), |p| self.root_path.join(p));
        let _ = std::fs::File::open(&dir).and_then(|f| f.sync_all());
    }

    /// Validate a vault-relative path, returning a cleaned relative path.
    ///
    /// Rejects `..`, absolute paths, Windows prefixes, and the empty/root path.
    /// `.` components are dropped. This is defense-in-depth and clean error
    /// reporting; `cap-std` is the kernel backstop for live symlink escape.
    pub fn validate_rel(rel: &str) -> Result<String> {
        let mut clean = PathBuf::new();
        for comp in Path::new(rel).components() {
            match comp {
                Component::Normal(seg) => {
                    // Reject an over-long component up front, so it fails as a
                    // clean validation error instead of a raw ENAMETOOLONG at
                    // commit time. 255 bytes is the common filesystem limit.
                    if seg.len() > MAX_PATH_COMPONENT_BYTES {
                        return Err(Error::new(
                            Code::Suffix,
                            format!(
                                "path component exceeds {MAX_PATH_COMPONENT_BYTES} bytes: {rel}"
                            ),
                        ));
                    }
                    clean.push(seg);
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(Error::traversal(format!("path contains '..': {rel}")));
                }
                Component::RootDir => {
                    return Err(Error::traversal(format!(
                        "absolute path not allowed: {rel}"
                    )));
                }
                Component::Prefix(_) => {
                    return Err(Error::traversal(format!("path prefix not allowed: {rel}")));
                }
            }
        }
        let clean = clean
            .to_str()
            .ok_or_else(|| Error::traversal(format!("non-UTF-8 path: {rel}")))?;
        if clean.is_empty() {
            return Err(Error::traversal(
                "path resolves to the vault root".to_string(),
            ));
        }
        Ok(clean.to_string())
    }

    /// Whether `rel` targets a protected directory — the server's internal
    /// state (`.md-mcp/`) or the git metadata (`.git/`) — which agent-facing
    /// note operations must not touch.
    #[must_use]
    pub fn is_internal_path(rel: &str) -> bool {
        match Self::validate_rel(rel) {
            Ok(clean) => PROTECTED_DIRS
                .iter()
                .any(|d| clean == *d || clean.starts_with(&format!("{d}/"))),
            Err(_) => false,
        }
    }

    /// Validate an agent-supplied note path: traversal-safe and not inside a
    /// protected directory (`.md-mcp/`, `.git/`).
    pub fn validate_note_rel(rel: &str) -> Result<String> {
        let clean = Self::validate_rel(rel)?;
        if Self::is_internal_path(&clean) {
            return Err(Error::traversal(format!(
                "cannot target a protected directory: {rel}"
            )));
        }
        Ok(clean)
    }

    /// Resolve a validated relative path against the on-disk tree, comparing
    /// path components after Unicode NFC normalization — so an NFC-spelled path
    /// finds a note a macOS client synced in NFD ([tool_spec §4]). An exact
    /// byte match always wins; a component with no match (e.g. a file being
    /// created) is kept as given. Returns the on-disk spelling.
    pub fn resolve_rel(&self, rel: &str) -> Result<String> {
        let clean = Self::validate_rel(rel)?;
        if self.root.exists(&clean) {
            return Ok(clean);
        }
        let mut resolved = String::new();
        for seg in clean.split('/') {
            let exact = if resolved.is_empty() {
                seg.to_string()
            } else {
                format!("{resolved}/{seg}")
            };
            if self.root.exists(&exact) {
                resolved = exact;
                continue;
            }
            let entries = if resolved.is_empty() {
                self.root.entries()
            } else {
                self.root.read_dir(&resolved)
            };
            let target = nfc(seg);
            let matched = entries.ok().and_then(|es| {
                let mut names: Vec<String> = es
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .filter(|n| nfc(n) == target)
                    .collect();
                names.sort();
                names.into_iter().next()
            });
            resolved = match matched {
                Some(name) if resolved.is_empty() => name,
                Some(name) => format!("{resolved}/{name}"),
                None => exact,
            };
        }
        Ok(resolved)
    }

    /// Whether a note or directory exists at `rel`.
    pub fn exists(&self, rel: &str) -> Result<bool> {
        let clean = self.resolve_rel(rel)?;
        Ok(self.root.exists(clean))
    }

    /// Whether `rel` exists and is a directory.
    pub fn is_dir(&self, rel: &str) -> Result<bool> {
        let clean = self.resolve_rel(rel)?;
        Ok(self
            .root
            .metadata(clean)
            .map(|m| m.is_dir())
            .unwrap_or(false))
    }

    /// Read a note's raw text (as stored on disk, UTF-8). The internal state
    /// directory is hidden — reading inside it reports the note as absent.
    pub fn read_note(&self, rel: &str) -> Result<String> {
        let clean = self.resolve_rel(rel)?;
        if Self::is_internal_path(rel) {
            return Err(Error::not_found(format!("note not found: {rel}")));
        }
        self.root
            .read_to_string(&clean)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Error::not_found(format!("note not found: {rel}")),
                _ => Error::io(format!("read {rel}: {e}")),
            })
    }

    /// Atomically write `bytes` to `rel`, creating parent directories as needed.
    ///
    /// Writes to a temp file in the target's parent directory, fsyncs it, then
    /// renames it over the target — so a reader sees the old or the complete new
    /// file, never a torn one.
    pub fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        // A trailing '/' denotes a directory (suffix convention); silently
        // stripping it would write a file where the caller expected a directory.
        if rel.ends_with('/') {
            return Err(Error::new(
                Code::Suffix,
                format!("a note path must not end with '/': {rel}"),
            ));
        }
        let clean = self.resolve_rel(rel)?;
        let clean_path = Path::new(&clean);
        let name = clean_path
            .file_name()
            .ok_or_else(|| Error::traversal(format!("path has no file name: {rel}")))?;
        let parent = clean_path.parent().filter(|p| !p.as_os_str().is_empty());

        let owned_parent = match parent {
            Some(p) => {
                self.root.create_dir_all(p).map_err(|e| {
                    // create_dir_all only reports these when an existing entry
                    // on the way is a file, not a directory.
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory
                    ) {
                        Error::conflict(format!(
                            "a parent of {rel} is occupied by a note, not a directory"
                        ))
                    } else {
                        Error::io(format!("mkdir {}: {e}", p.display()))
                    }
                })?;
                Some(
                    self.root
                        .open_dir(p)
                        .map_err(|e| Error::io(format!("open dir {}: {e}", p.display())))?,
                )
            }
            None => None,
        };
        let parent_dir = owned_parent.as_ref().unwrap_or(&self.root);

        let mut tmp = TempFile::new(parent_dir)
            .map_err(|e| Error::io(format!("create temp for {rel}: {e}")))?;
        tmp.write_all(bytes)
            .map_err(|e| Error::io(format!("write temp for {rel}: {e}")))?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| Error::io(format!("fsync temp for {rel}: {e}")))?;
        tmp.replace(name)
            .map_err(|e| Error::io(format!("commit {rel}: {e}")))?;
        self.fsync_parent(&clean);
        Ok(())
    }

    /// Acquire the exclusive cross-process vault lock (an OS lock on
    /// `.md-mcp/lock`), blocking until available; released on drop.
    ///
    /// Held around every transaction commit and every git operation
    /// ([ADR-0016](../../../docs/adr/0016-git-sync-integration.md)). External
    /// tools that mutate the vault (e.g. `flock <vault>/.md-mcp/lock git pull`)
    /// take the same lock, so a checkout can never interleave with a
    /// transaction.
    pub fn exclusive_lock(&self) -> Result<VaultLock> {
        self.root
            .create_dir_all(INTERNAL_DIR)
            .map_err(|e| Error::io(format!("create {INTERNAL_DIR}: {e}")))?;
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.create(true).write(true).read(true);
        let file = self
            .root
            .open_with(LOCK_FILE, &opts)
            .map_err(|e| Error::io(format!("open {LOCK_FILE}: {e}")))?
            .into_std();
        file.lock()
            .map_err(|e| Error::io(format!("lock {LOCK_FILE}: {e}")))?;
        Ok(VaultLock { _file: file })
    }

    /// Create a new note, refusing to overwrite an existing target unless
    /// `overwrite` is set. A pre-existing symlink (even dangling) counts as
    /// occupied and is refused.
    pub fn create_note(&self, rel: &str, bytes: &[u8], overwrite: bool) -> Result<()> {
        Self::validate_note_rel(rel)?;
        let clean = self.resolve_rel(rel)?;
        if !overwrite && self.root.symlink_metadata(&clean).is_ok() {
            return Err(Error::conflict(format!("note already exists: {rel}")));
        }
        self.write_atomic(rel, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;

    fn temp_vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        (dir, vault)
    }

    #[test]
    fn open_rejects_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(Vault::open(&missing).is_err());
    }

    // --- traversal corpus ---------------------------------------------------

    #[test]
    fn validate_rejects_parent_dir() {
        for p in ["..", "../x.md", "a/../../b.md", "a/../b.md"] {
            let e = Vault::validate_rel(p).unwrap_err();
            assert_eq!(e.code, Code::Traversal, "expected traversal for {p}");
        }
    }

    #[test]
    fn validate_rejects_absolute() {
        for p in ["/etc/passwd", "/a.md"] {
            assert_eq!(Vault::validate_rel(p).unwrap_err().code, Code::Traversal);
        }
    }

    #[test]
    fn validate_rejects_empty_and_dot_only() {
        for p in ["", ".", "./", "./."] {
            assert_eq!(
                Vault::validate_rel(p).unwrap_err().code,
                Code::Traversal,
                "expected traversal for {p:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_over_long_component() {
        let long = format!("{}.md", "a".repeat(300));
        let e = Vault::validate_rel(&long).unwrap_err();
        assert_eq!(e.code, Code::Suffix);
        // A component at exactly the limit is accepted.
        let ok = format!("{}.md", "a".repeat(MAX_PATH_COMPONENT_BYTES - 3));
        assert!(Vault::validate_rel(&ok).is_ok());
        // The limit is per-component, not per-path: many short segments pass.
        let deep = (0..300)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/")
            + "/n.md";
        assert!(Vault::validate_rel(&deep).is_ok());
    }

    #[test]
    fn validate_accepts_clean_paths_and_drops_curdir() {
        assert_eq!(Vault::validate_rel("a.md").unwrap(), "a.md");
        assert_eq!(
            Vault::validate_rel("daily/2026-06-25.md").unwrap(),
            "daily/2026-06-25.md"
        );
        assert_eq!(Vault::validate_rel("./a.md").unwrap(), "a.md");
        assert_eq!(Vault::validate_rel("a/./b.md").unwrap(), "a/b.md");
    }

    // --- symlink escape (cap-std kernel jail) -------------------------------

    #[test]
    fn read_through_symlinked_dir_to_outside_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"TOP SECRET").unwrap();
        let (vdir, vault) = temp_vault();
        // A symlink inside the vault pointing at the outside directory.
        std::os::unix::fs::symlink(outside.path(), vdir.path().join("escape")).unwrap();
        let got = vault.read_note("escape/secret.txt");
        assert!(
            got.is_err(),
            "cap-std must refuse to traverse the escaping symlink"
        );
        assert_ne!(got.ok(), Some("TOP SECRET".to_string()));
    }

    #[test]
    fn read_through_symlinked_file_to_outside_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"TOP SECRET").unwrap();
        let (vdir, vault) = temp_vault();
        std::os::unix::fs::symlink(&secret, vdir.path().join("escape.md")).unwrap();
        assert!(vault.read_note("escape.md").is_err());
    }

    #[test]
    fn write_through_traversal_path_is_refused() {
        let (_v, vault) = temp_vault();
        assert_eq!(
            vault.write_atomic("../escape.md", b"x").unwrap_err().code,
            Code::Traversal
        );
    }

    // --- atomic write -------------------------------------------------------

    #[test]
    fn write_then_read_roundtrips() {
        let (_v, vault) = temp_vault();
        vault.write_atomic("a.md", b"hello").unwrap();
        assert_eq!(vault.read_note("a.md").unwrap(), "hello");
    }

    #[test]
    fn write_auto_creates_parent_dirs() {
        let (_v, vault) = temp_vault();
        vault.write_atomic("daily/2026/x.md", b"deep").unwrap();
        assert_eq!(vault.read_note("daily/2026/x.md").unwrap(), "deep");
    }

    #[test]
    fn write_overwrites_atomically() {
        let (_v, vault) = temp_vault();
        vault.write_atomic("a.md", b"first").unwrap();
        vault.write_atomic("a.md", b"second").unwrap();
        assert_eq!(vault.read_note("a.md").unwrap(), "second");
    }

    #[test]
    fn written_file_is_readable_and_writable() {
        use std::os::unix::fs::PermissionsExt;
        let (vdir, vault) = temp_vault();
        vault.write_atomic("a.md", b"x").unwrap();
        let mode = std::fs::metadata(vdir.path().join("a.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_ne!(
            mode & 0o200,
            0,
            "owner-write bit must be set (mode {mode:o})"
        );
        assert_ne!(
            mode & 0o400,
            0,
            "owner-read bit must be set (mode {mode:o})"
        );
    }

    // --- create guard -------------------------------------------------------

    #[test]
    fn nfc_path_resolves_nfd_stored_note() {
        // A note synced from macOS is stored with NFD bytes; an agent-supplied
        // NFC path must still find it (tool_spec §4: NFC-normalized comparison).
        let (_v, vault) = temp_vault();
        let nfd = "\u{110b}\u{1161}/\u{1102}\u{1169}\u{1110}\u{1173}.md"; // 아/노트.md in NFD
        let nfc_path = "아/노트.md";
        assert_ne!(nfd, nfc_path);
        vault.write_atomic(nfd, b"content").unwrap();

        assert!(vault.exists(nfc_path).unwrap());
        assert_eq!(vault.read_note(nfc_path).unwrap(), "content");
        // The resolved spelling is the on-disk (NFD) one.
        assert_eq!(vault.resolve_rel(nfc_path).unwrap(), nfd);
        // Creating the NFC twin is a conflict, not a visually-identical duplicate.
        let e = vault.create_note(nfc_path, b"twin", false).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        // Writing through the NFC path edits the NFD file in place.
        vault.write_atomic(nfc_path, b"updated").unwrap();
        assert_eq!(vault.read_note(nfd).unwrap(), "updated");
    }

    #[test]
    fn commit_batch_resolves_nfc_paths() {
        let (_v, vault) = temp_vault();
        let nfd = "\u{1102}\u{1169}\u{1110}\u{1173}.md"; // 노트.md in NFD
        vault.write_atomic(nfd, b"x").unwrap();
        let outcomes = vault
            .commit_batch(&[crate::Op::Delete {
                path: "노트.md".to_string(),
            }])
            .unwrap();
        assert!(matches!(&outcomes[0], crate::OpOutcome::Deleted { path, .. } if path == nfd));
        assert!(!vault.exists(nfd).unwrap());
    }

    #[test]
    fn create_under_a_note_occupied_parent_is_a_conflict() {
        let (_v, vault) = temp_vault();
        vault.create_note("a.md", b"x", false).unwrap();
        for p in ["a.md/child.md", "a.md/deep/child.md"] {
            let e = vault.create_note(p, b"y", false).unwrap_err();
            assert_eq!(e.code, Code::Conflict, "expected CONFLICT for {p:?}: {e}");
        }
    }

    #[test]
    fn create_rejects_directory_suffix_path() {
        // `new/dir/` must not silently become a regular file named `dir`.
        let (_v, vault) = temp_vault();
        for p in ["new/dir/", "top/"] {
            let e = vault.create_note(p, b"x", false).unwrap_err();
            assert_eq!(e.code, Code::Suffix, "expected SUFFIX for {p:?}");
            assert!(
                !vault.exists(p).unwrap(),
                "nothing must be created for {p:?}"
            );
        }
    }

    #[test]
    fn create_refuses_existing_without_overwrite() {
        let (_v, vault) = temp_vault();
        vault.create_note("a.md", b"first", false).unwrap();
        let e = vault.create_note("a.md", b"second", false).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert_eq!(vault.read_note("a.md").unwrap(), "first");
    }

    #[test]
    fn create_overwrites_when_flagged() {
        let (_v, vault) = temp_vault();
        vault.create_note("a.md", b"first", false).unwrap();
        vault.create_note("a.md", b"second", true).unwrap();
        assert_eq!(vault.read_note("a.md").unwrap(), "second");
    }

    // --- internal state isolation -------------------------------------------

    #[test]
    fn internal_paths_are_recognized() {
        assert!(Vault::is_internal_path(".md-mcp"));
        assert!(Vault::is_internal_path(".md-mcp/journal/x.json"));
        assert!(Vault::is_internal_path("./.md-mcp/trash/a.md"));
        assert!(!Vault::is_internal_path("notes/.md-mcp.md"));
        assert!(!Vault::is_internal_path("a.md"));
        // .git/ is protected like .md-mcp/ (ADR-0016); look-alikes are not.
        assert!(Vault::is_internal_path(".git"));
        assert!(Vault::is_internal_path(".git/config"));
        assert!(Vault::is_internal_path("./.git/hooks/pre-commit.md"));
        assert!(!Vault::is_internal_path(".gitignore"));
        assert!(!Vault::is_internal_path("notes/.git.md"));
    }

    #[test]
    fn read_hides_internal_state() {
        let (_v, vault) = temp_vault();
        vault.write_atomic("a.md", b"x").unwrap();
        // Provoke a real .md-mcp/ via a committed delete, then a backup write.
        vault
            .commit_batch(&[crate::Op::Delete {
                path: "a.md".into(),
            }])
            .unwrap();
        let e = vault.read_note(".md-mcp/trash/a.md").unwrap_err();
        assert_eq!(e.code, Code::NotFound);
    }

    #[test]
    fn create_refuses_internal_path() {
        let (_v, vault) = temp_vault();
        let e = vault
            .create_note(".md-mcp/journal/evil.md", b"x", false)
            .unwrap_err();
        assert_eq!(e.code, Code::Traversal);
    }

    #[test]
    fn git_dir_is_write_protected_and_hidden() {
        let (vdir, vault) = temp_vault();
        std::fs::create_dir_all(vdir.path().join(".git")).unwrap();
        std::fs::write(vdir.path().join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        // No creation, no transaction op, no read inside .git/.
        let e = vault.create_note(".git/evil.md", b"x", false).unwrap_err();
        assert_eq!(e.code, Code::Traversal);
        let e = vault
            .commit_batch(&[crate::Op::Delete {
                path: ".git/HEAD".into(),
            }])
            .unwrap_err();
        assert_eq!(e.code, Code::Traversal);
        let e = vault.read_note(".git/HEAD").unwrap_err();
        assert_eq!(e.code, Code::NotFound);
    }

    // --- cross-process lock --------------------------------------------------

    #[test]
    fn exclusive_lock_excludes_other_holders_until_dropped() {
        let (vdir, vault) = temp_vault();
        let guard = vault.exclusive_lock().unwrap();
        // A second handle on the lock file (as an external tool would open it)
        // cannot take the lock while the guard lives.
        let other = std::fs::File::open(vdir.path().join(".md-mcp/lock")).unwrap();
        assert!(other.try_lock().is_err(), "lock must be held");
        drop(guard);
        assert!(other.try_lock().is_ok(), "lock must be released on drop");
    }

    #[test]
    fn create_refuses_dangling_symlink_target() {
        let (vdir, vault) = temp_vault();
        // A dangling symlink occupies the name even though its target is absent.
        std::os::unix::fs::symlink("nowhere", vdir.path().join("dead.md")).unwrap();
        let e = vault.create_note("dead.md", b"x", false).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
    }
}
