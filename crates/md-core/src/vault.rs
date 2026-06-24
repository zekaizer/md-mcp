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

use crate::error::{Error, Result};

/// A capability-jailed handle to a vault root directory.
pub struct Vault {
    root: Dir,
    root_path: PathBuf,
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

    /// Validate a vault-relative path, returning a cleaned relative path.
    ///
    /// Rejects `..`, absolute paths, Windows prefixes, and the empty/root path.
    /// `.` components are dropped. This is defense-in-depth and clean error
    /// reporting; `cap-std` is the kernel backstop for live symlink escape.
    pub fn validate_rel(rel: &str) -> Result<String> {
        let mut clean = PathBuf::new();
        for comp in Path::new(rel).components() {
            match comp {
                Component::Normal(seg) => clean.push(seg),
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

    /// Whether a note or directory exists at `rel`.
    pub fn exists(&self, rel: &str) -> Result<bool> {
        let clean = Self::validate_rel(rel)?;
        Ok(self.root.exists(clean))
    }

    /// Read a note's raw text (as stored on disk, UTF-8).
    pub fn read_note(&self, rel: &str) -> Result<String> {
        let clean = Self::validate_rel(rel)?;
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
        let clean = Self::validate_rel(rel)?;
        let clean_path = Path::new(&clean);
        let name = clean_path
            .file_name()
            .ok_or_else(|| Error::traversal(format!("path has no file name: {rel}")))?;
        let parent = clean_path.parent().filter(|p| !p.as_os_str().is_empty());

        let owned_parent = match parent {
            Some(p) => {
                self.root
                    .create_dir_all(p)
                    .map_err(|e| Error::io(format!("mkdir {}: {e}", p.display())))?;
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
        Ok(())
    }

    /// Create a new note, refusing to overwrite an existing target unless
    /// `overwrite` is set. A pre-existing symlink (even dangling) counts as
    /// occupied and is refused.
    pub fn create_note(&self, rel: &str, bytes: &[u8], overwrite: bool) -> Result<()> {
        let clean = Self::validate_rel(rel)?;
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

    #[test]
    fn create_refuses_dangling_symlink_target() {
        let (vdir, vault) = temp_vault();
        // A dangling symlink occupies the name even though its target is absent.
        std::os::unix::fs::symlink("nowhere", vdir.path().join("dead.md")).unwrap();
        let e = vault.create_note("dead.md", b"x", false).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
    }
}
