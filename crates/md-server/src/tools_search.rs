//! Discovery tools: list_notes, search_notes.
//!
//! Both are single (non-batch) read tools. list_notes pages a sorted vault walk;
//! search_notes scans note bodies with an aho-corasick automaton and filters
//! frontmatter ([ADR-0010](../../../docs/adr/0010-search-strategy.md)). A CJK
//! query scans a second time over [`cjk_fold`]ed text, so spacing and inline
//! style inside a term do not hide it.

use aho_corasick::AhoCorasick;
use md_core::frontmatter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::MdServer;
use crate::cjk_fold;
use crate::envelope::limit_bounds;

fn default_recursive() -> bool {
    true
}
fn default_list_limit() -> usize {
    200
}
fn default_search_limit() -> usize {
    20
}
fn default_context_lines() -> usize {
    2
}

/// Maximum bytes in a `search_notes` query. A real query is a few keywords; this
/// cap is far above any legitimate one yet bounds the aho-corasick automaton
/// build, whose `ascii_case_insensitive` construction is superlinear in a single
/// long token (a multi-megabyte query otherwise wedges the request for tens of
/// seconds). Over-cap queries are rejected up front instead.
const MAX_QUERY_BYTES: usize = 1024;

// --- list_notes -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListNotesRequest {
    #[serde(default)]
    pub directory: String,
    #[serde(default = "default_recursive")]
    pub recursive: bool,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub include_dirs: bool,
    #[serde(default = "default_list_limit")]
    #[schemars(range(min = 1, max = 1000))] // server-enforced, see limit_bounds
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListNotesResponse {
    pub items: Vec<ListItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListItem {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub modified_time: String,
}

// --- search_notes -----------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
pub enum SearchMode {
    Content,
    Filename,
    #[default]
    Both,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SearchNotesRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default)]
    pub frontmatter: Option<Map<String, Value>>,
    #[serde(default)]
    pub frontmatter_exists: Option<Map<String, Value>>,
    #[serde(default = "default_search_limit")]
    #[schemars(range(min = 1, max = 100))] // server-enforced, see limit_bounds
    pub limit: usize,
    #[serde(default = "default_context_lines")]
    pub context_lines: usize,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SearchNotesResponse {
    pub items: Vec<SearchItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SearchItem {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Number of body lines with a keyword hit; the snippet shows only the
    /// first, so a value > 1 means there is more than what the snippet shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
}

#[tool_router(router = search_router, vis = "pub(crate)")]
impl MdServer {
    /// List notes (and optionally directories) by directory and glob.
    #[tool(
        description = "List notes under a directory (default the whole vault), optionally filtered by a glob (e.g. daily/**/*.md). Returns path-sorted items with size and modified time; pass next_cursor to page. Directories (include_dirs) end with /.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub async fn list_notes(
        &self,
        Parameters(req): Parameters<ListNotesRequest>,
    ) -> Result<Json<ListNotesResponse>, ErrorData> {
        limit_bounds(req.limit, 1000)?;
        let _guard = self.lock().read().await;
        let limit = req.limit;
        let entries = self
            .vault()
            .list_entries(
                &req.directory,
                req.recursive,
                req.glob.as_deref(),
                req.include_dirs,
            )
            .map_err(|e| ErrorData::invalid_params(e.message.clone(), None))?;

        let after = req.cursor.unwrap_or_default();
        let filtered: Vec<_> = entries
            .into_iter()
            .filter(|e| e.path.as_str() > after.as_str())
            .collect();
        let next_cursor = (filtered.len() > limit).then(|| filtered[limit - 1].path.clone());
        let items = filtered
            .into_iter()
            .take(limit)
            .map(|e| ListItem {
                path: e.path,
                size_bytes: e.size_bytes,
                modified_time: e.modified,
            })
            .collect();
        Ok(Json(ListNotesResponse { items, next_cursor }))
    }

    /// Search notes by content, filename, and/or frontmatter fields.
    #[tool(
        description = "Search notes by content keywords (whitespace-AND), filename substring, and/or frontmatter field filters (all combined with AND). Returns path-sorted matches with a snippet and the filtered frontmatter values; pass next_cursor to page. Provide at least one of query/frontmatter/frontmatter_exists. CJK keywords match regardless of spacing and inline style, so 전역지침 finds 전역 지침 and **전역** 지침.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    pub async fn search_notes(
        &self,
        Parameters(req): Parameters<SearchNotesRequest>,
    ) -> Result<Json<SearchNotesResponse>, ErrorData> {
        let has_query = req.query.as_deref().is_some_and(|q| !q.trim().is_empty());
        if !has_query && req.frontmatter.is_none() && req.frontmatter_exists.is_none() {
            return Err(ErrorData::invalid_params(
                "provide at least one of query, frontmatter, or frontmatter_exists",
                None,
            ));
        }
        if let Some(q) = req.query.as_deref()
            && q.len() > MAX_QUERY_BYTES
        {
            return Err(ErrorData::invalid_params(
                format!("query exceeds {MAX_QUERY_BYTES} bytes: {} bytes", q.len()),
                None,
            ));
        }
        limit_bounds(req.limit, 100)?;
        let _guard = self.lock().read().await;
        Ok(Json(self.run_search(&req)))
    }
}

