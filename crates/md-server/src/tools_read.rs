//! Read tools: read_notes, read_outlines, read_sections.
//!
//! All read tools are partial-success: a failing item reports its state without
//! sinking its siblings. They acquire the read guard for the duration of their
//! vault reads ([ADR-0008](../../../docs/adr/0008-concurrency-and-isolation.md)).

use md_core::section::Scope;
use md_core::{Document, Vault, frontmatter};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::MdServer;
use crate::envelope::{ApiError, batch_limit, enforce_content_budget};
use crate::vault_path::VaultPath;

fn default_true() -> bool {
    true
}

/// `body` (lead body only) or `section` (lead body + subsections).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
pub enum ScopeArg {
    Body,
    #[default]
    Section,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::Body => Scope::Body,
            ScopeArg::Section => Scope::Section,
        }
    }
}

impl ScopeArg {
    fn as_str(self) -> &'static str {
        match self {
            ScopeArg::Body => "body",
            ScopeArg::Section => "section",
        }
    }
}

// --- read_notes -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadNotesRequest {
    /// Notes to read, each as path segments root to leaf (`["dir", "note.md"]`).
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub paths: Vec<VaultPath>,
    /// Include the note body (frontmatter block excluded).
    #[serde(default = "default_true")]
    pub include_body: bool,
    /// Include the parsed frontmatter object.
    #[serde(default = "default_true")]
    pub include_frontmatter: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadNotesResponse {
    pub notes: Vec<NoteRead>,
    /// Request indexes dropped to keep the response under the content budget;
    /// read those notes via read_outlines → read_sections instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted: Vec<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteRead {
    pub path: VaultPath,
    /// `true`/`false` when known; omitted when the read failed for a reason
    /// other than absence (e.g. TRAVERSAL), where existence is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

fn read_one_note(
    vault: &Vault,
    path: &VaultPath,
    include_body: bool,
    include_fm: bool,
) -> NoteRead {
    let raw = match path.rel().and_then(|rel| vault.read_note(&rel)) {
        Ok(raw) => raw,
        Err(e) if e.code == md_core::Code::NotFound => {
            return NoteRead {
                path: path.clone(),
                exists: Some(false),
                content: None,
                frontmatter: None,
                error: None,
            };
        }
        Err(e) => {
            return NoteRead {
                path: path.clone(),
                exists: None,
                content: None,
                frontmatter: None,
                error: Some(ApiError::from_core(&e)),
            };
        }
    };

    let normalized = md_core::text::normalize_newlines(&raw).into_owned();
    let content = include_body.then(|| {
        Document::parse(&normalized)
            .whole_body_span()
            .of(&normalized)
            .to_string()
    });

    let (frontmatter, error) = if include_fm {
        match frontmatter::parse(&normalized) {
            Ok(fm) => (fm, None),
            Err(e) => (None, Some(ApiError::from_core(&e))),
        }
    } else {
        (None, None)
    };

    NoteRead {
        path: path.clone(),
        exists: Some(true),
        content,
        frontmatter,
        error,
    }
}

// --- read_outlines ----------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadOutlinesRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub paths: Vec<VaultPath>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadOutlinesResponse {
    pub outlines: Vec<NoteOutline>,
    /// Request indexes dropped to keep the response under the content budget
    /// (a pathological note with thousands of headings); narrow the request
    /// or read the note via read_sections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted: Vec<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoteOutline {
    pub path: VaultPath,
    /// `true`/`false` when known; omitted when the read failed for a reason
    /// other than absence, where existence is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    /// One text row per heading, in document order: `<line> <#×level> <title>`.
    /// A following `↳ heading_path: [...]` row (plus `occurrence` when even
    /// the full path repeats) carries the address to pass to
    /// read_sections/edit_sections; without one, the address is `[title]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// Render outline entries as text rows. Titles cannot contain newlines
/// (the source is LF-normalized before parsing), so row starts are fully
/// emitter-controlled: heading rows begin with a digit, address rows with
/// `↳` — disjoint prefixes, titles verbatim to end of line, no escaping.
fn render_outline(entries: &[md_core::OutlineEntry]) -> String {
    let width = entries
        .iter()
        .map(|e| e.line)
        .max()
        .map_or(0, |m| m.to_string().len());
    let mut rows = Vec::with_capacity(entries.len());
    for e in entries {
        let title = e.heading_path.last().map_or("", String::as_str);
        rows.push(format!(
            "{:>width$} {} {}",
            e.line,
            "#".repeat(usize::from(e.level)),
            title
        ));
        if e.heading_path.len() > 1 || e.occurrence.is_some() {
            let mut marker = format!(
                "{:>width$} \u{21b3} heading_path: {}",
                "",
                serde_json::to_string(&e.heading_path).unwrap_or_default()
            );
            if let Some(occ) = e.occurrence {
                marker.push_str(&format!(" occurrence: {occ}"));
            }
            rows.push(marker);
        }
    }
    rows.join("\n")
}

fn read_one_outline(vault: &Vault, path: &VaultPath) -> NoteOutline {
    match path.rel().and_then(|rel| vault.read_note(&rel)) {
        Ok(raw) => {
            let normalized = md_core::text::normalize_newlines(&raw).into_owned();
            let outline = render_outline(&Document::parse(&normalized).outline());
            NoteOutline {
                path: path.clone(),
                exists: Some(true),
                outline: Some(outline),
                error: None,
            }
        }
        Err(e) if e.code == md_core::Code::NotFound => NoteOutline {
            path: path.clone(),
            exists: Some(false),
            outline: None,
            error: None,
        },
        Err(e) => NoteOutline {
            path: path.clone(),
            exists: None,
            outline: None,
            error: Some(ApiError::from_core(&e)),
        },
    }
}

// --- read_sections ----------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadSectionsRequest {
    #[schemars(length(max = 100))] // batch cap — keep in sync with MAX_BATCH
    pub targets: Vec<SectionTarget>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SectionTarget {
    pub path: VaultPath,
    /// Heading path; empty addresses the root (whole body).
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub occurrence: Option<usize>,
    #[serde(default)]
    pub scope: ScopeArg,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadSectionsResponse {
    pub sections: Vec<SectionRead>,
    /// Request indexes dropped to keep the response under the content budget;
    /// re-read those targets with narrower sections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted: Vec<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SectionRead {
    pub path: VaultPath,
    pub heading_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<usize>,
    pub scope: String,
    /// `true`/`false` when known; omitted when the read failed for a reason
    /// other than absence, where existence is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_exists: Option<bool>,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Why the heading path did not resolve (e.g. `AMBIGUOUS` vs `NOT_FOUND`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

fn read_one_section(vault: &Vault, t: &SectionTarget) -> SectionRead {
    let base = SectionRead {
        path: t.path.clone(),
        heading_path: t.heading_path.clone(),
        occurrence: t.occurrence,
        scope: ScopeArg::as_str(t.scope).to_string(),
        note_exists: Some(false),
        found: false,
        content: None,
        content_hash: None,
        error: None,
    };

    let raw = match t.path.rel().and_then(|rel| vault.read_note(&rel)) {
        Ok(raw) => raw,
        Err(e) if e.code == md_core::Code::NotFound => return base,
        // A non-absence failure (e.g. TRAVERSAL): existence is unknown and the
        // reason must not be swallowed.
        Err(e) => {
            return SectionRead {
                note_exists: None,
                error: Some(ApiError::from_core(&e)),
                ..base
            };
        }
    };
    let normalized = md_core::text::normalize_newlines(&raw).into_owned();
    let doc = Document::parse(&normalized);

    // `Ok(None)` = the root; `Ok(Some(i))` = a resolved heading; `Err` keeps the
    // reason (AMBIGUOUS vs NOT_FOUND), which the spec separates.
    let resolved = if t.heading_path.is_empty() {
        Ok(None)
    } else {
        doc.resolve_heading(&t.heading_path, t.occurrence).map(Some)
    };

    match resolved {
        Ok(idx) => {
            let scope: Scope = t.scope.into();
            let content = doc.section_content(&normalized, idx, scope).to_string();
            let hash = doc.content_hash(&normalized, idx, scope);
            SectionRead {
                note_exists: Some(true),
                found: true,
                content: Some(content),
                content_hash: Some(hash),
                ..base
            }
        }
        Err(e) => SectionRead {
            note_exists: Some(true),
            found: false,
            error: Some(ApiError::from_core(&e)),
            ..base
        },
    }
}

#[tool_router(router = read_router, vis = "pub(crate)")]
impl MdServer {
    /// Read one or more notes in full (body and/or frontmatter).
    #[tool(
        description = "Read one or more notes by path (segments root to leaf). Returns each note's body (frontmatter excluded) and/or parsed frontmatter. Missing notes are reported with exists:false rather than failing the call.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub async fn read_notes(
        &self,
        Parameters(req): Parameters<ReadNotesRequest>,
    ) -> Result<Json<ReadNotesResponse>, ErrorData> {
        batch_limit(req.paths.len())?;
        let _guard = self.lock().read().await;
        let mut notes: Vec<NoteRead> = req
            .paths
            .iter()
            .map(|p| read_one_note(self.vault(), p, req.include_body, req.include_frontmatter))
            .collect();
        // A note's response weight is its body plus its serialized frontmatter
        // — a huge frontmatter must not slip past the budget.
        let omitted = enforce_content_budget(&mut notes, |n| {
            n.content.as_deref().map_or(0, str::len)
                + n.frontmatter
                    .as_ref()
                    .and_then(|f| serde_json::to_string(f).ok())
                    .map_or(0, |s| s.len())
        });
        Ok(Json(ReadNotesResponse { notes, omitted }))
    }

    /// Read the heading outline (table of contents) of one or more notes.
    #[tool(
        description = "Read the heading outline of one or more notes without their bodies. Each heading is one text row `<line> <#×level> <title>`; its address for read_sections/edit_sections is [title], unless a `↳ heading_path: [...]` row follows — then pass that array (and occurrence, if given) verbatim. Use this on a large note to pick sections before reading or editing.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub async fn read_outlines(
        &self,
        Parameters(req): Parameters<ReadOutlinesRequest>,
    ) -> Result<Json<ReadOutlinesResponse>, ErrorData> {
        batch_limit(req.paths.len())?;
        let _guard = self.lock().read().await;
        let mut outlines: Vec<NoteOutline> = req
            .paths
            .iter()
            .map(|p| read_one_outline(self.vault(), p))
            .collect();
        // An outline's cost is its rendered rows, not note content.
        let omitted =
            enforce_content_budget(&mut outlines, |o| o.outline.as_ref().map_or(0, String::len));
        Ok(Json(ReadOutlinesResponse { outlines, omitted }))
    }

    /// Read specific sections by heading path, with their content_hash.
    #[tool(
        description = "Read specific sections of notes by heading_path (empty = the whole body). Returns each section's content and content_hash for the chosen scope; pass the same scope and occurrence to edit_sections so expected_hash matches.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub async fn read_sections(
        &self,
        Parameters(req): Parameters<ReadSectionsRequest>,
    ) -> Result<Json<ReadSectionsResponse>, ErrorData> {
        batch_limit(req.targets.len())?;
        let _guard = self.lock().read().await;
        let mut sections: Vec<SectionRead> = req
            .targets
            .iter()
            .map(|t| read_one_section(self.vault(), t))
            .collect();
        let omitted =
            enforce_content_budget(&mut sections, |s| s.content.as_deref().map_or(0, str::len));
        Ok(Json(ReadSectionsResponse { sections, omitted }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_with(notes: &[(&str, &str)]) -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        for (path, body) in notes {
            vault.write_atomic(path, body.as_bytes()).unwrap();
        }
        (dir, vault)
    }

    #[test]
    fn condensed_read_responses_satisfy_their_schemas() {
        use crate::envelope::assert_condensed_satisfies_schema;
        use rmcp::schemars::schema_for;
        assert_condensed_satisfies_schema(
            schema_for!(ReadNotesResponse),
            ReadNotesResponse {
                notes: vec![],
                omitted: vec![],
            },
        );
        assert_condensed_satisfies_schema(
            schema_for!(ReadOutlinesResponse),
            ReadOutlinesResponse {
                outlines: vec![],
                omitted: vec![],
            },
        );
        assert_condensed_satisfies_schema(
            schema_for!(ReadSectionsResponse),
            ReadSectionsResponse {
                sections: vec![],
                omitted: vec![],
            },
        );
    }

    #[test]
    fn read_note_returns_body_without_frontmatter() {
        let (_d, v) = vault_with(&[("n.md", "---\ntitle: T\n---\n# Body\ntext\n")]);
        let r = read_one_note(&v, &"n.md".into(), true, true);
        assert_eq!(r.exists, Some(true));
        assert_eq!(r.content.as_deref(), Some("# Body\ntext\n"));
        assert_eq!(r.frontmatter.unwrap()["title"], serde_json::json!("T"));
    }

    #[test]
    fn read_note_missing_reports_exists_false() {
        let (_d, v) = vault_with(&[]);
        let r = read_one_note(&v, &"nope.md".into(), true, true);
        assert_eq!(r.exists, Some(false));
        assert!(r.error.is_none());
    }

    #[test]
    fn read_error_without_absence_leaves_existence_unknown() {
        // A SEGMENT failure says nothing about existence: don't claim true.
        let (_d, v) = vault_with(&[]);
        let r = read_one_note(&v, &"../outside.md".into(), true, true);
        assert_eq!(r.exists, None);
        assert_eq!(r.error.unwrap().code, "SEGMENT");

        let o = read_one_outline(&v, &"../outside.md".into());
        assert_eq!(o.exists, None);
        assert_eq!(o.error.unwrap().code, "SEGMENT");

        // read_sections used to swallow the error entirely and report
        // note_exists:false — indistinguishable from a missing note.
        let t = SectionTarget {
            path: "../outside.md".into(),
            heading_path: vec![],
            occurrence: None,
            scope: ScopeArg::Section,
        };
        let s = read_one_section(&v, &t);
        assert_eq!(s.note_exists, None);
        assert_eq!(s.error.unwrap().code, "SEGMENT");
    }

    #[test]
    fn read_note_broken_frontmatter_reports_error() {
        let (_d, v) = vault_with(&[("b.md", "---\nx: : :\n bad\n---\nbody\n")]);
        let r = read_one_note(&v, &"b.md".into(), true, true);
        assert_eq!(r.exists, Some(true));
        assert_eq!(r.error.unwrap().code, "FRONTMATTER_PARSE");
    }

    #[test]
    fn outline_renders_one_text_row_per_heading() {
        let (_d, v) = vault_with(&[("o.md", "# A\n## B\n# C\n")]);
        let o = read_one_outline(&v, &"o.md".into());
        assert_eq!(o.outline.as_deref(), Some("1 # A\n2 ## B\n3 # C"));
    }

    #[test]
    fn outline_right_aligns_line_numbers() {
        let (_d, v) = vault_with(&[("o.md", &format!("# A\n{}## B\n", "x\n".repeat(10)))]);
        let o = read_one_outline(&v, &"o.md".into());
        assert_eq!(o.outline.as_deref(), Some(" 1 # A\n12 ## B"));
    }

    #[test]
    fn outline_marks_shadowed_titles_with_address_row() {
        let (_d, v) = vault_with(&[("o.md", "# Q1\n## Status\n# Q2\n## Status\n")]);
        let o = read_one_outline(&v, &"o.md".into());
        assert_eq!(
            o.outline.as_deref(),
            Some(
                "1 # Q1\n\
                 2 ## Status\n  \u{21b3} heading_path: [\"Q1\",\"Status\"]\n\
                 3 # Q2\n\
                 4 ## Status\n  \u{21b3} heading_path: [\"Q2\",\"Status\"]"
            )
        );
    }

    #[test]
    fn outline_appends_occurrence_for_identical_chains() {
        let (_d, v) = vault_with(&[("o.md", "# A\n# A\n")]);
        let o = read_one_outline(&v, &"o.md".into());
        assert_eq!(
            o.outline.as_deref(),
            Some(
                "1 # A\n  \u{21b3} heading_path: [\"A\"] occurrence: 1\n\
                 2 # A\n  \u{21b3} heading_path: [\"A\"] occurrence: 2"
            )
        );
    }

    #[test]
    fn outline_of_headingless_note_is_empty_string() {
        let (_d, v) = vault_with(&[("o.md", "just text\n")]);
        let o = read_one_outline(&v, &"o.md".into());
        assert_eq!(o.outline.as_deref(), Some(""));
    }

    #[test]
    fn section_found_returns_content_and_hash() {
        let (_d, v) = vault_with(&[("s.md", "# A\nlead\n## B\nsub\n")]);
        let t = SectionTarget {
            path: "s.md".into(),
            heading_path: vec!["A".into()],
            occurrence: None,
            scope: ScopeArg::Body,
        };
        let s = read_one_section(&v, &t);
        assert_eq!(s.note_exists, Some(true));
        assert!(s.found);
        assert_eq!(s.content.as_deref(), Some("lead\n"));
        assert_eq!(s.content_hash.unwrap().len(), 8);
    }

    #[test]
    fn section_not_found_separates_from_note_missing() {
        let (_d, v) = vault_with(&[("s.md", "# A\n")]);
        let t = SectionTarget {
            path: "s.md".into(),
            heading_path: vec!["X".into()],
            occurrence: None,
            scope: ScopeArg::Section,
        };
        let s = read_one_section(&v, &t);
        assert_eq!(s.note_exists, Some(true));
        assert!(!s.found);

        let missing = SectionTarget {
            path: "no.md".into(),
            heading_path: vec!["A".into()],
            occurrence: None,
            scope: ScopeArg::Section,
        };
        let s2 = read_one_section(&v, &missing);
        assert_eq!(s2.note_exists, Some(false));
        assert!(!s2.found);
    }

    #[test]
    fn section_ambiguous_is_reported_distinctly() {
        let (_d, v) = vault_with(&[("s.md", "# A\n# A\n")]);
        let t = SectionTarget {
            path: "s.md".into(),
            heading_path: vec!["A".into()],
            occurrence: None,
            scope: ScopeArg::Section,
        };
        let s = read_one_section(&v, &t);
        assert_eq!(s.note_exists, Some(true));
        assert!(!s.found);
        assert_eq!(s.error.unwrap().code, "AMBIGUOUS");
    }

    #[tokio::test]
    async fn oversized_frontmatter_counts_against_the_budget() {
        let items: String = (0..3500)
            .map(|i| format!("- item-{i}-{}\n", "x".repeat(70)))
            .collect();
        let big = format!("---\ndata:\n{items}---\nsmall body\n");
        let (_d, v) = vault_with(&[("bigfm.md", &big), ("small.md", "# S\nok\n")]);
        let server = MdServer::new(v);
        let resp = server
            .read_notes(Parameters(ReadNotesRequest {
                paths: vec!["bigfm.md".into(), "small.md".into()],
                include_body: true,
                include_frontmatter: true,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.omitted, vec![0]);
        assert_eq!(resp.notes.len(), 1);
        assert_eq!(resp.notes[0].path, "small.md");
    }

    #[tokio::test]
    async fn oversized_outline_is_dropped_and_reported_omitted() {
        // ~6000 heading rows render past the content budget.
        let big: String = (0..6000)
            .map(|i| format!("# Heading number {i} with some padding\nx\n"))
            .collect();
        let (_d, v) = vault_with(&[("many.md", &big), ("small.md", "# S\nok\n")]);
        let server = MdServer::new(v);
        let resp = server
            .read_outlines(Parameters(ReadOutlinesRequest {
                paths: vec!["many.md".into(), "small.md".into()],
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.omitted, vec![0]);
        assert_eq!(resp.outlines.len(), 1);
        assert_eq!(resp.outlines[0].path, "small.md");
    }

    #[tokio::test]
    async fn oversized_read_drops_whole_items_and_reports_omitted() {
        let big = format!("# Big\n{}", "x".repeat(crate::envelope::MAX_CONTENT_BYTES));
        let (_d, v) = vault_with(&[("big.md", &big), ("small.md", "# S\nok\n")]);
        let server = MdServer::new(v);

        let resp = server
            .read_notes(Parameters(ReadNotesRequest {
                paths: vec!["big.md".into(), "small.md".into()],
                include_body: true,
                include_frontmatter: false,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.omitted, vec![0], "the oversized note is dropped whole");
        assert_eq!(resp.notes.len(), 1);
        assert_eq!(resp.notes[0].path, "small.md");
        assert_eq!(resp.notes[0].content.as_deref(), Some("# S\nok\n"));

        let resp = server
            .read_sections(Parameters(ReadSectionsRequest {
                targets: vec![
                    SectionTarget {
                        path: "small.md".into(),
                        heading_path: vec![],
                        occurrence: None,
                        scope: ScopeArg::Section,
                    },
                    SectionTarget {
                        path: "big.md".into(),
                        heading_path: vec![],
                        occurrence: None,
                        scope: ScopeArg::Section,
                    },
                ],
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(resp.omitted, vec![1]);
        assert_eq!(resp.sections.len(), 1);
        assert_eq!(resp.sections[0].path, "small.md");
    }

    #[tokio::test]
    async fn read_notes_tool_returns_all_items() {
        let (_d, v) = vault_with(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let server = MdServer::new(v);
        let resp = server
            .read_notes(Parameters(ReadNotesRequest {
                paths: vec!["a.md".into(), "missing.md".into()],
                include_body: true,
                include_frontmatter: false,
            }))
            .await
            .unwrap();
        assert_eq!(resp.0.notes.len(), 2);
        assert_eq!(resp.0.notes[0].exists, Some(true));
        assert_eq!(resp.0.notes[1].exists, Some(false));
    }
}
