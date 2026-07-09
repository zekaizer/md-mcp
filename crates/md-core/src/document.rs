//! The document parser and section model.
//!
//! [`Document::parse`] scans LF-normalized note source into a frontmatter span
//! and a flat list of ATX [`Heading`]s with byte spans, code-fence aware. From
//! that, section/body/preamble spans and heading-path addressing are derived
//! ([ADR-0003](../../../docs/adr/0003-document-parser-and-section-model.md)).

use std::collections::HashMap;

use crate::error::{Code, Error, Result};
use crate::text::{Span, nfc};

/// One ATX heading in a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    /// Heading level, 1–6 (the number of leading `#`).
    pub level: u8,
    /// The heading text, verbatim (compared after NFC, never re-encoded).
    pub text: String,
    /// 1-based line number of the heading.
    pub line: usize,
    /// Byte span of the heading line, including its trailing newline.
    pub span: Span,
    /// Index of the nearest ancestor heading (lower level), if any.
    parent: Option<usize>,
}

/// A parsed note: its frontmatter span (if any) and its headings.
#[derive(Clone, Debug)]
pub struct Document {
    /// Byte span of the leading `---`…`---` frontmatter block, if present.
    pub frontmatter: Option<Span>,
    /// Headings in document order.
    pub headings: Vec<Heading>,
    len: usize,
}

/// One row of a note's outline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineEntry {
    pub heading_path: Vec<String>,
    pub level: u8,
    pub line: usize,
    /// 1-based position among headings sharing the identical full path.
    pub occurrence: usize,
    /// Whether more than one heading shares this full path.
    pub ambiguous: bool,
}

impl Document {
    /// Parse LF-normalized note source.
    #[must_use]
    pub fn parse(source: &str) -> Document {
        let len = source.len();
        let lines = line_spans(source);

        let (frontmatter, body_start_line) = detect_frontmatter(source, &lines);

        let mut headings: Vec<Heading> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut fence: Option<Fence> = None;

        for (idx, &(s, e)) in lines.iter().enumerate().skip(body_start_line) {
            let line = source[s..e].trim_end_matches(['\r', '\n']);
            let indent = line.len() - line.trim_start_matches(' ').len();
            let trimmed = &line[indent..];

            if let Some(f) = fence {
                if indent <= 3 && f.closes(trimmed) {
                    fence = None;
                }
                continue;
            }
            if indent <= 3 {
                if let Some(f) = Fence::opens(trimmed) {
                    fence = Some(f);
                    continue;
                }
                if let Some((level, text)) = parse_atx(trimmed) {
                    let line_no = idx + 1;
                    let i = headings.len();
                    while let Some(&top) = stack.last() {
                        if headings[top].level >= level {
                            stack.pop();
                        } else {
                            break;
                        }
                    }
                    let parent = stack.last().copied();
                    headings.push(Heading {
                        level,
                        text,
                        line: line_no,
                        span: Span::new(s, e),
                        parent,
                    });
                    stack.push(i);
                }
            }
        }

        Document {
            frontmatter,
            headings,
            len,
        }
    }

