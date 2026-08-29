//! Content-write tools: create_notes, append_notes, edit_sections, replace_text,
//! edit_properties.
//!
//! create/append are partial-success (independent items). edit_sections,
//! replace_text and edit_properties are all-or-nothing: the whole batch is
//! validated, then applied through the transaction engine
//! ([ADR-0011](../../../docs/adr/0011-error-envelope-and-structured-output.md)).

use std::collections::BTreeMap;

use md_core::section::Scope;
use md_core::{Code, Document, Op, frontmatter, patch};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::MdServer;
use crate::envelope::{ApiError, MAX_WRITE_BYTES, batch_limit, write_size_error};
use crate::events::EventOp;
use crate::tools_read::ScopeArg;

/// Distinguish an omitted `value` (remove) from an explicit `null` (set null).
fn deserialize_some<'de, D>(d: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(d).map(Some)
}

fn body_has_frontmatter(content: &str) -> bool {
    let normalized = md_core::text::normalize_newlines(content);
    Document::parse(&normalized).frontmatter.is_some()
}

// --- create_notes -----------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateNotesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub notes: Vec<NoteInput>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteInput {
    pub path: String,
    /// The note body (no leading `---` frontmatter block — pass frontmatter
    /// separately). Required unless `base` is given.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
    /// Copy this existing note verbatim (frontmatter and body). Excludes
    /// `content` and `frontmatter`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateNotesResponse {
    pub created: Vec<CreateResult>,
    /// Present while git sync is failing: local commits are not on the remote
    /// (ADR-0019). Inspect and recover via sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateResult {
    pub path: String,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

// --- append_notes -----------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppendNotesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub appends: Vec<AppendInput>,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppendInput {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub create_if_missing: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppendNotesResponse {
    pub appended: Vec<AppendResult>,
    /// Present while git sync is failing (ADR-0019); see sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppendResult {
    pub path: String,
    pub appended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

