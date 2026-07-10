//! Organization tools: move_notes, delete_notes.
//!
//! Both are destructive and all-or-nothing: the batch is validated for path
//! safety, suffix rules, collisions, and overlap before any move, then applied
//! through the transaction engine
//! ([ADR-0024](../../../docs/adr/0024-unified-move-primitive.md)).

use md_core::{Code, Error, Op, OpOutcome, Vault};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::MdServer;
use crate::envelope::{ApiError, batch_limit};
use crate::events::EventOp;

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

/// Whether `a` strictly contains `b` (b is inside a's subtree, not equal).
fn contains_strict(a: &str, b: &str) -> bool {
    nfc_key(b).starts_with(&format!("{}/", nfc_key(a)))
}

/// Echo an on-disk path with the directory suffix convention: when the caller
/// addressed a directory (trailing `/`), the echoed path ends with `/` too, so
/// it can be fed straight back into dest/path arguments.
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
///
/// A destination strictly inside another item's *destination* subtree is NOT a
/// collision: the batch declares a final tree and the engine applies ancestor
/// destinations first (ADR-0024). A destination inside another item's *source*
/// subtree — a path the batch vacates — stays rejected as order-sensitive in a
/// confusing way.
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
                || contains_strict(fi, tj)
                || contains_strict(fj, ti)
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
    /// Validate and report what would happen without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// After the batch, remove source directories it left empty (ascending).
    #[serde(default)]
    pub prune_empty: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DeleteNotesResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<DeletedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
    /// Directories removed because the batch emptied them (prune_empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pruned: Vec<String>,
    /// True when this was a dry run: nothing was written.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Present while git sync is failing (ADR-0019); see sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DeletedItem {
    pub path: String,
    pub trashed_to: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveNotesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub moves: Vec<MoveItem>,
    #[serde(default)]
    pub overwrite: bool,
    /// Validate and report what would happen without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// After the batch, remove source directories it left empty (ascending).
    #[serde(default)]
    pub prune_empty: bool,
    /// Also rewrite standard-Markdown links so they keep pointing at the
    /// moved notes (ADR-0022). Atomic with the batch; wikilinks untouched.
    #[serde(default)]
    pub update_links: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveItem {
    pub source: String,
    /// Destination (ADR-0024): ending in `/` it names a directory to move
    /// into, keeping the source basename (`/` alone is the vault root);
    /// otherwise it is the full destination path including the new basename
    /// (a note keeps `.md`), renaming and moving in one step.
    pub dest: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub moved: Vec<MovedItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
    /// Directories removed because the batch emptied them (prune_empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pruned: Vec<String>,
    /// Notes whose links were rewritten for this batch (update_links),
    /// reported at their post-batch paths.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relinked: Vec<String>,
    /// True when this was a dry run: nothing was written.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Present while git sync is failing (ADR-0019); see sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MovedItem {
    pub from: String,
    pub to: String,
}

/// The per-batch behavior flags for a move batch.
struct MoveOpts {
    dry_run: bool,
    prune_empty: bool,
    update_links: bool,
}

#[tool_router(router = organize_router, vis = "pub(crate)")]
impl MdServer {
    /// Delete notes or directories (to a recoverable trash). All-or-nothing.
    #[tool(
        description = "Delete notes (or directories ending in /) to a recoverable trash. All-or-nothing: a missing path, the vault root, or two overlapping targets reject the whole batch and nothing is deleted. Returns each item's trash location. dry_run:true validates and returns the planned outcome (or every rejection) without deleting. prune_empty:true also removes source directories the batch left empty (reported as pruned).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn delete_notes(
        &self,
        Parameters(req): Parameters<DeleteNotesRequest>,
    ) -> Result<Json<DeleteNotesResponse>, ErrorData> {
        batch_limit(req.paths.len())?;
        let _guard = self.lock().write().await;
        let mut r = self
            .run_delete(&req.paths, req.dry_run, req.prune_empty)
            .await;
        r.sync_warning = self.sync_warning();
        Ok(Json(r))
    }

    /// Move or rename notes and directories. All-or-nothing.
    #[tool(
        description = "Move or rename notes and directories. Each item's dest either ends with '/' — move into that directory keeping the basename ('/' alone is the vault root) — or is the full destination path including the new basename (a note keeps .md), renaming and moving in one step. All-or-nothing: collisions (without overwrite), moving a directory into its own subtree, or in-batch duplicates/swaps reject the whole batch; an item may land inside another item's moved directory (ancestor destinations apply first). dry_run:true validates and returns the planned destinations without moving. prune_empty:true also removes source directories the batch left empty (reported as pruned). update_links:true also rewrites standard-Markdown links vault-wide so they keep pointing at the moved notes (relinked notes are reported; wikilinks untouched).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn move_notes(
        &self,
        Parameters(req): Parameters<MoveNotesRequest>,
    ) -> Result<Json<MoveResponse>, ErrorData> {
        batch_limit(req.moves.len())?;
        let _guard = self.lock().write().await;
        let mut r = self
            .run_move(
                &req.moves,
                req.overwrite,
                req.dry_run,
                req.prune_empty,
                req.update_links,
            )
            .await;
        r.sync_warning = self.sync_warning();
        Ok(Json(r))
    }
}

