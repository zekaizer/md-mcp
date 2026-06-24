//! Content-write tools: create_notes, append_notes, edit_sections, edit_properties.
//!
//! create/append are partial-success (independent items). edit_sections and
//! edit_properties are all-or-nothing: the whole batch is validated, then applied
//! through the transaction engine
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
use crate::envelope::{ApiError, batch_limit};
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

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateNotesRequest {
    pub notes: Vec<NoteInput>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteInput {
    pub path: String,
    /// The note body (no leading `---` frontmatter block — pass frontmatter separately).
    pub content: String,
    #[serde(default)]
    pub frontmatter: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateNotesResponse {
    pub created: Vec<CreateResult>,
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

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AppendNotesRequest {
    pub appends: Vec<AppendInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
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
    pub edits: Vec<EditItem>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditItem {
    pub path: String,
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub occurrence: Option<usize>,
    pub operation: OperationArg,
    #[serde(default)]
    pub scope: ScopeArg,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub new_heading: Option<String>,
    /// Destination for the `move` operation.
    #[serde(default)]
    pub destination: Option<DestinationArg>,
    #[serde(default)]
    pub expected_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
pub enum PositionArg {
    Before,
    After,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DestinationArg {
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub occurrence: Option<usize>,
    pub position: PositionArg,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditSectionsResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<AppliedEdit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
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

// --- edit_properties --------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditPropertiesRequest {
    pub edits: Vec<PropertyEdit>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PropertyEdit {
    pub path: String,
    pub key: String,
    /// Present (even `null`) sets the key; omitted removes it.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub value: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EditPropertiesResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<PropertyApplied>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
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
        description = "Create one or more notes. content is the body only; pass frontmatter as a separate object (a leading --- block in content is rejected). Partial success: a failing note does not block the others. overwrite:false refuses an existing note."
    )]
    pub async fn create_notes(
        &self,
        Parameters(req): Parameters<CreateNotesRequest>,
    ) -> Result<Json<CreateNotesResponse>, ErrorData> {
        batch_limit(req.notes.len())?;
        let _guard = self.lock().write().await;
        let created = req
            .notes
            .iter()
            .map(|note| {
                if body_has_frontmatter(&note.content) {
                    return CreateResult {
                        path: note.path.clone(),
                        created: false,
                        error: Some(ApiError {
                            code: Code::Conflict.as_str().to_string(),
                            message: "content must not start with a --- frontmatter block; pass frontmatter separately".to_string(),
                            index: None,
                        }),
                    };
                }
                let fm = note.frontmatter.clone().unwrap_or(Value::Null);
                let result = frontmatter::with_frontmatter(&note.content, &fm)
                    .and_then(|text| self.vault().create_note(&note.path, text.as_bytes(), req.overwrite));
                match result {
                    Ok(()) => CreateResult { path: note.path.clone(), created: true, error: None },
                    Err(e) => CreateResult { path: note.path.clone(), created: false, error: Some(ApiError::from_core(&e)) },
                }
            })
            .collect();
        Ok(Json(CreateNotesResponse { created }))
    }

    /// Append raw content to the end of notes (no separator inserted).
    #[tool(
        description = "Append raw content to the end of each note (no separator is inserted; include your own newline). Partial success. create_if_missing creates an absent note. For section-internal edits use edit_sections."
    )]
    pub async fn append_notes(
        &self,
        Parameters(req): Parameters<AppendNotesRequest>,
    ) -> Result<Json<AppendNotesResponse>, ErrorData> {
        batch_limit(req.appends.len())?;
        let _guard = self.lock().write().await;
        let mut appended = Vec::with_capacity(req.appends.len());
        for item in &req.appends {
            appended.push(self.append_one(item));
        }
        Ok(Json(AppendNotesResponse { appended }))
    }

    /// Edit note sections by heading path (all-or-nothing).
    #[tool(
        description = "Edit sections by heading_path: replace/append/delete (by scope body|section), insert_before/insert_after, rename. All-or-nothing: any rejected edit (unresolved/ambiguous heading, overlap, HASH_MISMATCH, HEADING_LEVEL) rejects the whole batch and nothing is written. Pass expected_hash from read_sections for optimistic concurrency."
    )]
    pub async fn edit_sections(
        &self,
        Parameters(req): Parameters<EditSectionsRequest>,
    ) -> Result<Json<EditSectionsResponse>, ErrorData> {
        batch_limit(req.edits.len())?;
        let _guard = self.lock().write().await;
        Ok(Json(self.run_edit_sections(&req.edits)))
    }

    /// Set or remove frontmatter properties (all-or-nothing).
    #[tool(
        description = "Set or remove top-level frontmatter properties. One item = one (note, key): value present (even null) sets it, value omitted removes it. All-or-nothing: removing an absent key or editing over broken YAML rejects the whole batch."
    )]
    pub async fn edit_properties(
        &self,
        Parameters(req): Parameters<EditPropertiesRequest>,
    ) -> Result<Json<EditPropertiesResponse>, ErrorData> {
        batch_limit(req.edits.len())?;
        let _guard = self.lock().write().await;
        Ok(Json(self.run_edit_properties(&req.edits)))
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
                    message: "cannot target the internal state directory".to_string(),
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

    fn run_edit_sections(&self, edits: &[EditItem]) -> EditSectionsResponse {
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
            };
        }
        if let Err(e) = self.vault().commit_batch(&writes) {
            return EditSectionsResponse {
                ok: false,
                applied: vec![],
                errors: vec![ApiError::from_core(&e)],
            };
        }
        applied.sort_by_key(|a| a.index);
        EditSectionsResponse {
            ok: true,
            applied,
            errors: vec![],
        }
    }

    fn run_edit_properties(&self, edits: &[PropertyEdit]) -> EditPropertiesResponse {
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
            };
        }
        if let Err(e) = self.vault().commit_batch(&writes) {
            return EditPropertiesResponse {
                ok: false,
                applied: vec![],
                errors: vec![ApiError::from_core(&e)],
            };
        }
        applied.sort_by_key(|a| a.index);
        EditPropertiesResponse {
            ok: true,
            applied,
            errors: vec![],
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

    #[tokio::test]
    async fn batch_over_100_is_rejected() {
        let (_d, s) = server(&[]);
        let notes: Vec<NoteInput> = (0..101)
            .map(|i| NoteInput {
                path: format!("n{i}.md"),
                content: "x".into(),
                frontmatter: None,
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
    async fn create_notes_writes_body_and_frontmatter() {
        let (_d, s) = server(&[]);
        let resp = s
            .create_notes(Parameters(CreateNotesRequest {
                notes: vec![NoteInput {
                    path: "n.md".into(),
                    content: "# Body\n".into(),
                    frontmatter: Some(json!({"status": "draft"})),
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
                        content: "new".into(),
                        frontmatter: None,
                    },
                    NoteInput {
                        path: "dbl.md".into(),
                        content: "---\nx: 1\n---\nbody\n".into(),
                        frontmatter: None,
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
        assert_eq!(s.vault().read_note("a.md").unwrap(), "# B\nbx\n# A\nax\n");
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
}