// --- edit_sections ----------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum OperationArg {
    Replace,
    Append,
    Delete,
    InsertBefore,
    InsertAfter,
    Rename,
    Move,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditSectionsRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub edits: Vec<EditItem>,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditItem {
    pub path: String,
    pub heading_path: Vec<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<usize>,
    pub operation: OperationArg,
    #[serde(default)]
    pub scope: ScopeArg,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_heading: Option<String>,
    /// Destination for the `move` operation.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<DestinationArg>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
pub enum PositionArg {
    Before,
    After,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct DestinationArg {
    pub heading_path: Vec<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<usize>,
    pub position: PositionArg,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditSectionsResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<AppliedEdit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
    /// Present while git sync is failing (ADR-0019); see sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppliedEdit {
    pub index: usize,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_heading_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

fn to_core_edit(item: &EditItem) -> Result<md_core::Edit, ApiError> {
    let operation = match item.operation {
        OperationArg::Replace => md_core::Operation::Replace,
        OperationArg::Append => md_core::Operation::Append,
        OperationArg::Delete => md_core::Operation::Delete,
        OperationArg::InsertBefore => md_core::Operation::InsertBefore,
        OperationArg::InsertAfter => md_core::Operation::InsertAfter,
        OperationArg::Rename => md_core::Operation::Rename,
        OperationArg::Move => md_core::Operation::Move,
    };
    let destination = item.destination.as_ref().map(|d| md_core::Destination {
        heading_path: d.heading_path.clone(),
        occurrence: d.occurrence,
        position: match d.position {
            PositionArg::Before => md_core::Position::Before,
            PositionArg::After => md_core::Position::After,
        },
    });
    Ok(md_core::Edit {
        heading_path: item.heading_path.clone(),
        occurrence: item.occurrence,
        operation: Some(operation),
        scope: item.scope.into(),
        content: item.content.clone(),
        new_heading: item.new_heading.clone(),
        destination,
        expected_hash: item.expected_hash.clone(),
    })
}

/// Post-edit content_hash and the resulting heading path (for rename/move),
/// best-effort. `occ` is the occurrence to resolve the post-edit path by.
fn edit_outcome(new_source: &str, item: &EditItem) -> (Option<Vec<String>>, Option<String>) {
    let doc = Document::parse(new_source);
    let (path, scope, new_path, occ) = match item.operation {
        OperationArg::Rename => {
            let mut p = item.heading_path.clone();
            if let (Some(last), Some(nh)) = (p.last_mut(), item.new_heading.as_ref()) {
                *last = nh.clone();
            }
            (p.clone(), Scope::Section, Some(p), item.occurrence)
        }
        OperationArg::Move => {
            // The moved section keeps its own heading text; it becomes a sibling
            // of the destination anchor (or a top-level section at the root).
            let (Some(dest), Some(leaf)) = (item.destination.as_ref(), item.heading_path.last())
            else {
                return (None, None);
            };
            let mut p = dest.heading_path.clone();
            if p.is_empty() {
                p.push(leaf.clone());
            } else {
                *p.last_mut().expect("non-empty") = leaf.clone();
            }
            (p.clone(), Scope::Section, Some(p), None)
        }
        OperationArg::Replace | OperationArg::Append | OperationArg::Delete => (
            item.heading_path.clone(),
            item.scope.into(),
            None,
            item.occurrence,
        ),
        _ => return (None, None),
    };
    let idx = if path.is_empty() {
        Some(None)
    } else {
        doc.resolve_heading(&path, occ).ok().map(Some)
    };
    (
        new_path,
        idx.map(|i| doc.content_hash(new_source, i, scope)),
    )
}

// --- replace_text -----------------------------------------------------------

/// How many changed lines one item reports: enough to see where the edit
/// landed, never enough to re-send the note
/// ([ADR-0027](../../../docs/adr/0027-literal-text-replacement.md)).
const MAX_HITS: usize = 5;
/// Byte budget for one reported line, cut on a char boundary and marked.
const MAX_HIT_BYTES: usize = 160;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReplaceTextRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub replaces: Vec<ReplaceItem>,
    /// Report what would change without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReplaceItem {
    pub path: String,
    /// The text to find, matched literally (byte-for-byte), never as a pattern.
    pub find: String,
    /// What each match becomes; empty deletes the match.
    pub replace: String,
    /// Restrict the search to one section; empty searches the whole note body.
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<usize>,
    #[serde(default)]
    pub scope: ScopeArg,
    /// Replace every match instead of requiring exactly one.
    #[serde(default)]
    pub replace_all: bool,
    /// Assert the number of matches; the batch is rejected unless it holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReplaceTextResponse {
    pub ok: bool,
    pub applied: Vec<AppliedReplace>,
    pub errors: Vec<ApiError>,
    /// Echoes the request: `true` means nothing was written.
    pub dry_run: bool,
    /// Present while git sync is failing (ADR-0019); see sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppliedReplace {
    pub index: usize,
    pub path: String,
    /// How many matches were replaced — may exceed the reported `hits`.
    pub replaced: usize,
    /// The first few changed lines, addressed in the resulting note.
    pub hits: Vec<ReplaceHit>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReplaceHit {
    /// 1-based line number in the note as it now reads.
    pub line: usize,
    /// That line after the replacement, truncated with `…` when long.
    pub text: String,
}

fn to_core_replacement(item: &ReplaceItem) -> md_core::Replacement {
    md_core::Replacement {
        heading_path: item.heading_path.clone(),
        occurrence: item.occurrence,
        scope: item.scope.into(),
        find: item.find.clone(),
        replace: item.replace.clone(),
        replace_all: item.replace_all,
        expected_count: item.expected_count,
        expected_hash: item.expected_hash.clone(),
    }
}

/// Bound one reported line to [`MAX_HIT_BYTES`], cutting on a char boundary.
fn truncate_hit(line: &str) -> String {
    if line.len() <= MAX_HIT_BYTES {
        return line.to_string();
    }
    let mut end = MAX_HIT_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

// --- edit_properties --------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditPropertiesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub edits: Vec<PropertyEdit>,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[schemars(crate = "rmcp::schemars")]
pub struct PropertyEdit {
    pub path: String,
    pub key: String,
    /// Present (even `null`) sets the key; omitted removes it. Serialization
    /// mirrors that (`None` omitted), so a condensed commit-body call reads
    /// back faithfully.
    #[serde(
        default,
        deserialize_with = "deserialize_some",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditPropertiesResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<PropertyApplied>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
    /// Present while git sync is failing (ADR-0019); see sync_vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PropertyApplied {
    pub index: usize,
    pub path: String,
    pub key: String,
}

#[tool_router(router = write_router, vis = "pub(crate)")]
impl MdServer {
    /// Create new notes (refusing to overwrite unless `overwrite` is set).
    #[tool(
        description = "Create one or more notes. content is the body only; pass frontmatter as a separate object (a leading --- block in content is rejected). Alternatively pass base — the path of an existing note to copy verbatim (frontmatter and body); base excludes content and frontmatter, so edit the copy afterwards with the other tools. Partial success: a failing note does not block the others. overwrite:false refuses an existing note.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn create_notes(
        &self,
        Parameters(req): Parameters<CreateNotesRequest>,
    ) -> Result<Json<CreateNotesResponse>, ErrorData> {
        batch_limit(req.notes.len())?;
        let _guard = self.lock().write().await;
        let created: Vec<CreateResult> = req
            .notes
            .iter()
            .map(|note| {
                let result = match (&note.base, &note.content) {
                    (Some(_), _) if note.content.is_some() || note.frontmatter.is_some() => {
                        Err(md_core::Error::conflict(
                            "base copies a note verbatim; pass neither content nor frontmatter",
                        ))
                    }
                    (Some(base), _) => self.vault().read_note(base).and_then(|text| {
                        if text.len() > MAX_WRITE_BYTES {
                            return Err(write_size_error("note", text.len()));
                        }
                        self.vault().create_note(&note.path, text.as_bytes(), req.overwrite)
                    }),
                    (None, None) => Err(md_core::Error::new(
                        Code::MissingContent,
                        "content is required unless base is given",
                    )),
                    (None, Some(content)) if body_has_frontmatter(content) => {
                        Err(md_core::Error::conflict(
                            "content must not start with a --- frontmatter block; pass frontmatter separately",
                        ))
                    }
                    (None, Some(content)) => {
                        let fm = note.frontmatter.clone().unwrap_or(Value::Null);
                        frontmatter::with_frontmatter(content, &fm).and_then(|text| {
                            if text.len() > MAX_WRITE_BYTES {
                                return Err(write_size_error("note", text.len()));
                            }
                            self.vault().create_note(&note.path, text.as_bytes(), req.overwrite)
                        })
                    }
                };
                match result {
                    Ok(()) => CreateResult { path: note.path.clone(), created: true, error: None },
                    Err(e) => CreateResult { path: note.path.clone(), created: false, error: Some(ApiError::from_core(&e)) },
                }
            })
            .collect();
        let ops: Vec<EventOp> = created
            .iter()
            .filter(|c| c.created)
            .map(|c| EventOp::Create {
                path: c.path.clone(),
            })
            .collect();
        for op in &ops {
            self.emit_event("create_notes", None, std::slice::from_ref(op));
        }
        self.auto_commit("create_notes", &ops, &req).await;
        Ok(Json(CreateNotesResponse {
            created,
            sync_warning: self.sync_warning(),
        }))
    }

    /// Append raw content to the end of notes (no separator inserted).
    #[tool(
        description = "Append raw content to the end of each note (no separator is inserted; include your own newline). Partial success. create_if_missing creates an absent note. For section-internal edits use edit_sections.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub async fn append_notes(
        &self,
        Parameters(req): Parameters<AppendNotesRequest>,
    ) -> Result<Json<AppendNotesResponse>, ErrorData> {
        batch_limit(req.appends.len())?;
        let _guard = self.lock().write().await;
        let mut appended = Vec::with_capacity(req.appends.len());
        let mut ops = Vec::new();
        for item in &req.appends {
            let result = self.append_one(item);
            if result.appended {
                let op = EventOp::Write {
                    path: result.path.clone(),
                };
                self.emit_event("append_notes", None, std::slice::from_ref(&op));
                ops.push(op);
            }
            appended.push(result);
        }
        self.auto_commit("append_notes", &ops, &req).await;
        Ok(Json(AppendNotesResponse {
            appended,
            sync_warning: self.sync_warning(),
        }))
    }

    /// Edit note sections by heading path (all-or-nothing).
    #[tool(
        description = "Edit sections by heading_path: replace/append/delete (by scope body|section), insert_before/insert_after, rename (new_heading), move (to a destination section). append continues the section's text (after its last non-blank line, keeping the blank lines before the next heading); to append prose to the section itself use scope:\"body\" — with scope:\"section\" plain text joins the last subsection. replace keeps the blank lines the replaced span was framed by, so content need not carry the separator under the heading. insert_*/move place a sibling block, blank-line-separated on both sides. All-or-nothing: any rejected edit (unresolved/ambiguous heading, overlap, HASH_MISMATCH, HEADING_LEVEL) rejects the whole batch and nothing is written. Pass expected_hash from read_sections for optimistic concurrency.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn edit_sections(
        &self,
        Parameters(req): Parameters<EditSectionsRequest>,
    ) -> Result<Json<EditSectionsResponse>, ErrorData> {
        batch_limit(req.edits.len())?;
        let _guard = self.lock().write().await;
        let mut r = self.run_edit_sections(&req.edits).await;
        r.sync_warning = self.sync_warning();
        Ok(Json(r))
    }

    /// Replace literal text inside note bodies (all-or-nothing).
    #[tool(
        description = "Change literal text in place — the cheap path for typos, renamed terms, and corrected links: no reading the note first, no resending the surrounding text. One item = one (note, find) substitution inside the note body; frontmatter is never searched (use edit_properties). find is matched byte-for-byte — no regex, no case folding, and no CJK spacing/markup folding, so a search_notes hit for 전역지침 will not match the note's '전역 지침'. By default find must occur exactly once: 0 matches reject NOT_FOUND, 2+ reject AMBIGUOUS — narrow it with a longer find or a heading_path (+scope body|section, with optional expected_hash), or pass replace_all:true / expected_count:<n> to take every match. All-or-nothing: any rejected item (NOT_FOUND, AMBIGUOUS, COUNT_MISMATCH, HASH_MISMATCH, OVERLAP with another item) rejects the whole batch and nothing is written. Returns per item the replacement count plus the first few changed lines, numbered in the resulting note — never the note body. dry_run:true reports exactly that without writing. Structural change (adding, deleting, moving, renaming sections) belongs to edit_sections.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn replace_text(
        &self,
        Parameters(req): Parameters<ReplaceTextRequest>,
    ) -> Result<Json<ReplaceTextResponse>, ErrorData> {
        batch_limit(req.replaces.len())?;
        let _guard = self.lock().write().await;
        let mut r = self.run_replace_text(&req.replaces, req.dry_run).await;
        r.sync_warning = self.sync_warning();
        Ok(Json(r))
    }

    /// Set or remove frontmatter properties (all-or-nothing).
    #[tool(
        description = "Set or remove top-level frontmatter properties. One item = one (note, key): value present (even null) sets it, value omitted removes it. All-or-nothing: removing an absent key or editing over broken YAML rejects the whole batch.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn edit_properties(
        &self,
        Parameters(req): Parameters<EditPropertiesRequest>,
    ) -> Result<Json<EditPropertiesResponse>, ErrorData> {
        batch_limit(req.edits.len())?;
        let _guard = self.lock().write().await;
        let mut r = self.run_edit_properties(&req.edits).await;
        r.sync_warning = self.sync_warning();
        Ok(Json(r))
    }
}