impl MdServer {
    fn run_search(&self, req: &SearchNotesRequest) -> SearchNotesResponse {
        let has_query = req.query.as_deref().is_some_and(|q| !q.trim().is_empty());
        if !has_query && req.frontmatter.is_none() && req.frontmatter_exists.is_none() {
            return SearchNotesResponse {
                items: vec![],
                next_cursor: None,
            };
        }
        let limit = req.limit;

        // Keywords and scanned text are NFC-normalized (tool_spec §4): an NFC
        // query must find NFD-authored content synced from macOS.
        let keywords: Vec<String> = req
            .query
            .as_deref()
            .map(|q| {
                q.split_whitespace()
                    .map(|w| md_core::text::nfc(w).to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        let automaton = (!keywords.is_empty())
            .then(|| {
                AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .build(&keywords)
                    .ok()
            })
            .flatten();
        // Gate the second, folded scan on the only queries it can help.
        let query_has_cjk = keywords.iter().any(|k| cjk_fold::has_cjk(k));

        let Ok(entries) = self.vault().list_entries("", true, None, false) else {
            return SearchNotesResponse {
                items: vec![],
                next_cursor: None,
            };
        };

        let after = req.cursor.clone().unwrap_or_default();
        let mut matches: Vec<SearchItem> = Vec::new();
        for entry in entries {
            if entry.path.as_str() <= after.as_str() {
                continue;
            }
            let Ok(raw) = self.vault().read_note(&entry.path) else {
                continue;
            };
            let normalized = md_core::text::normalize_newlines(&raw).into_owned();
            let fm = frontmatter::parse(&normalized).ok().flatten();

            if !self.matches_frontmatter(req, fm.as_ref()) {
                continue;
            }

            let body = md_core::Document::parse(&normalized)
                .whole_body_span()
                .of(&normalized)
                .to_string();
            let body = md_core::text::nfc(&body).into_owned();
            let (text_ok, snippet, match_count) = self.match_query(
                req,
                &keywords,
                automaton.as_ref(),
                query_has_cjk,
                &entry.path,
                &body,
            );
            if has_query && !text_ok {
                continue;
            }

            let echoed_fm = req.frontmatter.as_ref().map(|filter| {
                let mut echo = Map::new();
                if let Some(obj) = fm.as_ref().and_then(Value::as_object) {
                    for k in filter.keys() {
                        if let Some(v) = obj.get(k) {
                            echo.insert(k.clone(), v.clone());
                        }
                    }
                }
                Value::Object(echo)
            });

            matches.push(SearchItem {
                path: entry.path,
                snippet,
                match_count,
                frontmatter: echoed_fm,
            });
            if matches.len() > limit {
                break; // we have enough to know there is a next page
            }
        }

        let next_cursor = (matches.len() > limit).then(|| matches[limit - 1].path.clone());
        matches.truncate(limit);
        SearchNotesResponse {
            items: matches,
            next_cursor,
        }
    }

    fn matches_frontmatter(&self, req: &SearchNotesRequest, fm: Option<&Value>) -> bool {
        let obj = fm.and_then(Value::as_object);
        if let Some(filter) = &req.frontmatter {
            for (k, expected) in filter {
                let Some(actual) = obj.and_then(|o| o.get(k)) else {
                    return false;
                };
                let ok = match actual {
                    Value::Array(items) => items.contains(expected),
                    other => other == expected,
                };
                if !ok {
                    return false;
                }
            }
        }
        if let Some(exists) = &req.frontmatter_exists {
            for (k, want) in exists {
                let present = obj.is_some_and(|o| o.contains_key(k));
                if want.as_bool().unwrap_or(true) != present {
                    return false;
                }
            }
        }
        true
    }

    fn match_query(
        &self,
        req: &SearchNotesRequest,
        keywords: &[String],
        automaton: Option<&AhoCorasick>,
        query_has_cjk: bool,
        path: &str,
        body: &str,
    ) -> (bool, Option<String>, Option<usize>) {
        let want_content = matches!(req.mode, SearchMode::Content | SearchMode::Both);
        let want_filename = matches!(req.mode, SearchMode::Filename | SearchMode::Both);
        let query_lower =
            md_core::text::nfc(req.query.as_deref().unwrap_or_default()).to_lowercase();

        let filename_hit = want_filename && !query_lower.is_empty() && {
            let path_lower = md_core::text::nfc(path).to_lowercase();
            path_lower.contains(&query_lower)
                || (query_has_cjk && folded_contains(&path_lower, &query_lower))
        };

        let mut content_hit = false;
        let mut snippet = None;
        let mut match_count = None;
        if want_content && let Some(ac) = automaton {
            let mut seen = vec![false; keywords.len()];
            let mut hits = std::collections::BTreeSet::new(); // original byte offsets
            for m in ac.find_iter(body) {
                seen[m.pattern().as_usize()] = true;
                hits.insert(m.start());
            }
            // Fold only what the raw scan left unanswered. The two scans are
            // unioned, never swapped: folding drops markers a query may itself
            // contain, so the shadow alone would lose raw hits.
            if query_has_cjk
                && !seen.iter().all(|&s| s)
                && let Some(folded) = cjk_fold::fold(body)
            {
                for m in ac.find_iter(folded.text()) {
                    seen[m.pattern().as_usize()] = true;
                    hits.insert(folded.to_original(m.start()));
                }
            }
            if seen.iter().all(|&s| s) {
                content_hit = true;
                snippet = hits
                    .first()
                    .map(|&off| build_snippet(body, off, req.context_lines));
                let lines: std::collections::BTreeSet<usize> =
                    hits.iter().map(|&off| line_of(body, off)).collect();
                match_count = Some(lines.len());
            }
        }
        (filename_hit || content_hit, snippet, match_count)
    }
}

/// Whether `needle` occurs in `haystack` once CJK-internal gaps are folded out
/// of both. Callers check the raw `contains` first; this only adds the hits that
/// spacing or inline style hid.
fn folded_contains(haystack: &str, needle: &str) -> bool {
    let folded_hay = cjk_fold::fold(haystack);
    let folded_needle = cjk_fold::fold(needle);
    if folded_hay.is_none() && folded_needle.is_none() {
        return false; // nothing to fold; the raw `contains` already decided
    }
    let hay = folded_hay.as_ref().map_or(haystack, cjk_fold::Folded::text);
    let needle = folded_needle
        .as_ref()
        .map_or(needle, cjk_fold::Folded::text);
    hay.contains(needle)
}

/// The 0-based line the byte offset `off` falls on.
fn line_of(body: &str, off: usize) -> usize {
    body[..off].bytes().filter(|&b| b == b'\n').count()
}

/// Extract `context_lines` of context around the byte offset `off`.
fn build_snippet(body: &str, off: usize, context_lines: usize) -> String {
    let line_no = line_of(body, off);
    let lines: Vec<&str> = body.lines().collect();
    let start = line_no.saturating_sub(context_lines);
    let end = (line_no + context_lines + 1).min(lines.len());
    lines[start..end].join("\n")
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
    async fn nfc_query_finds_nfd_content_and_filename() {
        // "\u{d55c}\u{ae00}" (NFC) vs NFD jamo sequence on disk.
        let nfd_word = "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}"; // 한글 in NFD
        let body = format!("# T\n{nfd_word} content here\n");
        let nfd_name = format!("{nfd_word}.md");
        let (_d, s) = server(&[(nfd_name.as_str(), body.as_str())]);
        let r = s
            .search_notes(Parameters(SearchNotesRequest {
                query: Some("\u{d55c}\u{ae00}".into()),
                mode: SearchMode::default(),
                frontmatter: None,
                frontmatter_exists: None,
                limit: 20,
                context_lines: 0,
                cursor: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(r.items.len(), 1, "NFC query must match NFD content");
        assert!(r.items[0].snippet.is_some());
        let r = s
            .search_notes(Parameters(SearchNotesRequest {
                query: Some("\u{d55c}\u{ae00}".into()),
                mode: SearchMode::Filename,
                frontmatter: None,
                frontmatter_exists: None,
                limit: 20,
                context_lines: 0,
                cursor: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(r.items.len(), 1, "NFC query must match NFD filename");
    }

    #[tokio::test]
    async fn match_count_reveals_hits_beyond_the_snippet() {
        let (_d, s) = server(&[(
            "m.md",
            "# One\nfindme a.\nfiller\nfindme b.\n\n# Two\nfindme c.\n",
        )]);
        let r = s
            .search_notes(Parameters(SearchNotesRequest {
                query: Some("findme".into()),
                mode: SearchMode::default(),
                frontmatter: None,
                frontmatter_exists: None,
                limit: 20,
                context_lines: 0,
                cursor: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(r.items[0].match_count, Some(3));
        assert_eq!(r.items[0].snippet.as_deref(), Some("findme a."));
    }

    #[tokio::test]
    async fn out_of_range_limit_is_rejected() {
        // The schema's min/max are not framework-enforced; the server rejects
        // instead of silently clamping.
        let (_d, s) = server(&[("a.md", "x")]);
        for bad in [0usize, 1001] {
            let r = s
                .list_notes(Parameters(ListNotesRequest {
                    directory: String::new(),
                    recursive: true,
                    glob: None,
                    include_dirs: false,
                    limit: bad,
                    cursor: None,
                }))
                .await;
            assert!(r.is_err(), "list limit {bad} must be rejected");
        }
        let r = s
            .search_notes(Parameters(SearchNotesRequest {
                query: Some("x".into()),
                mode: SearchMode::default(),
                frontmatter: None,
                frontmatter_exists: None,
                limit: 500,
                context_lines: 2,
                cursor: None,
            }))
            .await;
        assert!(r.is_err(), "search limit 500 must be rejected");
    }

    #[tokio::test]
    async fn over_long_query_is_rejected() {
        // An over-cap query must fail fast, not wedge the aho-corasick build
        // (whose case-insensitive construction is superlinear in one long token).
        let (_d, s) = server(&[("a.md", "body")]);
        let huge = "z".repeat(MAX_QUERY_BYTES + 1);
        let r = s
            .search_notes(Parameters(SearchNotesRequest {
                query: Some(huge),
                mode: SearchMode::default(),
                frontmatter: None,
                frontmatter_exists: None,
                limit: 20,
                context_lines: 2,
                cursor: None,
            }))
            .await;
        assert!(r.is_err(), "over-cap query must be rejected");
    }

    #[tokio::test]
    async fn list_notes_pages_sorted() {
        let (_d, s) = server(&[("a.md", "x"), ("b.md", "y"), ("c.md", "z")]);
        let page1 = s
            .list_notes(Parameters(ListNotesRequest {
                directory: String::new(),
                recursive: true,
                glob: None,
                include_dirs: false,
                limit: 2,
                cursor: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(
            page1
                .items
                .iter()
                .map(|i| i.path.as_str())
                .collect::<Vec<_>>(),
            ["a.md", "b.md"]
        );
        assert_eq!(page1.next_cursor.as_deref(), Some("b.md"));

        let page2 = s
            .list_notes(Parameters(ListNotesRequest {
                directory: String::new(),
                recursive: true,
                glob: None,
                include_dirs: false,
                limit: 2,
                cursor: page1.next_cursor,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(
            page2
                .items
                .iter()
                .map(|i| i.path.as_str())
                .collect::<Vec<_>>(),
            ["c.md"]
        );
        assert!(page2.next_cursor.is_none());
    }

    fn search_req(query: Option<&str>) -> SearchNotesRequest {
        SearchNotesRequest {
            query: query.map(str::to_string),
            mode: SearchMode::Both,
            frontmatter: None,
            frontmatter_exists: None,
            limit: 20,
            context_lines: 1,
            cursor: None,
        }
    }

    #[tokio::test]
    async fn search_content_requires_all_keywords() {
        let (_d, s) = server(&[("a.md", "alpha beta gamma\n"), ("b.md", "alpha only\n")]);
        let r = s
            .search_notes(Parameters(search_req(Some("alpha gamma"))))
            .await
            .unwrap()
            .0;
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].path, "a.md");
        assert!(
            r.items[0]
                .snippet
                .as_deref()
                .unwrap()
                .contains("alpha beta gamma")
        );
    }

    #[tokio::test]
    async fn search_filename_substring() {
        let (_d, s) = server(&[("meeting-notes.md", "x"), ("other.md", "y")]);
        let mut req = search_req(Some("meeting"));
        req.mode = SearchMode::Filename;
        let r = s.search_notes(Parameters(req)).await.unwrap().0;
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].path, "meeting-notes.md");
    }

    #[tokio::test]
    async fn search_frontmatter_filter_and_echo() {
        let (_d, s) = server(&[
            (
                "p1.md",
                "---\nstatus: draft\ntags:\n  - project\n---\nbody\n",
            ),
            ("p2.md", "---\nstatus: done\n---\nbody\n"),
        ]);
        let mut req = search_req(None);
        let mut filter = Map::new();
        filter.insert("status".into(), json!("draft"));
        req.frontmatter = Some(filter);
        let r = s.search_notes(Parameters(req)).await.unwrap().0;
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].path, "p1.md");
        assert_eq!(
            r.items[0].frontmatter.as_ref().unwrap()["status"],
            json!("draft")
        );
    }

    #[tokio::test]
    async fn search_frontmatter_list_contains() {
        let (_d, s) = server(&[("p.md", "---\ntags:\n  - a\n  - b\n---\nx\n")]);
        let mut req = search_req(None);
        let mut filter = Map::new();
        filter.insert("tags".into(), json!("b"));
        req.frontmatter = Some(filter);
        let r = s.search_notes(Parameters(req)).await.unwrap().0;
        assert_eq!(r.items.len(), 1);
    }

    #[tokio::test]
    async fn search_requires_a_criterion() {
        let (_d, s) = server(&[("a.md", "x")]);
        let result = s.search_notes(Parameters(search_req(None))).await;
        assert!(
            result.is_err(),
            "missing criterion must be a protocol error"
        );
    }

    async fn paths_for(s: &MdServer, query: &str) -> Vec<String> {
        s.search_notes(Parameters(search_req(Some(query))))
            .await
            .unwrap()
            .0
            .items
            .into_iter()
            .map(|i| i.path)
            .collect()
    }

    #[tokio::test]
    async fn unspaced_cjk_query_matches_spacing_and_inline_style() {
        // Korean is written with optional spacing, so "전역지침" and "전역 지침"
        // are the same term; inline style markers split it in the source only.
        let (_d, s) = server(&[
            ("bold.md", "# T\n**전역** 지침을 따른다.\n"),
            ("code.md", "# T\n`전역` 지침을 따른다.\n"),
            ("head.md", "## 전역 지침\n본문.\n"),
            ("plain.md", "# T\n전역 지침을 따른다.\n"),
            ("wrap.md", "# T\n전역\n지침을 따른다.\n"),
        ]);
        assert_eq!(
            paths_for(&s, "전역지침").await,
            ["bold.md", "code.md", "head.md", "plain.md", "wrap.md"]
        );
    }

    #[tokio::test]
    async fn cjk_bridging_stops_at_block_structure() {
        // Block markers sit at line starts and cell boundaries, so they must keep
        // separating terms; only inline style is bridged.
        let (_d, s) = server(&[
            ("list.md", "- 전역\n- 지침\n"),
            ("para.md", "전역\n\n지침\n"),
            ("quote.md", "전역\n> 지침\n"),
            ("rule.md", "전역\n\n***\n\n지침\n"),
            ("section.md", "전역\n## 지침\n"),
            ("sentence.md", "전역. 지침\n"),
            ("table.md", "| 전역 | 지침 |\n"),
            ("underscore.md", "__전역__ 지침\n"),
        ]);
        assert_eq!(paths_for(&s, "전역지침").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn cjk_bridging_leaves_ascii_spacing_alone() {
        let (_d, s) = server(&[("a.md", "hello world\n"), ("b.md", "snake_case name\n")]);
        assert_eq!(paths_for(&s, "helloworld").await, Vec::<String>::new());
        assert_eq!(paths_for(&s, "snakecase").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn bridged_hit_reports_original_text() {
        // The shadow text exists only to match; snippets and counts must address
        // the original bytes, markers and all.
        let (_d, s) = server(&[("a.md", "# T\nfiller\n**전역** 지침을 본다.\ntail\n")]);
        let mut req = search_req(Some("전역지침"));
        req.context_lines = 0;
        let r = s.search_notes(Parameters(req)).await.unwrap().0;
        assert_eq!(r.items[0].snippet.as_deref(), Some("**전역** 지침을 본다."));
        assert_eq!(r.items[0].match_count, Some(1));
    }

    #[tokio::test]
    async fn folding_never_loses_a_raw_match() {
        // Folding drops markers a query may legitimately contain, so the raw and
        // folded scans are unioned rather than swapped.
        let (_d, s) = server(&[("a.md", "# T\n`전역` 지침\n")]);
        assert_eq!(paths_for(&s, "`전역`").await, ["a.md"]);
    }

    #[tokio::test]
    async fn filename_mode_bridges_cjk_spacing_both_ways() {
        let (_d, s) = server(&[("로컬지침.md", "y"), ("전역 지침.md", "x")]);
        for (query, want) in [("전역지침", "전역 지침.md"), ("로컬 지침", "로컬지침.md")]
        {
            let mut req = search_req(Some(query));
            req.mode = SearchMode::Filename;
            let r = s.search_notes(Parameters(req)).await.unwrap().0;
            let got: Vec<&str> = r.items.iter().map(|i| i.path.as_str()).collect();
            assert_eq!(got, [want], "filename query {query:?}");
        }
    }
}
