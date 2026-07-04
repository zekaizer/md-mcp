//! Organization tools: rename_notes, relocate_notes, delete_notes.
//!
//! All three are destructive and all-or-nothing: the batch is validated for path
//! safety, suffix rules, collisions, and overlap before any move, then applied
//! through the transaction engine
//! ([ADR-0009](../../../docs/adr/0009-delete-recovery-and-move-validation.md)).

use md_core::{Code, Error, Op, OpOutcome, Vault};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::MdServer;
use crate::envelope::{ApiError, batch_limit};

// --- path helpers -----------------------------------------------------------

fn strip_slash(p: &str) -> &str {
    p.strip_suffix('/').unwrap_or(p)
}

fn is_dir_path(p: &str) -> bool {
    p.ends_with('/')
}

fn basename(p: &str) -> &str {
    let p = strip_slash(p);
    p.rsplit('/').next().unwrap_or(p)
}

fn parent_dir(p: &str) -> String {
    let p = strip_slash(p);
    match p.rfind('/') {
        Some(i) => p[..=i].to_string(),
        None => String::new(),
    }
}

/// A path's comparison key: slash-stripped and NFC-normalized (spec §4 — a
/// batch mixing NFD and NFC spellings of one path must not slip past
/// duplicate/overlap detection to fail confusingly at commit time).
fn nfc_key(p: &str) -> String {
    md_core::text::nfc(strip_slash(p)).into_owned()
}

/// Whether `a` is an ancestor of (or equal to) `b`.
fn contains(a: &str, b: &str) -> bool {
    let a = nfc_key(a);
    let b = nfc_key(b);
    a == b || b.starts_with(&format!("{a}/"))
}

/// Whether two paths overlap (one contains the other).
fn overlap(a: &str, b: &str) -> bool {
    contains(a, b) || contains(b, a)
}

/// Echo an on-disk path with the directory suffix convention: when the caller
/// addressed a directory (trailing `/`), the echoed path ends with `/` too, so
/// it can be fed straight back into dest_dir/path arguments.
fn with_dir_suffix(actual: String, requested: &str) -> String {
    if is_dir_path(requested) && !actual.ends_with('/') {
        format!("{actual}/")
    } else {
        actual
    }
}

fn err(index: usize, code: Code, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.as_str().to_string(),
        message: message.into(),
        index: Some(index),
    }
}

/// Reject in-batch source/dest collisions and source overlap among move pairs.
fn check_move_collisions(pairs: &[(usize, String, String)], errors: &mut Vec<ApiError>) {
    let n = pairs.len();
    let mut bad = vec![false; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let (_, fi, ti) = &pairs[i];
            let (_, fj, tj) = &pairs[j];
            if nfc_key(fi) == nfc_key(fj)
                || nfc_key(ti) == nfc_key(tj)
                || nfc_key(ti) == nfc_key(fj)
                || nfc_key(tj) == nfc_key(fi)
                || overlap(fi, fj)
            {
                bad[i] = true;
                bad[j] = true;
            }
        }
    }
    for (k, &b) in bad.iter().enumerate() {
        if b {
            errors.push(err(
                pairs[k].0,
                Code::BatchCollision,
                "item collides with another move in the batch (duplicate, swap, or overlap)",
            ));
        }
    }
}