impl MdServer {
    fn append_one(&self, item: &AppendInput) -> AppendResult {
        if md_core::Vault::is_internal_path(&item.path) {
            return AppendResult {
                path: item.path.clone(),
                appended: false,
                error: Some(ApiError {
                    code: Code::Traversal.as_str().to_string(),
                    message: "cannot target a protected directory".to_string(),
                    index: None,
                }),
            };
        }
        let base = match self.vault().read_note(&item.path) {
            Ok(s) => s,
            Err(e) if e.code == Code::NotFound && item.create_if_missing => String::new(),
            Err(e) => {
                return AppendResult {
                    path: item.path.clone(),
                    appended: false,
                    error: Some(ApiError::from_core(&e)),
                };
            }
        };
        let combined =
            md_core::text::normalize_newlines(&format!("{base}{}", item.content)).into_owned();
        // Bound the resulting note, so repeated appends can't grow it unbounded.
        if combined.len() > MAX_WRITE_BYTES {
            return AppendResult {
                path: item.path.clone(),
                appended: false,
                error: Some(ApiError::from_core(&write_size_error(
                    "note",
                    combined.len(),
                ))),
            };
        }
        match self.vault().write_atomic(&item.path, combined.as_bytes()) {
            Ok(()) => AppendResult {
                path: item.path.clone(),
                appended: true,
                error: None,
            },
            Err(e) => AppendResult {
                path: item.path.clone(),
                appended: false,
                error: Some(ApiError::from_core(&e)),
            },
        }
    }

