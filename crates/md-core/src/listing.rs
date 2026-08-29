//! Vault listing: a sorted, dot-directory-excluded walk for `list_notes`.
//!
//! Notes (`.md`) and, optionally, directories are returned in stable path order.
//! Hidden entries (names starting with `.`, including the internal `.md-mcp/`
//! state) are skipped. A glob filters notes only, relative to the start directory.

use std::time::UNIX_EPOCH;

use globset::GlobBuilder;

use crate::error::{Code, Error, Result};
use crate::vault::Vault;

/// Maximum bytes in a `list_notes` glob pattern. A real glob is a handful of
/// path components (`daily/**/*.md`); this cap is orders of magnitude above any
/// legitimate pattern yet well below the point where globset's lazy regex
/// compilation (`Glob::compile_matcher`, which `.expect()`s on a failed NFA
/// build) panics on a pathological literal — so an oversized glob fails as a
/// clean `TOO_LARGE` rejection instead of taking the request down.
const MAX_GLOB_BYTES: usize = 1024;

/// One listed entry: a note or a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Vault-relative path; directories end with `/`.
    pub path: String,
    pub is_dir: bool,
    /// File size in bytes; `None` for directories.
    pub size_bytes: Option<u64>,
    /// Last-modified time, ISO 8601 UTC.
    pub modified: String,
}