// --- requests / responses ---------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DeleteNotesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DeleteNotesResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<DeletedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DeletedItem {
    pub path: String,
    pub trashed_to: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RenameNotesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub renames: Vec<RenameItem>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RenameItem {
    pub path: String,
    /// New basename (with extension for notes); no slash — use relocate to move.
    pub new_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RelocateNotesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub moves: Vec<RelocateItem>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RelocateItem {
    pub source: String,
    /// Destination directory (ends with `/`; `/` alone is the vault root);
    /// the source basename is kept.
    pub dest_dir: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub moved: Vec<MovedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RenameResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub renamed: Vec<MovedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MovedItem {
    pub from: String,
    pub to: String,
}

#[tool_router(router = organize_router, vis = "pub(crate)")]
impl MdServer {
    /// Delete notes or directories (to a recoverable trash). All-or-nothing.
    #[tool(
        description = "Delete notes (or directories ending in /) to a recoverable trash. All-or-nothing: a missing path, the vault root, or two overlapping targets reject the whole batch and nothing is deleted. Returns each item's trash location."
    )]
    pub async fn delete_notes(
        &self,
        Parameters(req): Parameters<DeleteNotesRequest>,
    ) -> Result<Json<DeleteNotesResponse>, ErrorData> {
        batch_limit(req.paths.len())?;
        let _guard = self.lock().write().await;
        Ok(Json(self.run_delete(&req.paths)))
    }

    /// Rename a note or directory in place (same parent). All-or-nothing.
    #[tool(
        description = "Rename notes or directories in place (same parent, basename only; new_name must not contain '/'). A note keeps its .md extension. All-or-nothing: collisions (without overwrite) or in-batch swaps reject the whole batch."
    )]
    pub async fn rename_notes(
        &self,
        Parameters(req): Parameters<RenameNotesRequest>,
    ) -> Result<Json<RenameResponse>, ErrorData> {
        batch_limit(req.renames.len())?;
        let _guard = self.lock().write().await;
        let r = self.run_rename(&req.renames, req.overwrite);
        Ok(Json(RenameResponse {
            ok: r.ok,
            renamed: r.moved,
            errors: r.errors,
        }))
    }

    /// Move notes or directories into a destination directory. All-or-nothing.
    #[tool(
        description = "Relocate notes or directories into dest_dir (ends with /), keeping the basename; multiple items may target one directory. All-or-nothing: collisions (without overwrite), moving a directory into its own subtree, or in-batch overlaps reject the whole batch."
    )]
    pub async fn relocate_notes(
        &self,
        Parameters(req): Parameters<RelocateNotesRequest>,
    ) -> Result<Json<MoveResponse>, ErrorData> {
        batch_limit(req.moves.len())?;
        let _guard = self.lock().write().await;
        Ok(Json(self.run_relocate(&req.moves, req.overwrite)))
    }
}

impl MdServer {
    fn run_delete(&self, paths: &[String]) -> DeleteNotesResponse {
        let mut errors = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            if let Err(e) = Vault::validate_rel(p) {
                errors.push(ApiError::at(i, &e));
                continue;
            }
            if !self.vault().exists(p).unwrap_or(false) {
                errors.push(err(i, Code::NotFound, format!("path not found: {p}")));
            }
        }
        for i in 0..paths.len() {
            for j in (i + 1)..paths.len() {
                if overlap(&paths[i], &paths[j]) {
                    errors.push(err(
                        i,
                        Code::Overlap,
                        "path overlaps another delete in the batch",
                    ));
                    errors.push(err(
                        j,
                        Code::Overlap,
                        "path overlaps another delete in the batch",
                    ));
                }
            }
        }
        if !errors.is_empty() {
            errors.sort_by_key(|e| e.index);
            return DeleteNotesResponse {
                ok: false,
                deleted: vec![],
                errors,
            };
        }