    async fn run_edit_sections(&self, edits: &[EditItem]) -> EditSectionsResponse {
        // Group edits by path, keeping each edit's global index.
        let mut by_path: BTreeMap<&str, Vec<(usize, &EditItem)>> = BTreeMap::new();
        for (i, e) in edits.iter().enumerate() {
            by_path.entry(&e.path).or_default().push((i, e));
        }

        let mut errors: Vec<ApiError> = Vec::new();
        let mut writes: Vec<Op> = Vec::new();
        let mut applied: Vec<AppliedEdit> = Vec::new();

        for (path, items) in &by_path {
            // Convert to core edits; collect conversion errors (e.g. move).
            let mut core_edits = Vec::new();
            let mut conv_ok = true;
            for (gi, item) in items {
                match to_core_edit(item) {
                    Ok(ce) => core_edits.push(ce),
                    Err(mut e) => {
                        e.index = Some(*gi);
                        errors.push(e);
                        conv_ok = false;
                    }
                }
            }
            if !conv_ok {
                continue;
            }

            let source = match self.vault().read_note(path) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(ApiError::at(items[0].0, &e));
                    continue;
                }
            };
            match patch::patch_sections(&source, &core_edits) {
                Ok(new_source) if new_source.len() > MAX_WRITE_BYTES => {
                    // The edit would grow the note past the write limit; reject
                    // the whole batch (all-or-nothing) at the first item.
                    errors.push(ApiError::at(
                        items[0].0,
                        &write_size_error("edited note", new_source.len()),
                    ));
                }
                Ok(new_source) => {
                    for (li, (gi, item)) in items.iter().enumerate() {
                        let _ = li;
                        let (new_path, hash) = edit_outcome(&new_source, item);
                        applied.push(AppliedEdit {
                            index: *gi,
                            path: (*path).to_string(),
                            new_heading_path: new_path,
                            content_hash: hash,
                        });
                    }
                    writes.push(Op::Write {
                        path: (*path).to_string(),
                        content: new_source.into_bytes(),
                    });
                }
                Err(batch_errors) => {
                    for be in batch_errors {
                        let gi = items[be.index].0;
                        errors.push(ApiError::at(gi, &be.error));
                    }
                }
            }
        }

        if !errors.is_empty() {
            errors.sort_by_key(|e| e.index);
            return EditSectionsResponse {
                ok: false,
                applied: vec![],
                errors,
                sync_warning: None,
            };
        }
        match self.vault().commit_batch(&writes) {
            Ok(receipt) => {
                let ops = EventOp::from_outcomes(&receipt.outcomes);
                self.emit_event("edit_sections", Some(&receipt.batch_id), &ops);
                self.auto_commit("edit_sections", &ops, &edits).await;
            }
            Err(e) => {
                return EditSectionsResponse {
                    ok: false,
                    applied: vec![],
                    errors: vec![ApiError::from_core(&e)],
                    sync_warning: None,
                };
            }
        }
        applied.sort_by_key(|a| a.index);
        EditSectionsResponse {
            ok: true,
            applied,
            errors: vec![],
            sync_warning: None,
        }
    }

    async fn run_replace_text(&self, items: &[ReplaceItem], dry_run: bool) -> ReplaceTextResponse {
        // Group by path, keeping each item's global index: one note is read,
        // rewritten, and written once however many items target it.
        let mut by_path: BTreeMap<&str, Vec<(usize, &ReplaceItem)>> = BTreeMap::new();
        for (i, it) in items.iter().enumerate() {
            by_path.entry(&it.path).or_default().push((i, it));
        }

        let mut errors: Vec<ApiError> = Vec::new();
        let mut writes: Vec<Op> = Vec::new();
        let mut applied: Vec<AppliedReplace> = Vec::new();

        for (path, group) in &by_path {
            let source = match self.vault().read_note(path) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(ApiError::at(group[0].0, &e));
                    continue;
                }
            };
            let core: Vec<md_core::Replacement> = group
                .iter()
                .map(|(_, it)| to_core_replacement(it))
                .collect();
            match md_core::replace_text(&source, &core) {
                Ok((new_source, _)) if new_source.len() > MAX_WRITE_BYTES => {
                    // The replacement would grow the note past the write limit;
                    // reject the whole batch (all-or-nothing) at the first item.
                    errors.push(ApiError::at(
                        group[0].0,
                        &write_size_error("replaced note", new_source.len()),
                    ));
                }
                Ok((new_source, hits)) => {
                    for (li, (gi, _)) in group.iter().enumerate() {
                        applied.push(AppliedReplace {
                            index: *gi,
                            path: (*path).to_string(),
                            replaced: hits[li].len(),
                            hits: hits[li]
                                .iter()
                                .take(MAX_HITS)
                                .map(|h| ReplaceHit {
                                    line: h.line,
                                    text: truncate_hit(&h.text),
                                })
                                .collect(),
                        });
                    }
                    writes.push(Op::Write {
                        path: (*path).to_string(),
                        content: new_source.into_bytes(),
                    });
                }
                Err(batch_errors) => {
                    for be in batch_errors {
                        errors.push(ApiError::at(group[be.index].0, &be.error));
                    }
                }
            }
        }

        if !errors.is_empty() {
            errors.sort_by_key(|e| e.index);
            return ReplaceTextResponse {
                ok: false,
                applied: vec![],
                errors,
                dry_run,
                sync_warning: None,
            };
        }
        applied.sort_by_key(|a| a.index);
        if dry_run {
            return ReplaceTextResponse {
                ok: true,
                applied,
                errors: vec![],
                dry_run: true,
                sync_warning: None,
            };
        }
        match self.vault().commit_batch(&writes) {
            Ok(receipt) => {
                let ops = EventOp::from_outcomes(&receipt.outcomes);
                self.emit_event("replace_text", Some(&receipt.batch_id), &ops);
                self.auto_commit("replace_text", &ops, &items).await;
            }
            Err(e) => {
                return ReplaceTextResponse {
                    ok: false,
                    applied: vec![],
                    errors: vec![ApiError::from_core(&e)],
                    dry_run: false,
                    sync_warning: None,
                };
            }
        }
        ReplaceTextResponse {
            ok: true,
            applied,
            errors: vec![],
            dry_run: false,
            sync_warning: None,
        }
    }

    async fn run_edit_properties(&self, edits: &[PropertyEdit]) -> EditPropertiesResponse {
        // Apply per path in order, accumulating into one new content per path.
        let mut by_path: BTreeMap<&str, Vec<(usize, &PropertyEdit)>> = BTreeMap::new();
        for (i, e) in edits.iter().enumerate() {
            by_path.entry(&e.path).or_default().push((i, e));
        }

        let mut errors: Vec<ApiError> = Vec::new();
        let mut writes: Vec<Op> = Vec::new();
        let mut applied: Vec<PropertyApplied> = Vec::new();

        for (path, items) in &by_path {
            let mut source = match self.vault().read_note(path) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(ApiError::at(items[0].0, &e));
                    continue;
                }
            };
            let mut path_ok = true;
            for (gi, edit) in items {
                let result = match &edit.value {
                    Some(v) => frontmatter::set_property(&source, &edit.key, v),
                    None => {
                        // Removing an absent key is a rejection (not a silent no-op).
                        match frontmatter::has_property(&source, &edit.key) {
                            Ok(true) => frontmatter::remove_property(&source, &edit.key),
                            Ok(false) => Err(md_core::Error::not_found(format!(
                                "cannot remove absent key: {}",
                                edit.key
                            ))),
                            Err(e) => Err(e),
                        }
                    }
                };
                match result {
                    Ok(new_source) => {
                        source = new_source;
                        applied.push(PropertyApplied {
                            index: *gi,
                            path: (*path).to_string(),
                            key: edit.key.clone(),
                        });
                    }
                    Err(e) => {
                        errors.push(ApiError::at(*gi, &e));
                        path_ok = false;
                        break;
                    }
                }
            }
            if path_ok && source.len() > MAX_WRITE_BYTES {
                errors.push(ApiError::at(
                    items[0].0,
                    &write_size_error("note", source.len()),
                ));
                path_ok = false;
            }
            if path_ok {
                writes.push(Op::Write {
                    path: (*path).to_string(),
                    content: source.into_bytes(),
                });
            }
        }

        if !errors.is_empty() {
            errors.sort_by_key(|e| e.index);
            return EditPropertiesResponse {
                ok: false,
                applied: vec![],
                errors,
                sync_warning: None,
            };
        }
        match self.vault().commit_batch(&writes) {
            Ok(receipt) => {
                let ops = EventOp::from_outcomes(&receipt.outcomes);
                self.emit_event("edit_properties", Some(&receipt.batch_id), &ops);
                self.auto_commit("edit_properties", &ops, &edits).await;
            }
            Err(e) => {
                return EditPropertiesResponse {
                    ok: false,
                    applied: vec![],
                    errors: vec![ApiError::from_core(&e)],
                    sync_warning: None,
                };
            }
        }
        applied.sort_by_key(|a| a.index);
        EditPropertiesResponse {
            ok: true,
            applied,
            errors: vec![],
            sync_warning: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use md_core::Vault;
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::json;

    fn server(notes: &[(&str, &str)]) -> (tempfile::TempDir, MdServer) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        for (p, b) in notes {
            vault.write_atomic(p, b.as_bytes()).unwrap();
        }
        (dir, MdServer::new(vault))
    }

    #[test]
    fn condensed_write_responses_satisfy_their_schemas() {
        use crate::envelope::assert_condensed_satisfies_schema;
        use rmcp::schemars::schema_for;
        assert_condensed_satisfies_schema(
            schema_for!(EditSectionsResponse),
            EditSectionsResponse {
                ok: true,
                applied: vec![],
                errors: vec![],
                sync_warning: None,
            },
        );
        assert_condensed_satisfies_schema(
            schema_for!(EditPropertiesResponse),
            EditPropertiesResponse {
                ok: true,
                applied: vec![],
                errors: vec![],
                sync_warning: None,
            },
        );
        assert_condensed_satisfies_schema(
            schema_for!(ReplaceTextResponse),
            ReplaceTextResponse {
                ok: true,
                applied: vec![],
                errors: vec![],
                dry_run: false,
                sync_warning: None,
            },
        );
    }

    #[tokio::test]
    async fn batch_over_100_is_rejected() {
        let (_d, s) = server(&[]);
        let notes: Vec<NoteInput> = (0..101)
            .map(|i| NoteInput {
                path: format!("n{i}.md"),
                content: Some("x".into()),
                frontmatter: None,
                base: None,
            })
            .collect();
        let result = s
            .create_notes(Parameters(CreateNotesRequest {
                notes,
                overwrite: false,
            }))
            .await;
        let err = result.err().expect("over-limit batch must be rejected");
        assert!(format!("{err:?}").contains("exceeds"), "got: {err:?}");
    }

    #[tokio::test]
    async fn the_tool_surface_obeys_the_vault_own_path_rules() {
        use unicode_normalization::UnicodeNormalization;

        let (dir, s) = server(&[]);
        let decomposed: String = "노트.md".nfd().collect();
        assert_ne!(decomposed, "노트.md", "the fixture must actually differ");

        let result = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![
                    NoteInput {
                        path: "script.sh".to_string(),
                        content: Some("echo hi\n".to_string()),
                        frontmatter: None,
                        base: None,
                    },
                    NoteInput {
                        path: decomposed,
                        content: Some("body\n".to_string()),
                        frontmatter: None,
                        base: None,
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap();

        assert!(
            !result.0.created[0].created,
            "the rule that decides what a listing shows belongs to the vault, so \
             every way in obeys it — not only the transfer API"
        );
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n == "script.sh"), "got {names:?}");
        assert!(
            names.iter().any(|n| n == "노트.md"),
            "a decomposed name written through the tool surface is composed too: \
             got {names:?}"
        );
    }

    #[tokio::test]
    async fn oversized_writes_are_rejected_per_tool() {
        let (_d, s) = server(&[("seed.md", "# A\nbody\n")]);
        let big = "z".repeat(MAX_WRITE_BYTES + 1);

        // create: per-item TOO_LARGE, siblings unaffected (partial success).
        let r = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![
                    NoteInput {
                        path: "big.md".into(),
                        content: Some(big.clone()),
                        frontmatter: None,
                        base: None,
                    },
                    NoteInput {
                        path: "small.md".into(),
                        content: Some("ok\n".into()),
                        frontmatter: None,
                        base: None,
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(!r.created[0].created);
        assert_eq!(r.created[0].error.as_ref().unwrap().code, "TOO_LARGE");
        assert!(r.created[1].created);
        assert!(!s.vault().exists("big.md").unwrap());

        // append: bounded by the resulting note size.
        let r = s
            .append_notes(Parameters(AppendNotesRequest {
                appends: vec![AppendInput {
                    path: "seed.md".into(),
                    content: big.clone(),
                    create_if_missing: false,
                }],
            }))
            .await
            .unwrap()
            .0;
        assert!(!r.appended[0].appended);
        assert_eq!(r.appended[0].error.as_ref().unwrap().code, "TOO_LARGE");
        assert_eq!(s.vault().read_note("seed.md").unwrap(), "# A\nbody\n");

        // edit_sections: all-or-nothing rejection.
        let r = s
            .edit_sections(Parameters(EditSectionsRequest {
                edits: vec![EditItem {
                    path: "seed.md".into(),
                    heading_path: vec!["A".into()],
                    occurrence: None,
                    operation: OperationArg::Replace,
                    scope: ScopeArg::Body,
                    content: Some(big.clone()),
                    new_heading: None,
                    destination: None,
                    expected_hash: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        assert!(!r.ok);
        assert_eq!(r.errors[0].code, "TOO_LARGE");
        assert_eq!(s.vault().read_note("seed.md").unwrap(), "# A\nbody\n");
    }

    #[tokio::test]
    async fn create_notes_writes_body_and_frontmatter() {
        let (_d, s) = server(&[]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "n.md".into(),
                    content: Some("# Body\n".into()),
                    frontmatter: Some(json!({"status": "draft"})),
                    base: None,
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(resp.created[0].created);
        assert_eq!(
            s.vault().read_note("n.md").unwrap(),
            "---\nstatus: draft\n---\n# Body\n"
        );
    }

    #[tokio::test]
    async fn create_notes_refuses_existing_and_double_frontmatter() {
        let (_d, s) = server(&[("exists.md", "old")]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![
                    NoteInput {
                        path: "exists.md".into(),
                        content: Some("new".into()),
                        frontmatter: None,
                        base: None,
                    },
                    NoteInput {
                        path: "dbl.md".into(),
                        content: Some("---\nx: 1\n---\nbody\n".into()),
                        frontmatter: None,
                        base: None,
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.created[0].error.as_ref().unwrap().code, "CONFLICT");
        assert_eq!(resp.created[1].error.as_ref().unwrap().code, "CONFLICT");
        assert_eq!(s.vault().read_note("exists.md").unwrap(), "old");
    }

    #[tokio::test]
    async fn create_notes_base_copies_a_note_verbatim() {
        let tpl = "---\nstatus: draft\n---\n# Tpl\nbody\n";
        let (_d, s) = server(&[("tpl.md", tpl)]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "new.md".into(),
                    content: None,
                    frontmatter: None,
                    base: Some("tpl.md".into()),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert!(resp.created[0].created, "{:?}", resp.created[0].error);
        assert_eq!(s.vault().read_note("new.md").unwrap(), tpl);
        assert_eq!(s.vault().read_note("tpl.md").unwrap(), tpl);
    }

    #[tokio::test]
    async fn create_notes_base_excludes_content_and_frontmatter() {
        let (_d, s) = server(&[("tpl.md", "body\n")]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![
                    NoteInput {
                        path: "a.md".into(),
                        content: Some("x\n".into()),
                        frontmatter: None,
                        base: Some("tpl.md".into()),
                    },
                    NoteInput {
                        path: "b.md".into(),
                        content: None,
                        frontmatter: Some(json!({"k": 1})),
                        base: Some("tpl.md".into()),
                    },
                ],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.created[0].error.as_ref().unwrap().code, "CONFLICT");
        assert_eq!(resp.created[1].error.as_ref().unwrap().code, "CONFLICT");
        assert!(!s.vault().exists("a.md").unwrap());
        assert!(!s.vault().exists("b.md").unwrap());
    }

    #[tokio::test]
    async fn create_notes_base_missing_is_not_found() {
        let (_d, s) = server(&[]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "new.md".into(),
                    content: None,
                    frontmatter: None,
                    base: Some("absent.md".into()),
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.created[0].error.as_ref().unwrap().code, "NOT_FOUND");
        assert!(!s.vault().exists("new.md").unwrap());
    }

    #[tokio::test]
    async fn create_notes_without_content_or_base_is_rejected() {
        let (_d, s) = server(&[]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "new.md".into(),
                    content: None,
                    frontmatter: None,
                    base: None,
                }],
                overwrite: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(
            resp.created[0].error.as_ref().unwrap().code,
            "MISSING_CONTENT"
        );
        assert!(!s.vault().exists("new.md").unwrap());
    }

    #[tokio::test]
    async fn append_notes_appends_and_creates() {
        let (_d, s) = server(&[("log.md", "line1\n")]);
        let resp = s
            .append_notes(Parameters(AppendNotesRequest {
                appends: vec![
                    AppendInput {
                        path: "log.md".into(),
                        content: "line2\n".into(),
                        create_if_missing: false,
                    },
                    AppendInput {
                        path: "new.md".into(),
                        content: "fresh\n".into(),
                        create_if_missing: true,
                    },
                    AppendInput {
                        path: "absent.md".into(),
                        content: "x".into(),
                        create_if_missing: false,
                    },
                ],
            }))
            .await
            .unwrap()
            .0;
        assert!(resp.appended[0].appended && resp.appended[1].appended);
        assert!(!resp.appended[2].appended);
        assert_eq!(s.vault().read_note("log.md").unwrap(), "line1\nline2\n");
        assert_eq!(s.vault().read_note("new.md").unwrap(), "fresh\n");
    }

    #[tokio::test]
    async fn edit_sections_applies_or_rejects_whole_batch() {
        let (_d, s) = server(&[("a.md", "# A\nold\n")]);
        // Good edit succeeds.
        let ok = s
            .edit_sections(Parameters(EditSectionsRequest {
                edits: vec![EditItem {
                    path: "a.md".into(),
                    heading_path: vec!["A".into()],
                    occurrence: None,
                    operation: OperationArg::Replace,
                    scope: ScopeArg::Body,
                    content: Some("new".into()),
                    new_heading: None,
                    destination: None,
                    expected_hash: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok);
        assert_eq!(s.vault().read_note("a.md").unwrap(), "# A\nnew\n");

        // A batch with an unresolved heading rejects entirely; the note is untouched.
        let bad = s
            .edit_sections(Parameters(EditSectionsRequest {
                edits: vec![
                    EditItem {
                        path: "a.md".into(),
                        heading_path: vec!["A".into()],
                        occurrence: None,
                        operation: OperationArg::Replace,
                        scope: ScopeArg::Body,
                        content: Some("zzz".into()),
                        new_heading: None,
                        destination: None,
                        expected_hash: None,
                    },
                    EditItem {
                        path: "a.md".into(),
                        heading_path: vec!["Nope".into()],
                        occurrence: None,
                        operation: OperationArg::Replace,
                        scope: ScopeArg::Body,
                        content: Some("x".into()),
                        new_heading: None,
                        destination: None,
                        expected_hash: None,
                    },
                ],
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert!(!bad.errors.is_empty());
        assert_eq!(s.vault().read_note("a.md").unwrap(), "# A\nnew\n");
    }

    #[tokio::test]
    async fn edit_sections_move_relocates_a_section() {
        let (_d, s) = server(&[("a.md", "# A\nax\n# B\nbx\n")]);
        let resp = s
            .edit_sections(Parameters(EditSectionsRequest {
                edits: vec![EditItem {
                    path: "a.md".into(),
                    heading_path: vec!["A".into()],
                    occurrence: None,
                    operation: OperationArg::Move,
                    scope: ScopeArg::Section,
                    content: None,
                    new_heading: None,
                    destination: Some(DestinationArg {
                        heading_path: vec!["B".into()],
                        occurrence: None,
                        position: PositionArg::After,
                    }),
                    expected_hash: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        assert!(ok_or_dump(&resp));
        assert_eq!(s.vault().read_note("a.md").unwrap(), "# B\nbx\n\n# A\nax\n");
        // The move echoes the section's new heading path and content_hash.
        assert_eq!(
            resp.applied[0].new_heading_path,
            Some(vec!["A".to_string()])
        );
        assert!(resp.applied[0].content_hash.is_some());
    }

    fn ok_or_dump(r: &EditSectionsResponse) -> bool {
        assert!(r.ok, "edit failed: {:?}", r.errors);
        r.ok
    }

    #[tokio::test]
    async fn edit_properties_set_remove_and_reject_absent() {
        let (_d, s) = server(&[("p.md", "---\na: 1\n---\nbody\n")]);
        // Set b, remove a.
        let ok = s
            .edit_properties(Parameters(EditPropertiesRequest {
                edits: vec![
                    PropertyEdit {
                        path: "p.md".into(),
                        key: "b".into(),
                        value: Some(json!("two")),
                    },
                    PropertyEdit {
                        path: "p.md".into(),
                        key: "a".into(),
                        value: None,
                    },
                ],
            }))
            .await
            .unwrap()
            .0;
        assert!(ok.ok);
        let fm = frontmatter::parse(&s.vault().read_note("p.md").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(fm["b"], json!("two"));
        assert!(fm.as_object().unwrap().get("a").is_none());

        // Removing an absent key rejects the whole batch.
        let bad = s
            .edit_properties(Parameters(EditPropertiesRequest {
                edits: vec![PropertyEdit {
                    path: "p.md".into(),
                    key: "ghost".into(),
                    value: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        assert!(!bad.ok);
        assert_eq!(bad.errors[0].code, "NOT_FOUND");
    }

    fn replace_item(path: &str, find: &str, replace: &str) -> ReplaceItem {
        ReplaceItem {
            path: path.into(),
            find: find.into(),
            replace: replace.into(),
            heading_path: vec![],
            occurrence: None,
            scope: ScopeArg::Section,
            replace_all: false,
            expected_count: None,
            expected_hash: None,
        }
    }

    async fn run_replace(
        s: &MdServer,
        replaces: Vec<ReplaceItem>,
        dry_run: bool,
    ) -> ReplaceTextResponse {
        s.replace_text(Parameters(ReplaceTextRequest { replaces, dry_run }))
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn replace_text_fixes_a_typo_and_reports_the_new_line() {
        let (_d, s) = server(&[("n.md", "# A\nthe qiuck fox\n")]);
        let r = run_replace(&s, vec![replace_item("n.md", "qiuck", "quick")], false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.applied[0].replaced, 1);
        assert_eq!(r.applied[0].hits[0].line, 2);
        assert_eq!(r.applied[0].hits[0].text, "the quick fox");
        assert_eq!(s.vault().read_note("n.md").unwrap(), "# A\nthe quick fox\n");
    }

    #[tokio::test]
    async fn replace_text_rejects_the_whole_batch_on_one_bad_item() {
        let (_d, s) = server(&[("a.md", "one\n"), ("b.md", "two two\n")]);
        let r = run_replace(
            &s,
            vec![
                replace_item("a.md", "one", "ONE"),
                replace_item("b.md", "two", "TWO"),
            ],
            false,
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.errors[0].code, "AMBIGUOUS");
        assert_eq!(r.errors[0].index, Some(1));
        // The healthy item's note is untouched too.
        assert_eq!(s.vault().read_note("a.md").unwrap(), "one\n");
    }

    #[tokio::test]
    async fn replace_text_dry_run_reports_without_writing() {
        let (_d, s) = server(&[("n.md", "cat\ncat\n")]);
        let item = ReplaceItem {
            replace_all: true,
            ..replace_item("n.md", "cat", "dog")
        };
        let r = run_replace(&s, vec![item], true).await;
        assert!(r.ok, "{:?}", r.errors);
        assert!(r.dry_run);
        assert_eq!(r.applied[0].replaced, 2);
        assert_eq!(s.vault().read_note("n.md").unwrap(), "cat\ncat\n");
    }

    #[tokio::test]
    async fn replace_text_caps_and_truncates_the_reported_hits() {
        let long = "y".repeat(MAX_HIT_BYTES + 50);
        let body = format!("tpyo {long}\n").repeat(MAX_HITS + 3);
        let (_d, s) = server(&[("n.md", &body)]);
        let item = ReplaceItem {
            replace_all: true,
            ..replace_item("n.md", "tpyo", "typo")
        };
        let r = run_replace(&s, vec![item], false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(r.applied[0].replaced, MAX_HITS + 3);
        assert_eq!(r.applied[0].hits.len(), MAX_HITS);
        let text = &r.applied[0].hits[0].text;
        assert!(
            text.len() <= MAX_HIT_BYTES + 3,
            "hit text not truncated: {}",
            text.len()
        );
        assert!(text.ends_with('…'), "truncated hit must be marked: {text}");
    }

    #[tokio::test]
    async fn replace_text_scoped_to_a_section_leaves_the_rest_alone() {
        let (_d, s) = server(&[("n.md", "# A\nterm\n# B\nterm\n")]);
        let item = ReplaceItem {
            heading_path: vec!["B".into()],
            ..replace_item("n.md", "term", "TERM")
        };
        let r = run_replace(&s, vec![item], false).await;
        assert!(r.ok, "{:?}", r.errors);
        assert_eq!(
            s.vault().read_note("n.md").unwrap(),
            "# A\nterm\n# B\nTERM\n"
        );
    }
}