    /// The byte length of the parsed source.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.len
    }

    /// The subtree span of heading `i`: its line to the next same-or-higher
    /// heading (or EOF) — the `section` scope.
    #[must_use]
    pub fn section_span(&self, i: usize) -> Span {
        let h = &self.headings[i];
        let end = self.headings[i + 1..]
            .iter()
            .find(|n| n.level <= h.level)
            .map_or(self.len, |n| n.span.start);
        Span::new(h.span.start, end)
    }

    /// The lead-body span of heading `i`: its line end to the next heading of
    /// any level (or EOF) — the `body` scope, subsections excluded.
    #[must_use]
    pub fn own_body_span(&self, i: usize) -> Span {
        let start = self.headings[i].span.end;
        let end = self.headings.get(i + 1).map_or(self.len, |n| n.span.start);
        Span::new(start, end.max(start))
    }

    /// The preamble span: frontmatter end to the first heading (or EOF) — the
    /// root `body` scope.
    #[must_use]
    pub fn preamble_span(&self) -> Span {
        let start = self.frontmatter.map_or(0, |f| f.end);
        let end = self.headings.first().map_or(self.len, |h| h.span.start);
        Span::new(start, end.max(start))
    }

    /// The whole-body span: frontmatter end to EOF — the root `section` scope.
    #[must_use]
    pub fn whole_body_span(&self) -> Span {
        let start = self.frontmatter.map_or(0, |f| f.end);
        Span::new(start, self.len)
    }

    /// The full ancestor chain of heading `i`, outermost first.
    #[must_use]
    pub fn heading_path(&self, i: usize) -> Vec<String> {
        self.chain(i).into_iter().map(str::to_string).collect()
    }

    /// Whether heading `node` is `ancestor` itself or nested within its subtree.
    #[must_use]
    pub fn is_within(&self, node: usize, ancestor: usize) -> bool {
        let mut cur = Some(node);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.headings[c].parent;
        }
        false
    }

    fn chain(&self, i: usize) -> Vec<&str> {
        let mut v = Vec::new();
        let mut cur = Some(i);
        while let Some(c) = cur {
            v.push(self.headings[c].text.as_str());
            cur = self.headings[c].parent;
        }
        v.reverse();
        v
    }

    /// Resolve a heading path (NFC suffix match) and optional 1-based
    /// `occurrence` to a heading index.
    pub fn resolve_heading(&self, path: &[String], occurrence: Option<usize>) -> Result<usize> {
        if path.is_empty() {
            return Err(Error::new(
                Code::NotFound,
                "empty heading path is the root, not a heading",
            ));
        }
        let path_nfc: Vec<String> = path.iter().map(|s| nfc(s).into_owned()).collect();
        // Exact full-chain matches first: this is the group read_outlines
        // computes occurrence/ambiguous over, so the numbers line up. Only when
        // nothing matches exactly does the path fall back to suffix matching —
        // a convenience for deep notes — and then occurrence is refused, since
        // a suffix-match index has no counterpart in the outline.
        let exact: Vec<usize> = (0..self.headings.len())
            .filter(|&i| {
                let chain = self.chain(i);
                chain.len() == path_nfc.len() && is_suffix(&chain, &path_nfc)
            })
            .collect();
        let exact_mode = !exact.is_empty();
        let matches: Vec<usize> = if exact_mode {
            exact
        } else {
            (0..self.headings.len())
                .filter(|&i| is_suffix(&self.chain(i), &path_nfc))
                .collect()
        };

        if matches.is_empty() {
            return Err(Error::new(
                Code::NotFound,
                format!("heading path not found: {path:?}"),
            ));
        }

        match occurrence {
            Some(occ) => {
                if occ == 0 {
                    return Err(Error::new(Code::NotFound, "occurrence is 1-based"));
                }
                if !exact_mode {
                    return Err(Error::new(
                        Code::Ambiguous,
                        format!(
                            "occurrence requires the full heading_path; {path:?} matches only \
                             by suffix — use the exact chain from read_outlines"
                        ),
                    ));
                }
                matches.get(occ - 1).copied().ok_or_else(|| {
                    Error::new(
                        Code::NotFound,
                        format!(
                            "occurrence {occ} of {} match(es) for {path:?}",
                            matches.len()
                        ),
                    )
                })
            }
            None => match matches.as_slice() {
                [only] => Ok(*only),
                many if exact_mode => Err(Error::new(
                    Code::Ambiguous,
                    format!(
                        "heading path matches {} headings; specify occurrence: {path:?}",
                        many.len()
                    ),
                )),
                many => Err(Error::new(
                    Code::Ambiguous,
                    format!(
                        "{path:?} suffix-matches {} headings; give the full heading_path \
                         from read_outlines (plus occurrence if it is marked ambiguous)",
                        many.len()
                    ),
                )),
            },
        }
    }

    /// The note's outline: every heading with its path, level, line, and
    /// occurrence/ambiguity among identical full paths.
    #[must_use]
    pub fn outline(&self) -> Vec<OutlineEntry> {
        let n = self.headings.len();
        let chains: Vec<Vec<String>> = (0..n).map(|i| self.heading_path(i)).collect();
        let keys: Vec<Vec<String>> = chains
            .iter()
            .map(|c| c.iter().map(|s| nfc(s).into_owned()).collect())
            .collect();

        let mut groups: HashMap<&Vec<String>, Vec<usize>> = HashMap::new();
        for (i, key) in keys.iter().enumerate() {
            groups.entry(key).or_default().push(i);
        }

        (0..n)
            .map(|i| {
                let group = &groups[&keys[i]];
                let occurrence = group.iter().position(|&x| x == i).unwrap_or(0) + 1;
                OutlineEntry {
                    heading_path: chains[i].clone(),
                    level: self.headings[i].level,
                    line: self.headings[i].line,
                    occurrence,
                    ambiguous: group.len() > 1,
                }
            })
            .collect()
    }
}