        let ops: Vec<Op> = paths
            .iter()
            .map(|p| Op::Delete { path: p.clone() })
            .collect();
        match self.vault().commit_batch(&ops) {
            Ok(outcomes) => {
                let deleted = outcomes
                    .into_iter()
                    .zip(paths)
                    .filter_map(|(o, requested)| match o {
                        OpOutcome::Deleted { path, trashed_to } => Some(DeletedItem {
                            path: with_dir_suffix(path, requested),
                            trashed_to,
                        }),
                        _ => None,
                    })
                    .collect();
                DeleteNotesResponse {
                    ok: true,
                    deleted,
                    errors: vec![],
                }
            }
            Err(e) => DeleteNotesResponse {
                ok: false,
                deleted: vec![],
                errors: vec![ApiError::from_core(&e)],
            },
        }
    }

    fn run_rename(&self, renames: &[RenameItem], overwrite: bool) -> MoveResponse {
        let mut errors = Vec::new();
        let mut pairs: Vec<(usize, String, String)> = Vec::new();

        for (i, r) in renames.iter().enumerate() {
            match self.compute_rename(r) {
                Ok(to) => {
                    // "Occupied" must mean a *different* file: a destination
                    // that resolves to the source itself is a Unicode
                    // respelling of the same note (e.g. NFD -> NFC), not a
                    // collision.
                    let same_file = matches!(
                        (
                            self.vault().resolve_rel(strip_slash(&to)),
                            self.vault().resolve_rel(strip_slash(&r.path)),
                        ),
                        (Ok(a), Ok(b)) if a == b
                    );
                    if !overwrite
                        && strip_slash(&to) != strip_slash(&r.path)
                        && !same_file
                        && self.vault().exists(&to).unwrap_or(false)
                    {
                        errors.push(err(
                            i,
                            Code::Conflict,
                            format!("destination already exists: {to}"),
                        ));
                    }
                    pairs.push((i, r.path.clone(), to));
                }
                Err(e) => errors.push(ApiError::at(i, &e)),
            }
        }
        self.finish_move(pairs, errors)
    }

    fn run_relocate(&self, moves: &[RelocateItem], overwrite: bool) -> MoveResponse {
        let mut errors = Vec::new();
        let mut pairs: Vec<(usize, String, String)> = Vec::new();

        for (i, m) in moves.iter().enumerate() {
            match self.compute_relocate(m) {
                Ok(to) => {
                    if strip_slash(&to) == strip_slash(&m.source) {
                        // A no-op move reads as "already exists" otherwise,
                        // suggesting a different file is in the way.
                        errors.push(err(
                            i,
                            Code::Conflict,
                            format!("source is already in dest_dir: {}", m.source),
                        ));
                    } else if !overwrite && self.vault().exists(&to).unwrap_or(false) {
                        errors.push(err(
                            i,
                            Code::Conflict,
                            format!("destination already exists: {to}"),
                        ));
                    }
                    pairs.push((i, m.source.clone(), to));
                }
                Err(e) => errors.push(ApiError::at(i, &e)),
            }
        }
        self.finish_move(pairs, errors)
    }

    fn compute_rename(&self, r: &RenameItem) -> Result<String, Error> {
        Vault::validate_rel(&r.path)?;
        if !self.vault().exists(&r.path).unwrap_or(false) {
            return Err(Error::not_found(format!("source not found: {}", r.path)));
        }
        if r.new_name.contains('/') {
            return Err(Error::new(
                Code::Suffix,
                "new_name must not contain '/'; use relocate to move",
            ));
        }
        let is_dir = is_dir_path(&r.path);
        if is_dir {
            if r.new_name.ends_with(".md") {
                return Err(Error::new(
                    Code::Suffix,
                    "a directory's new_name must not end with .md",
                ));
            }
        } else if !r.new_name.ends_with(".md") {
            return Err(Error::new(
                Code::Suffix,
                "a note's new_name must end with .md",
            ));
        }
        let to = format!(
            "{}{}{}",
            parent_dir(&r.path),
            r.new_name,
            if is_dir { "/" } else { "" }
        );
        Vault::validate_rel(&to)?;
        // A no-op rename would otherwise reach the commit stage, where backing
        // up the destination removes the source and a raw IO error leaks out.
        // Bytewise on purpose: an NFC respelling of the same name is a
        // legitimate rename, not a no-op.
        if strip_slash(&to) == strip_slash(&r.path) {
            return Err(Error::new(
                Code::Conflict,
                format!("new_name is already the current name: {}", r.path),
            ));
        }
        Ok(to)
    }

    fn compute_relocate(&self, m: &RelocateItem) -> Result<String, Error> {
        Vault::validate_rel(&m.source)?;
        if !self.vault().exists(&m.source).unwrap_or(false) {
            return Err(Error::not_found(format!("source not found: {}", m.source)));
        }
        // "/" is the suffix convention's spelling of the vault root — without
        // it there is no way to relocate a note back to the top level.
        let dest_prefix: &str = if m.dest_dir == "/" {
            ""
        } else if is_dir_path(&m.dest_dir) {
            &m.dest_dir
        } else {
            return Err(Error::new(Code::DestNotDir, "dest_dir must end with '/'"));
        };
        // Reject if dest_dir, or any of its existing ancestors, is occupied by a
        // non-directory — so the failure is an indexed validation rejection, not
        // a late create_dir_all IO error at commit time.
        let dest = strip_slash(dest_prefix);
        let mut prefix = String::new();
        for seg in dest.split('/').filter(|s| !s.is_empty()) {
            prefix = if prefix.is_empty() {
                seg.to_string()
            } else {
                format!("{prefix}/{seg}")
            };
            if self.vault().exists(&prefix).unwrap_or(false)
                && !self.vault().is_dir(&prefix).unwrap_or(false)
            {
                return Err(Error::new(
                    Code::DestNotDir,
                    format!("dest_dir path is occupied by a non-directory: {prefix}"),
                ));
            }
        }

        let is_dir = is_dir_path(&m.source);
        let to = format!(
            "{}{}{}",
            dest_prefix,
            basename(&m.source),
            if is_dir { "/" } else { "" }
        );
        Vault::validate_rel(&to)?;
        // A directory cannot move into its own subtree.
        if is_dir && !dest_prefix.is_empty() && contains(&m.source, dest_prefix) {
            return Err(Error::new(
                Code::Overlap,
                "cannot move a directory into its own subtree",
            ));
        }
        Ok(to)
    }

    fn finish_move(
        &self,
        pairs: Vec<(usize, String, String)>,
        mut errors: Vec<ApiError>,
    ) -> MoveResponse {
        check_move_collisions(&pairs, &mut errors);
        if !errors.is_empty() {
            errors.sort_by_key(|e| e.index);
            return MoveResponse {
                ok: false,
                moved: vec![],
                errors,
            };
        }
        let ops: Vec<Op> = pairs
            .iter()
            .map(|(_, from, to)| Op::Move {
                from: from.clone(),
                to: to.clone(),
            })
            .collect();
        match self.vault().commit_batch(&ops) {
            Ok(outcomes) => {
                let moved = outcomes
                    .into_iter()
                    .zip(&pairs)
                    .filter_map(|(o, (_, pfrom, pto))| match o {
                        OpOutcome::Moved { from, to } => Some(MovedItem {
                            from: with_dir_suffix(from, pfrom),
                            to: with_dir_suffix(to, pto),
                        }),
                        _ => None,
                    })
                    .collect();
                MoveResponse {
                    ok: true,
                    moved,
                    errors: vec![],
                }
            }
            Err(e) => MoveResponse {
                ok: false,
                moved: vec![],
                errors: vec![ApiError::from_core(&e)],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::wrapper::Parameters;

    fn server(notes: &[(&str, &str)]) -> (tempfile::TempDir, MdServer) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        for (p, b) in notes {
            vault.write_atomic(p, b.as_bytes()).unwrap();
        }
        (dir, MdServer::new(vault))
    }

    #[tokio::test]
    async fn delete_to_trash_all_or_nothing() {
        let (_d, s) = server(&[("a.md", "x"), ("b.md", "y")]);
        // A missing path rejects the whole batch.
        let bad = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["a.md".into(), "missing.md".into()],
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert!(s.vault().exists("a.md").unwrap());

        let ok = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["a.md".into()],
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok);
        assert!(ok.deleted[0].trashed_to.starts_with(".md-mcp/trash/"));
        assert!(!s.vault().exists("a.md").unwrap());
    }

    #[tokio::test]
    async fn relocate_to_the_vault_root_via_slash() {
        let (_d, s) = server(&[("deep/n.md", "x")]);
        let ok = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![RelocateItem {
                    source: "deep/n.md".into(),
                    dest_dir: "/".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.moved[0].to, "n.md");
        assert!(s.vault().exists("n.md").unwrap());

        // A directory also relocates to the root; a top-level dir is a no-op.
        let ok = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![RelocateItem {
                    source: "deep/".into(),
                    dest_dir: "/".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!ok.ok, "top-level dir into root is a no-op");
        assert!(ok.errors[0].message.contains("already in dest_dir"));
    }

    #[tokio::test]
    async fn mixed_spelling_batches_are_caught_at_validation() {
        // NFD and NFC spellings of one path must not slip past overlap and
        // duplicate detection to fail confusingly at commit time.
        let nfd_dir = "\u{1112}\u{1161}\u{11ab}"; // 한 in NFD
        let file = format!("{nfd_dir}/c.md");
        let (_d, s) = server(&[(file.as_str(), "x")]);

        let bad = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec![format!("{nfd_dir}/"), "\u{d55c}/c.md".into()],
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "OVERLAP", "{:?}", bad.errors);
        assert!(s.vault().exists(&file).unwrap(), "nothing deleted");

        let bad = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![
                    RenameItem {
                        path: file.clone(),
                        new_name: "d.md".into(),
                    },
                    RenameItem {
                        path: "\u{d55c}/c.md".into(),
                        new_name: "e.md".into(),
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert!(
            bad.errors.iter().any(|e| e.code == "BATCH_COLLISION"),
            "{:?}",
            bad.errors
        );
    }

    #[tokio::test]
    async fn rename_fixes_unicode_spelling_of_the_same_note() {
        // NFD -> NFC and back: same text, different bytes; must rename the
        // on-disk spelling without overwrite and without a false CONFLICT.
        let nfd = "\u{1112}\u{1161}\u{11ab}.md"; // 한.md in NFD
        let nfc = "\u{d55c}.md"; // 한.md in NFC
        let (_d, s) = server(&[(nfd, "content")]);

        let ok = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: nfd.into(),
                    new_name: nfc.into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.renamed[0].to, nfc);
        assert_eq!(s.vault().resolve_rel(nfc).unwrap(), nfc, "on-disk bytes are NFC now");

        // And back to NFD.
        let ok = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: nfc.into(),
                    new_name: nfd.into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(s.vault().resolve_rel(nfc).unwrap(), nfd);
        assert_eq!(s.vault().read_note(nfc).unwrap(), "content");
    }

    #[tokio::test]
    async fn noop_rename_is_an_indexed_validation_error() {
        // Renaming to the current name used to fail at commit time with a raw
        // un-indexed IO error (after a backup/rollback round-trip).
        let (_d, s) = server(&[("a.md", "x")]);
        let bad = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: "a.md".into(),
                    new_name: "a.md".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "CONFLICT");
        assert_eq!(bad.errors[0].index, Some(0));
        assert!(bad.errors[0].message.contains("current name"));
        assert!(s.vault().exists("a.md").unwrap());
    }

    #[tokio::test]
    async fn noop_relocate_names_the_actual_problem() {
        let (_d, s) = server(&[("d/n.md", "x")]);
        let bad = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![RelocateItem {
                    source: "d/n.md".into(),
                    dest_dir: "d/".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert!(
            bad.errors[0].message.contains("already in dest_dir"),
            "message: {}",
            bad.errors[0].message
        );
    }

    #[tokio::test]
    async fn dir_echoes_keep_the_trailing_slash() {
        // Echoed directory paths follow the suffix convention so they can be
        // fed straight back into path/dest_dir arguments.
        let (_d, s) = server(&[("d/n.md", "x")]);
        let ok = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: "d/".into(),
                    new_name: "e".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(ok.renamed[0].from, "d/");
        assert_eq!(ok.renamed[0].to, "e/");

        let ok = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![RelocateItem {
                    source: "e/".into(),
                    dest_dir: "arch/".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(ok.moved[0].to, "arch/e/");

        let ok = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["arch/e/".into()],
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(ok.deleted[0].path, "arch/e/");
    }

    #[tokio::test]
    async fn rename_in_place_with_suffix_rules() {
        let (_d, s) = server(&[("note.md", "x")]);
        let ok = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: "note.md".into(),
                    new_name: "renamed.md".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok);
        assert_eq!(ok.renamed[0].to, "renamed.md");
        assert!(s.vault().exists("renamed.md").unwrap());

        // A slash in new_name is rejected.
        let bad = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: "renamed.md".into(),
                    new_name: "sub/x.md".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(bad.errors[0].code, "SUFFIX");
    }

    #[tokio::test]
    async fn relocate_into_directory() {
        let (_d, s) = server(&[("a.md", "x")]);
        let ok = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![RelocateItem {
                    source: "a.md".into(),
                    dest_dir: "archive/".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok);
        assert_eq!(ok.moved[0].to, "archive/a.md");
        assert!(s.vault().exists("archive/a.md").unwrap());
    }

    #[tokio::test]
    async fn missing_source_is_an_indexed_not_found() {
        // A missing source must fail validation as NOT_FOUND with the item's
        // index — not leak a raw IO rename error from the commit stage.
        let (_d, s) = server(&[("a.md", "x")]);
        let bad = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![
                    RelocateItem {
                        source: "a.md".into(),
                        dest_dir: "archive/".into(),
                    },
                    RelocateItem {
                        source: "ghost.md".into(),
                        dest_dir: "archive/".into(),
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "NOT_FOUND");
        assert_eq!(bad.errors[0].index, Some(1));
        assert!(s.vault().exists("a.md").unwrap(), "all-or-nothing: a.md stays");

        let bad = s
            .rename_notes(Parameters(RenameNotesRequest {
                renames: vec![RenameItem {
                    path: "ghost.md".into(),
                    new_name: "still-ghost.md".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "NOT_FOUND");
        assert_eq!(bad.errors[0].index, Some(0));
    }

    #[tokio::test]
    async fn relocate_into_a_file_occupied_dest_is_rejected_with_index() {
        let (_d, s) = server(&[("a.md", "x"), ("archive", "i am a file")]);
        let resp = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![RelocateItem {
                    source: "a.md".into(),
                    dest_dir: "archive/".into(),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!resp.ok);
        assert_eq!(resp.errors[0].code, "DEST_NOT_DIR");
        assert_eq!(resp.errors[0].index, Some(0));
        assert!(s.vault().exists("a.md").unwrap());
    }

    #[tokio::test]
    async fn relocate_duplicate_destination_is_rejected() {
        // Two items whose basenames collide into one directory both map to
        // d/a.md (neither exists yet) -> an in-batch collision.
        let (_d, s) = server(&[("a.md", "x"), ("sub/a.md", "y")]);
        let collide = s
            .relocate_notes(Parameters(RelocateNotesRequest {
                moves: vec![
                    RelocateItem {
                        source: "a.md".into(),
                        dest_dir: "d/".into(),
                    },
                    RelocateItem {
                        source: "sub/a.md".into(),
                        dest_dir: "d/".into(),
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!collide.ok);
        assert!(collide.errors.iter().all(|e| e.code == "BATCH_COLLISION"));
    }
}