impl MdServer {
    async fn run_delete(
        &self,
        paths: &[String],
        dry_run: bool,
        prune_empty: bool,
    ) -> DeleteNotesResponse {
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
                pruned: vec![],
                dry_run,
                sync_warning: None,
            };
        }
        if dry_run {
            // Validation passed; report the plan without touching the vault.
            let deleted = paths
                .iter()
                .map(|p| DeletedItem {
                    path: p.clone(),
                    trashed_to: self.vault().planned_trash_path(p),
                })
                .collect();
            return DeleteNotesResponse {
                ok: true,
                deleted,
                errors: vec![],
                pruned: vec![],
                dry_run: true,
                sync_warning: None,
            };
        }

        let ops: Vec<Op> = paths
            .iter()
            .map(|p| Op::Delete { path: p.clone() })
            .collect();
        match self.vault().commit_batch(&ops) {
            Ok(receipt) => {
                let ops = EventOp::from_outcomes(&receipt.outcomes);
                self.emit_event("delete_notes", Some(&receipt.batch_id), &ops);
                self.auto_commit("delete_notes", &ops, &paths).await;
                let deleted = receipt
                    .outcomes
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
                let pruned = if prune_empty {
                    self.prune_emptied_dirs(paths.iter().map(String::as_str))
                } else {
                    vec![]
                };
                DeleteNotesResponse {
                    ok: true,
                    deleted,
                    errors: vec![],
                    pruned,
                    dry_run: false,
                    sync_warning: None,
                }
            }
            Err(e) => DeleteNotesResponse {
                ok: false,
                deleted: vec![],
                errors: vec![ApiError::from_core(&e)],
                pruned: vec![],
                dry_run: false,
                sync_warning: None,
            },
        }
    }

    async fn run_move(
        &self,
        moves: &[MoveItem],
        overwrite: bool,
        dry_run: bool,
        prune_empty: bool,
        update_links: bool,
    ) -> MoveResponse {
        let mut errors = Vec::new();
        let mut pairs: Vec<(usize, String, String)> = Vec::new();

        for (i, m) in moves.iter().enumerate() {
            match self.compute_move(m) {
                Ok(to) => {
                    // "Occupied" must mean a *different* file: a destination
                    // that resolves to the source itself is a Unicode
                    // respelling of the same note (e.g. NFD -> NFC), not a
                    // collision.
                    let same_file = matches!(
                        (
                            self.vault().resolve_rel(strip_slash(&to)),
                            self.vault().resolve_rel(strip_slash(&m.source)),
                        ),
                        (Ok(a), Ok(b)) if a == b
                    );
                    if strip_slash(&to) == strip_slash(&m.source) {
                        // A no-op move reads as "already exists" otherwise,
                        // suggesting a different file is in the way. (It would
                        // also fail at commit time with a raw un-indexed IO
                        // error after a backup/rollback round-trip.)
                        errors.push(err(
                            i,
                            Code::Conflict,
                            format!("source is already at the destination: {}", m.source),
                        ));
                    } else if !overwrite && !same_file && self.vault().exists(&to).unwrap_or(false)
                    {
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
        self.finish_move(
            "move_notes",
            pairs,
            errors,
            MoveOpts {
                dry_run,
                prune_empty,
                update_links,
            },
            serde_json::json!({"moves": moves, "overwrite": overwrite, "update_links": update_links}),
        )
        .await
    }

    /// Best-effort post-batch pruning: ascend from each source's parent,
    /// removing directories the batch left empty. `remove_empty_dir` refuses a
    /// non-empty directory, so any remaining content stops the ascent — this
    /// can never delete notes. Returns pruned dirs with the `/` suffix.
    fn prune_emptied_dirs<'a>(&self, sources: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut pruned = Vec::new();
        for src in sources {
            let mut dir = parent_dir(src);
            while !dir.is_empty() && self.vault().remove_empty_dir(&dir) {
                pruned.push(dir.clone());
                dir = parent_dir(&dir);
            }
        }
        pruned.sort();
        pruned.dedup();
        pruned
    }

    /// Reject a destination-directory chain segment occupied by a
    /// non-directory — so the failure is an indexed validation rejection, not
    /// a late create_dir_all IO error at commit time.
    fn check_dest_chain(&self, dest: &str) -> Result<(), Error> {
        let mut prefix = String::new();
        for seg in strip_slash(dest).split('/').filter(|s| !s.is_empty()) {
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
                    format!("dest path is occupied by a non-directory: {prefix}"),
                ));
            }
        }
        Ok(())
    }

    /// Resolve a `MoveItem`'s destination path (ADR-0024): a `dest` ending in
    /// `/` targets a directory keeping the source basename; otherwise it is
    /// the full destination path including the new basename.
    fn compute_move(&self, m: &MoveItem) -> Result<String, Error> {
        Vault::validate_rel(&m.source)?;
        if !self.vault().exists(&m.source).unwrap_or(false) {
            return Err(Error::not_found(format!("source not found: {}", m.source)));
        }
        let is_dir = is_dir_path(&m.source);
        let dir_suffix = if is_dir { "/" } else { "" };
        let to = if m.dest == "/" || is_dir_path(&m.dest) {
            // Directory target, keeping the source basename. "/" is the suffix
            // convention's spelling of the vault root — without it there is no
            // way to move a note back to the top level.
            let dest_prefix: &str = if m.dest == "/" { "" } else { &m.dest };
            self.check_dest_chain(dest_prefix)?;
            format!("{dest_prefix}{}{dir_suffix}", basename(&m.source))
        } else {
            // Full destination path, including the new basename.
            if is_dir {
                if m.dest.ends_with(".md") {
                    return Err(Error::new(
                        Code::Suffix,
                        "a directory's dest must not end with .md",
                    ));
                }
            } else if !m.dest.ends_with(".md") {
                return Err(Error::new(
                    Code::Suffix,
                    "a note's dest must end with .md (end dest with '/' to move into a directory)",
                ));
            }
            self.check_dest_chain(&parent_dir(&m.dest))?;
            format!("{}{dir_suffix}", m.dest)
        };
        Vault::validate_rel(&to)?;
        // A directory cannot move into its own subtree. Strict containment on
        // purpose: an equal key is either a no-op or a Unicode respelling of
        // the same directory, both handled by the caller's conflict checks.
        if is_dir && contains_strict(&m.source, &to) {
            return Err(Error::new(
                Code::Overlap,
                "cannot move a directory into its own subtree",
            ));
        }
        Ok(to)
    }

    async fn finish_move(
        &self,
        tool: &str,
        mut pairs: Vec<(usize, String, String)>,
        mut errors: Vec<ApiError>,
        opts: MoveOpts,
        args: serde_json::Value,
    ) -> MoveResponse {
        let MoveOpts {
            dry_run,
            prune_empty,
            update_links,
        } = opts;
        check_move_collisions(&pairs, &mut errors);
        if !errors.is_empty() {
            errors.sort_by_key(|e| e.index);
            return MoveResponse {
                ok: false,
                moved: vec![],
                errors,
                pruned: vec![],
                relinked: vec![],
                dry_run,
                sync_warning: None,
            };
        }
        let (link_ops, relinked) = if update_links && !pairs.is_empty() {
            self.plan_link_rewrites(tool, &pairs, dry_run)
        } else {
            (vec![], vec![])
        };
        if dry_run {
            // Validation passed; report the plan without touching the vault.
            let moved = pairs
                .into_iter()
                .map(|(_, from, to)| MovedItem { from, to })
                .collect();
            return MoveResponse {
                ok: true,
                moved,
                errors: vec![],
                pruned: vec![],
                relinked,
                dry_run: true,
                sync_warning: None,
            };
        }
        // Nested destinations declare a final tree (ADR-0024): apply ancestor
        // destinations first so the directory a child lands in exists by the
        // time the child moves. Lexicographic dest order puts every ancestor
        // before its descendants; the response maps back to input order below.
        pairs.sort_by_key(|(_, _, to)| nfc_key(to));
        // Link rewrites go first, addressed at pre-move paths: the moves then
        // carry the rewritten bodies, and a move that overwrites a rewritten
        // note still wins (write-after-move would resurrect dead content).
        let ops: Vec<Op> = link_ops
            .into_iter()
            .chain(pairs.iter().map(|(_, from, to)| Op::Move {
                from: from.clone(),
                to: to.clone(),
            }))
            .collect();
        match self.vault().commit_batch(&ops) {
            Ok(receipt) => {
                let ops = EventOp::from_outcomes(&receipt.outcomes);
                self.emit_event(tool, Some(&receipt.batch_id), &ops);
                self.auto_commit(tool, &ops, &args).await;
                let mut moved: Vec<(usize, MovedItem)> = receipt
                    .outcomes
                    .into_iter()
                    .filter(|o| matches!(o, OpOutcome::Moved { .. }))
                    .zip(&pairs)
                    .filter_map(|(o, (idx, pfrom, pto))| match o {
                        OpOutcome::Moved { from, to } => Some((
                            *idx,
                            MovedItem {
                                from: with_dir_suffix(from, pfrom),
                                to: with_dir_suffix(to, pto),
                            },
                        )),
                        _ => None,
                    })
                    .collect();
                moved.sort_by_key(|(i, _)| *i);
                let moved = moved.into_iter().map(|(_, m)| m).collect();
                let pruned = if prune_empty {
                    self.prune_emptied_dirs(pairs.iter().map(|(_, from, _)| from.as_str()))
                } else {
                    vec![]
                };
                MoveResponse {
                    ok: true,
                    moved,
                    errors: vec![],
                    pruned,
                    relinked,
                    dry_run: false,
                    sync_warning: None,
                }
            }
            Err(e) => MoveResponse {
                ok: false,
                moved: vec![],
                errors: vec![ApiError::from_core(&e)],
                pruned: vec![],
                relinked: vec![],
                dry_run: false,
                sync_warning: None,
            },
        }
    }

    /// Plan the `update_links` rewrites for a validated batch: one full vault
    /// scan (ADR-0022 — no index), returning `Op::Write`s at pre-move paths
    /// plus the rewritten notes' post-batch paths for the response.
    ///
    /// Scan shape: every note is read once; a byte-level pre-filter (link
    /// punctuation, plus a moved basename for notes the batch doesn't move)
    /// rejects most notes before the CommonMark extractor runs. An unreadable
    /// note is skipped with a warning rather than sinking the batch.
    fn plan_link_rewrites(
        &self,
        tool: &str,
        pairs: &[(usize, String, String)],
        dry_run: bool,
    ) -> (Vec<Op>, Vec<String>) {
        use md_core::relink::MoveMap;

        let started = std::time::Instant::now();
        let map = MoveMap::new(
            &pairs
                .iter()
                .map(|(_, f, t)| (f.clone(), t.clone()))
                .collect::<Vec<_>>(),
        );
        // Pre-filter needles: a link to a moved path must spell its basename,
        // raw or with the common %20 space encoding. Moved notes themselves
        // bypass this (their own outbound links can point anywhere).
        let needles: Vec<String> = pairs
            .iter()
            .flat_map(|(_, from, _)| {
                let base = basename(from).to_string();
                let encoded = base.replace(' ', "%20");
                if encoded == base {
                    vec![base]
                } else {
                    vec![base, encoded]
                }
            })
            .collect();

        let entries = match self.vault().list_entries("", true, None, false) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "update_links: vault listing failed; skipping rewrites");
                return (vec![], vec![]);
            }
        };
        let notes_total = entries.len();
        let mut candidates = 0usize;
        let mut ops = Vec::new();
        let mut relinked = Vec::new();
        for entry in entries {
            let old_path = entry.path;
            let new_path = map.apply(&old_path).unwrap_or_else(|| old_path.clone());
            let source = match self.vault().read_note(&old_path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(path = %old_path, error = %e, "update_links: unreadable note skipped");
                    continue;
                }
            };
            if !(source.contains("](") || source.contains("]:")) {
                continue;
            }
            if old_path == new_path && !needles.iter().any(|n| source.contains(n.as_str())) {
                continue;
            }
            candidates += 1;
            // Links live in the body; frontmatter is never rewritten.
            let body_start = md_core::Document::parse(&source)
                .frontmatter
                .map_or(0, |s| s.end);
            let Some(new_body) =
                md_core::rewrite_body(&source[body_start..], &old_path, &new_path, &map)
            else {
                continue;
            };
            let content = format!("{}{}", &source[..body_start], new_body);
            ops.push(Op::Write {
                path: old_path,
                content: content.into_bytes(),
            });
            relinked.push(new_path);
        }
        tracing::info!(
            tool,
            moves = pairs.len(),
            notes_total,
            candidates,
            rewritten = relinked.len(),
            dry_run,
            elapsed_us = started.elapsed().as_micros() as u64,
            "update_links scan"
        );
        (ops, relinked)
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
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert!(s.vault().exists("a.md").unwrap());

        let ok = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["a.md".into()],
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok);
        assert!(ok.deleted[0].trashed_to.starts_with(".md-mcp/trash/"));
        assert!(!s.vault().exists("a.md").unwrap());
    }

    /// Shorthand for a single-item move_notes call with default flags.
    async fn move_one(s: &MdServer, source: &str, dest: &str) -> MoveResponse {
        s.move_notes(Parameters(MoveNotesRequest {
            moves: vec![MoveItem {
                source: source.into(),
                dest: dest.into(),
            }],
            overwrite: false,
            update_links: false,
            dry_run: false,
            prune_empty: false,
        }))
        .await
        .unwrap()
        .0
    }

    #[tokio::test]
    async fn dry_run_previews_a_valid_batch_without_writing() {
        let (_d, s) = server(&[("업무/plan.md", "x"), ("b.md", "y")]);

        // move --dry_run: the planned destinations come back, nothing moves.
        let r = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "업무/plan.md".into(),
                        dest: "02-areas/work/".into(),
                    },
                    MoveItem {
                        source: "b.md".into(),
                        dest: "c.md".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: true,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(r.ok, "{:?}", r.errors);
        assert!(r.dry_run);
        assert_eq!(r.moved[0].to, "02-areas/work/plan.md");
        assert_eq!(r.moved[1].to, "c.md");
        assert!(s.vault().exists("업무/plan.md").unwrap(), "not applied");
        assert!(!s.vault().exists("02-areas/work/plan.md").unwrap());
        assert!(s.vault().exists("b.md").unwrap(), "not applied");

        // delete --dry_run: the planned trash location, note still present.
        let r = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["b.md".into()],
                dry_run: true,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(r.ok, "{:?}", r.errors);
        assert!(r.dry_run);
        assert!(r.deleted[0].trashed_to.starts_with(".md-mcp/trash/"));
        assert!(s.vault().exists("b.md").unwrap(), "not applied");
    }

    #[tokio::test]
    async fn prune_empty_removes_only_dirs_the_batch_emptied() {
        let (_d, s) = server(&[("a/b/n.md", "x"), ("a/keep.md", "y")]);
        let r = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![MoveItem {
                    source: "a/b/n.md".into(),
                    dest: "dst/".into(),
                }],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: true,
            }))
            .await
            .unwrap()
            .0;
        assert!(r.ok, "{:?}", r.errors);
        // a/b/ was emptied and pruned; a/ still holds keep.md and stays.
        assert_eq!(r.pruned, vec!["a/b/"]);
        assert!(!s.vault().exists("a/b").unwrap());
        assert!(s.vault().exists("a/keep.md").unwrap());
    }

    #[tokio::test]
    async fn prune_empty_ascends_and_defaults_off() {
        // Default: the emptied chain stays.
        let (_d, s) = server(&[("x/y/n.md", "v")]);
        let r = move_one(&s, "x/y/n.md", "/").await;
        assert!(r.ok, "{:?}", r.errors);
        assert!(r.pruned.is_empty());
        assert!(s.vault().is_dir("x/y").unwrap(), "kept without the flag");

        // With the flag, delete ascends the emptied chain: x/y/, then x/.
        let (_d2, s2) = server(&[("x/y/n.md", "v")]);
        let r = s2
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["x/y/n.md".into()],
                dry_run: false,
                prune_empty: true,
            }))
            .await
            .unwrap()
            .0;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.pruned, vec!["x/", "x/y/"]);
        assert!(!s2.vault().exists("x").unwrap());
    }

    #[tokio::test]
    async fn dry_run_reports_every_rejection_without_writing() {
        // The same all-or-nothing validation runs; dry_run only skips the write.
        let (_d, s) = server(&[("a.md", "x"), ("b.md", "y")]);
        let r = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "a.md".into(),
                        dest: "dst/".into(),
                    },
                    MoveItem {
                        source: "missing.md".into(),
                        dest: "dst/".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: true,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!r.ok);
        assert!(r.dry_run);
        assert_eq!(r.errors[0].code, "NOT_FOUND");
        assert!(s.vault().exists("a.md").unwrap());
    }

    #[tokio::test]
    async fn move_to_the_vault_root_via_slash() {
        let (_d, s) = server(&[("deep/n.md", "x")]);
        let ok = move_one(&s, "deep/n.md", "/").await;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.moved[0].to, "n.md");
        assert!(s.vault().exists("n.md").unwrap());

        // A directory also moves to the root; a top-level dir is a no-op.
        let bad = move_one(&s, "deep/", "/").await;
        assert!(!bad.ok, "top-level dir into root is a no-op");
        assert!(bad.errors[0].message.contains("already at the destination"));
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
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "OVERLAP", "{:?}", bad.errors);
        assert!(s.vault().exists(&file).unwrap(), "nothing deleted");

        let bad = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: file.clone(),
                        dest: format!("{nfd_dir}/d.md"),
                    },
                    MoveItem {
                        source: "\u{d55c}/c.md".into(),
                        dest: "\u{d55c}/e.md".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
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
    async fn move_fixes_unicode_spelling_of_the_same_note() {
        // NFD -> NFC and back: same text, different bytes; must rename the
        // on-disk spelling without overwrite and without a false CONFLICT.
        let nfd = "\u{1112}\u{1161}\u{11ab}.md"; // 한.md in NFD
        let nfc = "\u{d55c}.md"; // 한.md in NFC
        let (_d, s) = server(&[(nfd, "content")]);

        let ok = move_one(&s, nfd, nfc).await;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.moved[0].to, nfc);
        assert_eq!(
            s.vault().resolve_rel(nfc).unwrap(),
            nfc,
            "on-disk bytes are NFC now"
        );

        // And back to NFD.
        let ok = move_one(&s, nfc, nfd).await;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(s.vault().resolve_rel(nfc).unwrap(), nfd);
        assert_eq!(s.vault().read_note(nfc).unwrap(), "content");
    }

    #[tokio::test]
    async fn noop_move_is_an_indexed_validation_error() {
        // Moving to the current path used to fail at commit time with a raw
        // un-indexed IO error (after a backup/rollback round-trip).
        let (_d, s) = server(&[("a.md", "x"), ("d/n.md", "y")]);

        // Full-path form: dest equals the source.
        let bad = move_one(&s, "a.md", "a.md").await;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "CONFLICT");
        assert_eq!(bad.errors[0].index, Some(0));
        assert!(bad.errors[0].message.contains("already at the destination"));
        assert!(s.vault().exists("a.md").unwrap());

        // Directory-target form: the source is already in dest.
        let bad = move_one(&s, "d/n.md", "d/").await;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "CONFLICT");
        assert!(bad.errors[0].message.contains("already at the destination"));
    }

    #[tokio::test]
    async fn dir_echoes_keep_the_trailing_slash() {
        // Echoed directory paths follow the suffix convention so they can be
        // fed straight back into path/dest arguments.
        let (_d, s) = server(&[("d/n.md", "x")]);

        // Rename a directory via a full destination path.
        let ok = move_one(&s, "d/", "e").await;
        assert_eq!(ok.moved[0].from, "d/");
        assert_eq!(ok.moved[0].to, "e/");

        // Move a directory into a directory target.
        let ok = move_one(&s, "e/", "arch/").await;
        assert_eq!(ok.moved[0].to, "arch/e/");

        let ok = s
            .delete_notes(Parameters(DeleteNotesRequest {
                paths: vec!["arch/e/".into()],
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(ok.deleted[0].path, "arch/e/");
    }

    #[tokio::test]
    async fn rename_in_place_via_full_path_with_suffix_rules() {
        let (_d, s) = server(&[("note.md", "x"), ("d/sub.md", "y")]);
        let ok = move_one(&s, "note.md", "renamed.md").await;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.moved[0].to, "renamed.md");
        assert!(s.vault().exists("renamed.md").unwrap());

        // A note's full-path dest must keep the .md extension.
        let bad = move_one(&s, "renamed.md", "extensionless").await;
        assert_eq!(bad.errors[0].code, "SUFFIX");

        // A directory's full-path dest must not end with .md.
        let bad = move_one(&s, "d/", "d2.md").await;
        assert_eq!(bad.errors[0].code, "SUFFIX");
    }

    #[tokio::test]
    async fn move_and_rename_in_one_call() {
        // The unified primitive's reason to exist (ADR-0024): a move that also
        // changes the basename is one atomic item, not a relocate+rename pair.
        let (_d, s) = server(&[("a/note.md", "x")]);
        let ok = move_one(&s, "a/note.md", "b/new.md").await;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.moved[0].from, "a/note.md");
        assert_eq!(ok.moved[0].to, "b/new.md");
        assert!(s.vault().exists("b/new.md").unwrap());
        assert!(!s.vault().exists("a/note.md").unwrap());

        // Same for a directory: move it under a new parent with a new name.
        let (_d2, s2) = server(&[("d/n.md", "x")]);
        let ok = move_one(&s2, "d/", "arch/d2").await;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(ok.moved[0].to, "arch/d2/");
        assert!(s2.vault().exists("arch/d2/n.md").unwrap());
    }

    #[tokio::test]
    async fn move_into_directory_keeps_basename() {
        let (_d, s) = server(&[("a.md", "x")]);
        let ok = move_one(&s, "a.md", "archive/").await;
        assert!(ok.ok);
        assert_eq!(ok.moved[0].to, "archive/a.md");
        assert!(s.vault().exists("archive/a.md").unwrap());
    }

    #[tokio::test]
    async fn nested_destinations_apply_ancestor_first() {
        // Issue #7's shape (ADR-0024): move a directory AND land another item
        // inside its destination, in one atomic batch. Input order is child
        // first to prove the engine orders ancestor destinations first.
        let (_d, s) = server(&[("ak/readme.md", "r"), ("kernel/dma-buf/x.md", "x")]);
        let ok = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "kernel/dma-buf/".into(),
                        dest: "02-areas/ak/dma-buf".into(),
                    },
                    MoveItem {
                        source: "ak/".into(),
                        dest: "02-areas/".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok, "{:?}", ok.errors);
        // The response keeps the input order.
        assert_eq!(ok.moved[0].from, "kernel/dma-buf/");
        assert_eq!(ok.moved[0].to, "02-areas/ak/dma-buf/");
        assert_eq!(ok.moved[1].from, "ak/");
        assert_eq!(ok.moved[1].to, "02-areas/ak/");
        assert!(s.vault().exists("02-areas/ak/readme.md").unwrap());
        assert!(s.vault().exists("02-areas/ak/dma-buf/x.md").unwrap());
        assert!(!s.vault().exists("ak").unwrap());
        assert!(!s.vault().exists("kernel/dma-buf").unwrap());
    }

    #[tokio::test]
    async fn dest_inside_another_items_source_is_rejected() {
        // An item landing inside a subtree another item vacates is
        // order-sensitive in a confusing way — rejected, not sequenced.
        let (_d, s) = server(&[("a/keep.md", "k"), ("x.md", "x")]);
        let bad = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "a/".into(),
                        dest: "z/".into(),
                    },
                    MoveItem {
                        source: "x.md".into(),
                        dest: "a/sub/x.md".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert!(
            bad.errors.iter().all(|e| e.code == "BATCH_COLLISION"),
            "{:?}",
            bad.errors
        );
        assert!(s.vault().exists("a/keep.md").unwrap(), "nothing moved");
        assert!(s.vault().exists("x.md").unwrap());
    }

    #[tokio::test]
    async fn swaps_and_chains_are_rejected() {
        let (_d, s) = server(&[("a.md", "x"), ("b.md", "y")]);
        // Swap: a -> b while b -> a.
        let bad = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "a.md".into(),
                        dest: "b.md".into(),
                    },
                    MoveItem {
                        source: "b.md".into(),
                        dest: "a.md".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        // The swap is a BATCH_COLLISION; each dest being an existing note
        // also surfaces as a CONFLICT — both reject the batch.
        assert!(
            bad.errors.iter().any(|e| e.code == "BATCH_COLLISION"),
            "{:?}",
            bad.errors
        );

        // Chain: a moves to where b was while b moves away.
        let bad = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "a.md".into(),
                        dest: "b.md".into(),
                    },
                    MoveItem {
                        source: "b.md".into(),
                        dest: "c.md".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
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
        assert!(s.vault().exists("a.md").unwrap());
        assert!(s.vault().exists("b.md").unwrap());
    }

    #[tokio::test]
    async fn dir_into_own_subtree_is_rejected_in_both_forms() {
        let (_d, s) = server(&[("d/n.md", "x")]);
        let bad = move_one(&s, "d/", "d/sub/").await;
        assert_eq!(bad.errors[0].code, "OVERLAP");
        let bad = move_one(&s, "d/", "d/sub2").await;
        assert_eq!(bad.errors[0].code, "OVERLAP");
        assert!(s.vault().exists("d/n.md").unwrap());
    }

    #[tokio::test]
    async fn conflict_requires_overwrite() {
        let (_d, s) = server(&[("a.md", "new"), ("taken.md", "old")]);
        let bad = move_one(&s, "a.md", "taken.md").await;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "CONFLICT");
        assert_eq!(s.vault().read_note("taken.md").unwrap(), "old");

        let ok = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![MoveItem {
                    source: "a.md".into(),
                    dest: "taken.md".into(),
                }],
                overwrite: true,
                update_links: false,
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok, "{:?}", ok.errors);
        assert_eq!(s.vault().read_note("taken.md").unwrap(), "new");
        assert!(!s.vault().exists("a.md").unwrap());
    }

    #[tokio::test]
    async fn missing_source_is_an_indexed_not_found() {
        // A missing source must fail validation as NOT_FOUND with the item's
        // index — not leak a raw IO rename error from the commit stage.
        let (_d, s) = server(&[("a.md", "x")]);
        let bad = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "a.md".into(),
                        dest: "archive/".into(),
                    },
                    MoveItem {
                        source: "ghost.md".into(),
                        dest: "archive/".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "NOT_FOUND");
        assert_eq!(bad.errors[0].index, Some(1));
        assert!(
            s.vault().exists("a.md").unwrap(),
            "all-or-nothing: a.md stays"
        );
    }

    #[tokio::test]
    async fn file_occupied_dest_is_rejected_with_index_in_both_forms() {
        let (_d, s) = server(&[("a.md", "x"), ("b.md", "y"), ("blocker", "i am a file")]);
        // Directory-target form: dest itself is a file.
        let bad = move_one(&s, "a.md", "blocker/").await;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "DEST_NOT_DIR");
        assert_eq!(bad.errors[0].index, Some(0));
        assert!(s.vault().exists("a.md").unwrap());

        // Full-path form: an ancestor of dest is a file.
        let bad = move_one(&s, "b.md", "blocker/sub/b.md").await;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "DEST_NOT_DIR");
        assert!(s.vault().exists("b.md").unwrap());
    }

    #[tokio::test]
    async fn duplicate_destination_across_forms_is_rejected() {
        // A directory-target item and a full-path item computing the same
        // destination (neither exists yet) -> an in-batch collision.
        let (_d, s) = server(&[("a.md", "x"), ("sub/a.md", "y")]);
        let collide = s
            .move_notes(Parameters(MoveNotesRequest {
                moves: vec![
                    MoveItem {
                        source: "a.md".into(),
                        dest: "d/".into(),
                    },
                    MoveItem {
                        source: "sub/a.md".into(),
                        dest: "d/a.md".into(),
                    },
                ],
                overwrite: false,
                update_links: false,
                dry_run: false,
                prune_empty: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!collide.ok);
        assert!(collide.errors.iter().all(|e| e.code == "BATCH_COLLISION"));
    }

    #[tokio::test]
    async fn dest_outside_the_vault_is_rejected() {
        let (_d, s) = server(&[("a.md", "x")]);
        let bad = move_one(&s, "a.md", "../escape.md").await;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].index, Some(0));
        assert!(s.vault().exists("a.md").unwrap());
    }

    // --- update_links (ADR-0022) -----------------------------------------

    async fn move_linked(s: &MdServer, source: &str, dest: &str, dry_run: bool) -> MoveResponse {
        s.move_notes(Parameters(MoveNotesRequest {
            moves: vec![MoveItem {
                source: source.into(),
                dest: dest.into(),
            }],
            overwrite: false,
            update_links: true,
            dry_run,
            prune_empty: false,
        }))
        .await
        .unwrap()
        .0
    }

    #[tokio::test]
    async fn update_links_repoints_inbound_links_atomically() {
        let (_d, s) = server(&[
            ("target.md", "# T"),
            ("linker.md", "see [t](target.md#top) and [o](other.md)"),
            ("other.md", "no links to the target"),
        ]);
        let r = move_linked(&s, "target.md", "sub/", false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.relinked, ["linker.md"]);
        assert_eq!(
            s.vault().read_note("linker.md").unwrap(),
            "see [t](sub/target.md#top) and [o](other.md)",
            "moved link repointed, fragment kept, other link untouched"
        );
        assert_eq!(
            s.vault().read_note("other.md").unwrap(),
            "no links to the target"
        );
    }

    #[tokio::test]
    async fn update_links_recomputes_the_moved_notes_outbound_links() {
        let (_d, s) = server(&[("note.md", "[s](sib.md)"), ("sib.md", "y")]);
        let r = move_linked(&s, "note.md", "deep/", false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.relinked, ["deep/note.md"]);
        assert_eq!(
            s.vault().read_note("deep/note.md").unwrap(),
            "[s](../sib.md)",
            "the moved note's own relative link is recomputed"
        );
    }

    #[tokio::test]
    async fn update_links_follows_a_directory_move() {
        let (_d, s) = server(&[("dir/x.md", "# X"), ("linker.md", "[x](dir/x.md)")]);
        let r = move_linked(&s, "dir/", "archive/", false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.relinked, ["linker.md"]);
        assert_eq!(
            s.vault().read_note("linker.md").unwrap(),
            "[x](archive/dir/x.md)"
        );
    }

    #[tokio::test]
    async fn update_links_dry_run_previews_without_writing() {
        let (_d, s) = server(&[("target.md", "# T"), ("linker.md", "[t](target.md)")]);
        let r = move_linked(&s, "target.md", "sub/", true).await;
        assert!(r.ok && r.dry_run);
        assert_eq!(r.relinked, ["linker.md"], "the plan names the note");
        assert_eq!(
            s.vault().read_note("linker.md").unwrap(),
            "[t](target.md)",
            "nothing written on dry_run"
        );
        assert!(s.vault().exists("target.md").unwrap());
    }

    #[tokio::test]
    async fn update_links_off_by_default_leaves_links_alone() {
        let (_d, s) = server(&[("target.md", "# T"), ("linker.md", "[t](target.md)")]);
        let r = move_one(&s, "target.md", "sub/").await;
        assert!(r.ok);
        assert!(r.relinked.is_empty());
        assert_eq!(s.vault().read_note("linker.md").unwrap(), "[t](target.md)");
    }

    #[tokio::test]
    async fn update_links_works_for_rename_and_skips_frontmatter() {
        let (_d, s) = server(&[
            ("old.md", "# O"),
            (
                "linker.md",
                "---\nurl: [f](old.md)\n---\nbody [l](old.md)\n",
            ),
        ]);
        let r = move_linked(&s, "old.md", "new.md", false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.relinked, ["linker.md"]);
        assert_eq!(
            s.vault().read_note("linker.md").unwrap(),
            "---\nurl: [f](old.md)\n---\nbody [l](new.md)\n",
            "body link rewritten; frontmatter never touched"
        );
    }

    #[tokio::test]
    async fn update_links_follows_a_move_and_rename_in_one() {
        // The combined form repoints inbound links to the full new path.
        let (_d, s) = server(&[("a/old.md", "# O"), ("linker.md", "[l](a/old.md)")]);
        let r = move_linked(&s, "a/old.md", "b/new.md", false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.relinked, ["linker.md"]);
        assert_eq!(s.vault().read_note("linker.md").unwrap(), "[l](b/new.md)");
    }
}
