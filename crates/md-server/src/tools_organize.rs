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
use crate::envelope::ApiError;

const MAX_BATCH: usize = 100;

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

/// Whether `a` is an ancestor of (or equal to) `b`.
fn contains(a: &str, b: &str) -> bool {
    let a = strip_slash(a);
    let b = strip_slash(b);
    a == b || b.starts_with(&format!("{a}/"))
}

/// Whether two paths overlap (one contains the other).
fn overlap(a: &str, b: &str) -> bool {
    contains(a, b) || contains(b, a)
}

fn err(index: usize, code: Code, message: impl Into<String>) -> ApiError {
    ApiError {
        code: code.as_str().to_string(),
        message: message.into(),
        index: Some(index),
    }
}

fn too_many(n: usize) -> Option<ApiError> {
    (n > MAX_BATCH).then(|| ApiError {
        code: Code::BatchCollision.as_str().to_string(),
        message: format!("batch of {n} exceeds the limit of {MAX_BATCH}"),
        index: None,
    })
}

/// Reject in-batch source/dest collisions and source overlap among move pairs.
fn check_move_collisions(pairs: &[(usize, String, String)], errors: &mut Vec<ApiError>) {
    let n = pairs.len();
    let mut bad = vec![false; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let (_, fi, ti) = &pairs[i];
            let (_, fj, tj) = &pairs[j];
            if strip_slash(fi) == strip_slash(fj)
                || strip_slash(ti) == strip_slash(tj)
                || strip_slash(ti) == strip_slash(fj)
                || strip_slash(tj) == strip_slash(fi)
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
    pub moves: Vec<RelocateItem>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RelocateItem {
    pub source: String,
    /// Destination directory (ends with `/`); the source basename is kept.
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
    ) -> Result<Json<MoveResponse>, ErrorData> {
        let _guard = self.lock().write().await;
        Ok(Json(self.run_rename(&req.renames, req.overwrite)))
    }

    /// Move notes or directories into a destination directory. All-or-nothing.
    #[tool(
        description = "Relocate notes or directories into dest_dir (ends with /), keeping the basename; multiple items may target one directory. All-or-nothing: collisions (without overwrite), moving a directory into its own subtree, or in-batch overlaps reject the whole batch."
    )]
    pub async fn relocate_notes(
        &self,
        Parameters(req): Parameters<RelocateNotesRequest>,
    ) -> Result<Json<MoveResponse>, ErrorData> {
        let _guard = self.lock().write().await;
        Ok(Json(self.run_relocate(&req.moves, req.overwrite)))
    }
}

impl MdServer {
    fn run_delete(&self, paths: &[String]) -> DeleteNotesResponse {
        if let Some(e) = too_many(paths.len()) {
            return DeleteNotesResponse {
                ok: false,
                deleted: vec![],
                errors: vec![e],
            };
        }
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
                    .filter_map(|o| match o {
                        OpOutcome::Deleted { path, trashed_to } => {
                            Some(DeletedItem { path, trashed_to })
                        }
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
        if let Some(e) = too_many(renames.len()) {
            return MoveResponse {
                ok: false,
                moved: vec![],
                errors: vec![e],
            };
        }
        let mut errors = Vec::new();
        let mut pairs: Vec<(usize, String, String)> = Vec::new();

        for (i, r) in renames.iter().enumerate() {
            match self.compute_rename(r) {
                Ok(to) => {
                    if !overwrite
                        && strip_slash(&to) != strip_slash(&r.path)
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
        if let Some(e) = too_many(moves.len()) {
            return MoveResponse {
                ok: false,
                moved: vec![],
                errors: vec![e],
            };
        }
        let mut errors = Vec::new();
        let mut pairs: Vec<(usize, String, String)> = Vec::new();

        for (i, m) in moves.iter().enumerate() {
            match self.compute_relocate(m) {
                Ok(to) => {
                    if !overwrite && self.vault().exists(&to).unwrap_or(false) {
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
        Ok(to)
    }

    fn compute_relocate(&self, m: &RelocateItem) -> Result<String, Error> {
        Vault::validate_rel(&m.source)?;
        if !is_dir_path(&m.dest_dir) {
            return Err(Error::new(Code::DestNotDir, "dest_dir must end with '/'"));
        }
        let is_dir = is_dir_path(&m.source);
        let to = format!(
            "{}{}{}",
            m.dest_dir,
            basename(&m.source),
            if is_dir { "/" } else { "" }
        );
        Vault::validate_rel(&to)?;
        // A directory cannot move into its own subtree.
        if is_dir && contains(&m.source, &m.dest_dir) {
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
                    .filter_map(|o| match o {
                        OpOutcome::Moved { from, to } => Some(MovedItem { from, to }),
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
        assert_eq!(ok.moved[0].to, "renamed.md");
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