/// A `Vec` of `(start, end)` byte spans, one per line, including the trailing `\n`.
pub(crate) fn line_spans(source: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            spans.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < source.len() {
        spans.push((start, source.len()));
    }
    spans
}

/// Returns `(frontmatter_span, first_body_line_index)`.
fn detect_frontmatter(source: &str, lines: &[(usize, usize)]) -> (Option<Span>, usize) {
    let Some(&(s0, e0)) = lines.first() else {
        return (None, 0);
    };
    if source[s0..e0].trim_end_matches(['\r', '\n']) != "---" {
        return (None, 0);
    }
    for (idx, &(s, e)) in lines.iter().enumerate().skip(1) {
        if source[s..e].trim_end_matches(['\r', '\n']) == "---" {
            return (Some(Span::new(0, e)), idx + 1);
        }
    }
    // Unterminated leading `---` is body, not frontmatter.
    (None, 0)
}

#[derive(Clone, Copy)]
pub(crate) struct Fence {
    ch: u8,
    len: usize,
}

impl Fence {
    /// If `trimmed` (indent already stripped) opens a fenced code block, return it.
    pub(crate) fn opens(trimmed: &str) -> Option<Fence> {
        let bytes = trimmed.as_bytes();
        let ch = *bytes.first()?;
        if ch != b'`' && ch != b'~' {
            return None;
        }
        let run = bytes.iter().take_while(|&&b| b == ch).count();
        if run < 3 {
            return None;
        }
        // A backtick info string may not contain a backtick.
        if ch == b'`' && trimmed[run..].contains('`') {
            return None;
        }
        Some(Fence { ch, len: run })
    }

    /// Whether `trimmed` closes this fence: a run of ≥`len` of the same char,
    /// then only whitespace.
    pub(crate) fn closes(self, trimmed: &str) -> bool {
        let run = trimmed.bytes().take_while(|&b| b == self.ch).count();
        run >= self.len && trimmed[run..].trim().is_empty()
    }
}

/// Parse an ATX heading from `trimmed` (leading indent already stripped),
/// returning `(level, text)`.
fn parse_atx(trimmed: &str) -> Option<(u8, String)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None; // `#tag` is not a heading
    }
    let mut text = rest.trim();
    let trailing = text.bytes().rev().take_while(|&b| b == b'#').count();
    if trailing > 0 {
        let before = &text[..text.len() - trailing];
        if before.is_empty() || before.ends_with([' ', '\t']) {
            text = before.trim_end();
        }
    }
    Some((u8::try_from(hashes).unwrap_or(6), text.to_string()))
}