impl Vault {
    /// List notes (and optionally directories) under `directory`, sorted by path.
    ///
    /// `glob` (relative to `directory`) filters notes only. A `**` in the glob
    /// implies recursion. Hidden entries are excluded.
    pub fn list_entries(
        &self,
        directory: &str,
        recursive: bool,
        glob: Option<&str>,
        include_dirs: bool,
    ) -> Result<Vec<Entry>> {
        let base = directory.strip_suffix('/').unwrap_or(directory);
        if !base.is_empty() {
            Vault::validate_rel(base)?;
            // The internal state directory is never listed, even if named directly.
            if Vault::is_internal_path(base) || !self.exists(base)? {
                return Ok(Vec::new());
            }
        }

        let matcher = match glob {
            Some(pat) => {
                // Bound the pattern before handing it to globset: its matcher
                // compilation panics (not errors) on an over-long literal.
                if pat.len() > MAX_GLOB_BYTES {
                    return Err(Error::new(
                        Code::TooLarge,
                        format!(
                            "glob pattern exceeds {MAX_GLOB_BYTES} bytes: {} bytes",
                            pat.len()
                        ),
                    ));
                }
                Some(
                    GlobBuilder::new(pat)
                        .literal_separator(true)
                        .build()
                        .map_err(|e| Error::io(format!("invalid glob {pat}: {e}")))?
                        .compile_matcher(),
                )
            }
            None => None,
        };
        let recursive = recursive || glob.is_some_and(|g| g.contains("**"));

        let mut out = Vec::new();
        self.walk(base, recursive, include_dirs, &mut out)?;

        // Apply the glob to notes only, relative to the start directory.
        if let Some(m) = &matcher {
            let prefix = if base.is_empty() {
                String::new()
            } else {
                format!("{base}/")
            };
            out.retain(|e| {
                e.is_dir || {
                    let rel = e.path.strip_prefix(&prefix).unwrap_or(&e.path);
                    m.is_match(rel)
                }
            });
        }

        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn walk(
        &self,
        dir_rel: &str,
        recursive: bool,
        include_dirs: bool,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        let read = if dir_rel.is_empty() {
            self.dir().entries()
        } else {
            self.dir().read_dir(dir_rel)
        }
        .map_err(|e| Error::io(format!("read dir {dir_rel}: {e}")))?;

        for entry in read {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            let Ok(ft) = entry.file_type() else { continue };

            if ft.is_dir() {
                // Hide dot-directories only: the internal `.md-mcp/` state and
                // external tool dirs (.git/, .obsidian/). Dot-FILES such as
                // `.NET.md` are legitimate notes and stay listable.
                if name.starts_with('.') {
                    continue;
                }
                if include_dirs {
                    out.push(Entry {
                        path: format!("{rel}/"),
                        is_dir: true,
                        size_bytes: None,
                        modified: modified_iso(&entry),
                    });
                }
                if recursive {
                    self.walk(&rel, recursive, include_dirs, out)?;
                }
            } else if ft.is_file() && Vault::is_note_path(&name) {
                let size = entry.metadata().ok().map(|m| m.len());
                out.push(Entry {
                    path: rel,
                    is_dir: false,
                    size_bytes: size,
                    modified: modified_iso(&entry),
                });
            }
        }
        Ok(())
    }
}

fn modified_iso(entry: &cap_std::fs::DirEntry) -> String {
    let secs = entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.into_std().duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    iso8601_utc(secs.cast_signed())
}

/// Format Unix epoch seconds as an ISO 8601 UTC timestamp (no external crate).
fn iso8601_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_with(notes: &[(&str, &str)]) -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        for (p, b) in notes {
            vault.write_atomic(p, b.as_bytes()).unwrap();
        }
        (dir, vault)
    }

    fn paths(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn lists_notes_sorted_and_recursive() {
        let (_d, v) = vault_with(&[("b.md", "x"), ("a.md", "y"), ("sub/c.md", "z")]);
        let e = v.list_entries("", true, None, false).unwrap();
        assert_eq!(paths(&e), ["a.md", "b.md", "sub/c.md"]);
    }

    #[test]
    fn lists_dot_prefixed_note_files() {
        // Real-world notes like ".NET.md" or ".env.md" are legitimate and must
        // stay listable/searchable — only dot-DIRECTORIES (the internal
        // .md-mcp/ state, external tool dirs) are hidden, not dot-FILES.
        let (_d, v) = vault_with(&[(".NET.md", "x"), (".env.md", "y"), ("a.md", "z")]);
        let e = v.list_entries("", true, None, false).unwrap();
        assert_eq!(paths(&e), [".NET.md", ".env.md", "a.md"]);
    }

    #[test]
    fn skips_dot_directories_but_keeps_dot_files() {
        let (_d, v) = vault_with(&[(".hidden/secret.md", "x"), (".NET.md", "y"), ("a.md", "z")]);
        let e = v.list_entries("", true, None, false).unwrap();
        // .hidden/ directory excluded; the .NET.md dot-file remains.
        assert_eq!(paths(&e), [".NET.md", "a.md"]);
    }

    #[test]
    fn non_recursive_skips_subdirs() {
        let (_d, v) = vault_with(&[("a.md", "x"), ("sub/c.md", "z")]);
        let e = v.list_entries("", false, None, false).unwrap();
        assert_eq!(paths(&e), ["a.md"]);
    }

    #[test]
    fn include_dirs_marks_directories() {
        let (_d, v) = vault_with(&[("sub/c.md", "z")]);
        let e = v.list_entries("", true, None, true).unwrap();
        assert!(
            e.iter()
                .any(|x| x.path == "sub/" && x.is_dir && x.size_bytes.is_none())
        );
    }

    #[test]
    fn glob_filters_notes_with_literal_separator() {
        let (_d, v) = vault_with(&[("a.md", ""), ("sub/b.md", ""), ("daily/2026-01.md", "")]);
        // `*.md` must not cross a slash.
        let top = v.list_entries("", true, Some("*.md"), false).unwrap();
        assert_eq!(paths(&top), ["a.md"]);
        // `**/*.md` matches at any depth.
        let all = v.list_entries("", true, Some("**/*.md"), false).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn over_long_glob_rejected_cleanly_not_panic() {
        let (_d, v) = vault_with(&[("a.md", "")]);
        // A pattern past the cap must return TOO_LARGE, never reach globset's
        // matcher compilation (which panics on a pathological literal).
        let huge = "z".repeat(MAX_GLOB_BYTES + 1);
        let err = v.list_entries("", true, Some(&huge), false).unwrap_err();
        assert_eq!(err.code, Code::TooLarge);
        // A pattern at exactly the cap is still accepted.
        let at_cap = "z".repeat(MAX_GLOB_BYTES);
        assert!(v.list_entries("", true, Some(&at_cap), false).is_ok());
    }

    #[test]
    fn internal_state_dir_is_excluded() {
        let (_d, v) = vault_with(&[("a.md", "x")]);
        // Provoke a .md-mcp/ directory via a committed delete.
        v.commit_batch(&[crate::Op::Delete {
            path: "a.md".into(),
        }])
        .unwrap();
        v.write_atomic("b.md", b"y").unwrap();
        let e = v.list_entries("", true, None, true).unwrap();
        assert!(e.iter().all(|x| !x.path.starts_with(".md-mcp")));
    }

    #[test]
    fn internal_directory_listed_directly_is_empty() {
        let (_d, v) = vault_with(&[("a.md", "x")]);
        // Provoke .md-mcp/trash with a trashed .md note.
        v.commit_batch(&[crate::Op::Delete {
            path: "a.md".into(),
        }])
        .unwrap();
        // Explicitly listing the internal trash must not leak the trashed note.
        assert!(
            v.list_entries(".md-mcp/trash/", true, None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            v.list_entries(".md-mcp", true, None, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_directory_is_empty_not_error() {
        let (_d, v) = vault_with(&[("a.md", "x")]);
        assert!(
            v.list_entries("nope/", true, None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn iso8601_formats_known_epoch() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