fn is_suffix(chain: &[&str], path_nfc: &[String]) -> bool {
    if path_nfc.len() > chain.len() {
        return false;
    }
    let off = chain.len() - path_nfc.len();
    chain[off..]
        .iter()
        .zip(path_nfc)
        .all(|(c, p)| nfc(c).as_ref() == p.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(doc: &Document) -> Vec<(u8, &str)> {
        doc.headings
            .iter()
            .map(|h| (h.level, h.text.as_str()))
            .collect()
    }

    #[test]
    fn parses_atx_levels_and_text() {
        let doc = Document::parse("# A\n## B\n### C\n");
        assert_eq!(texts(&doc), [(1, "A"), (2, "B"), (3, "C")]);
        assert_eq!(doc.headings[0].line, 1);
        assert_eq!(doc.headings[1].line, 2);
    }

    #[test]
    fn hash_without_space_is_not_a_heading() {
        let doc = Document::parse("#tag is a paragraph\n");
        assert!(doc.headings.is_empty());
    }

    #[test]
    fn empty_heading_and_closing_sequence() {
        // "## foo ##" -> closing sequence stripped; "### bar#" -> no space before
        // the trailing #, so it is kept; "# ###" -> all-hashes closing -> empty.
        let doc = Document::parse("#\n## foo ##\n### bar#\n# ###\n");
        assert_eq!(texts(&doc), [(1, ""), (2, "foo"), (3, "bar#"), (1, "")]);
    }

    #[test]
    fn seven_hashes_is_not_a_heading() {
        let doc = Document::parse("####### too deep\n");
        assert!(doc.headings.is_empty());
    }

    #[test]
    fn fenced_code_hash_is_not_a_heading() {
        let src = "# Real\n```\n# not a heading\n```\n# Also real\n";
        assert_eq!(
            texts(&Document::parse(src)),
            [(1, "Real"), (1, "Also real")]
        );
    }

    #[test]
    fn tilde_fence_and_backtick_info_string() {
        let src = "~~~rust\n# nope\n~~~\n```python\n## nope\n```\n# yes\n";
        assert_eq!(texts(&Document::parse(src)), [(1, "yes")]);
    }

    #[test]
    fn indented_four_spaces_is_not_a_heading() {
        let doc = Document::parse("    # indented code\n# top\n");
        assert_eq!(texts(&doc), [(1, "top")]);
    }

    #[test]
    fn heading_span_is_byte_accurate_with_multibyte() {
        let src = "## 한글 제목\nbody\n";
        let doc = Document::parse(src);
        assert_eq!(doc.headings[0].span.of(src), "## 한글 제목\n");
    }

    // --- frontmatter --------------------------------------------------------

    #[test]
    fn frontmatter_block_is_detected() {
        let src = "---\ntitle: x\n---\n# H\n";
        let doc = Document::parse(src);
        let fm = doc.frontmatter.unwrap();
        assert_eq!(fm.of(src), "---\ntitle: x\n---\n");
        assert_eq!(texts(&doc), [(1, "H")]);
    }

    #[test]
    fn unterminated_frontmatter_is_body() {
        let src = "---\n# H\nstill body\n";
        let doc = Document::parse(src);
        assert!(doc.frontmatter.is_none());
        assert_eq!(texts(&doc), [(1, "H")]);
    }

    // --- section spans ------------------------------------------------------

    #[test]
    fn section_vs_body_spans() {
        let src = "# A\nlead\n## B\nsub\n# C\n";
        let doc = Document::parse(src);
        // section of A includes its subsection B, stops at sibling C.
        assert_eq!(doc.section_span(0).of(src), "# A\nlead\n## B\nsub\n");
        // body of A is only its lead, before the first subheading.
        assert_eq!(doc.own_body_span(0).of(src), "lead\n");
    }

    #[test]
    fn preamble_and_whole_body() {
        let src = "intro\nmore\n# A\nx\n";
        let doc = Document::parse(src);
        assert_eq!(doc.preamble_span().of(src), "intro\nmore\n");
        assert_eq!(doc.whole_body_span().of(src), src);
    }

    #[test]
    fn preamble_after_frontmatter() {
        let src = "---\nk: v\n---\nintro\n# A\n";
        let doc = Document::parse(src);
        assert_eq!(doc.preamble_span().of(src), "intro\n");
    }

    #[test]
    fn last_heading_body_to_eof_without_trailing_newline() {
        let src = "# A\nbody";
        let doc = Document::parse(src);
        assert_eq!(doc.section_span(0).of(src), "# A\nbody");
        assert_eq!(doc.own_body_span(0).of(src), "body");
    }

    // --- addressing ---------------------------------------------------------

    #[test]
    fn resolve_by_suffix_and_parent_disambiguation() {
        let src = "# Q1\n## Status\n# Q2\n## Status\n";
        let doc = Document::parse(src);
        // Same leaf, different parents -> distinct, no occurrence needed.
        let a = doc
            .resolve_heading(&["Q1".into(), "Status".into()], None)
            .unwrap();
        let b = doc
            .resolve_heading(&["Q2".into(), "Status".into()], None)
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(doc.headings[a].line, 2);
        assert_eq!(doc.headings[b].line, 4);
    }

    #[test]
    fn bare_leaf_with_two_parents_is_ambiguous() {
        let src = "# Q1\n## Status\n# Q2\n## Status\n";
        let doc = Document::parse(src);
        let err = doc.resolve_heading(&["Status".into()], None).unwrap_err();
        assert_eq!(err.code, Code::Ambiguous);
        // occurrence indexes exact-match groups (the outline's numbering), so
        // it cannot be combined with a suffix-only match.
        let err = doc
            .resolve_heading(&["Status".into()], Some(1))
            .unwrap_err();
        assert_eq!(err.code, Code::Ambiguous);
        assert!(err.message.contains("full heading_path"), "{}", err.message);
    }

    #[test]
    fn exact_match_beats_suffix_match() {
        // A nested `## X` comes first in document order; the top-level `# X`
        // is the exact match for ["X"] and must win — resolving by suffix here
        // silently edited the wrong section.
        let src = "# Y\ny\n## X\nnested\n# X\ntop\n";
        let doc = Document::parse(src);
        let i = doc.resolve_heading(&["X".into()], None).unwrap();
        assert_eq!(doc.headings[i].line, 5, "must be the top-level X");
        // occurrence numbering agrees with the outline (exact-match group).
        let i = doc.resolve_heading(&["X".into()], Some(1)).unwrap();
        assert_eq!(doc.headings[i].line, 5);
    }

    #[test]
    fn unique_suffix_still_resolves_without_occurrence() {
        let src = "# A\n## B\n### C\nc\n";
        let doc = Document::parse(src);
        let i = doc
            .resolve_heading(&["B".into(), "C".into()], None)
            .unwrap();
        assert_eq!(doc.headings[i].line, 3);
    }

    #[test]
    fn resolve_not_found_and_out_of_range() {
        let doc = Document::parse("# A\n");
        assert_eq!(
            doc.resolve_heading(&["X".into()], None).unwrap_err().code,
            Code::NotFound
        );
        assert_eq!(
            doc.resolve_heading(&["A".into()], Some(2))
                .unwrap_err()
                .code,
            Code::NotFound
        );
    }

    #[test]
    fn resolve_is_nfc_insensitive() {
        // Source authored in NFD ("가" = U+1100 U+1161), path queried in NFC.
        let src = "# \u{1100}\u{1161}\nbody\n";
        let doc = Document::parse(src);
        assert!(doc.resolve_heading(&["가".into()], None).is_ok());
    }

    #[test]
    fn outline_reports_paths_and_ambiguity() {
        let src = "# Q1\n## Status\n# Q2\n## Status\n";
        let doc = Document::parse(src);
        let out = doc.outline();
        assert_eq!(out[0].heading_path, vec!["Q1"]);
        assert_eq!(out[1].heading_path, vec!["Q1", "Status"]);
        // The two "Status" headings have different full paths -> not ambiguous.
        assert!(!out[1].ambiguous);
        assert_eq!(out[1].occurrence, 1);
    }

    #[test]
    fn outline_marks_identical_full_paths_ambiguous() {
        let src = "# A\n# A\n";
        let doc = Document::parse(src);
        let out = doc.outline();
        assert!(out[0].ambiguous && out[1].ambiguous);
        assert_eq!(out[0].occurrence, 1);
        assert_eq!(out[1].occurrence, 2);
    }
}
