//! Section-level edits: the pure text transformation behind `edit_sections`.
//!
//! [`patch_sections`] resolves every edit against the **original** snapshot,
//! rejects overlapping targets, validates inserted heading levels, and applies
//! the surviving edits as byte splices. It is all-or-nothing: any rejected edit
//! fails the whole batch with one error per offending item. `move` is a
//! two-location operation (cut at the source, insert at the destination).

use crate::document::Document;
use crate::error::{Code, Error};
use crate::section::Scope;
use crate::text::normalize_newlines;

/// A section edit operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Replace,
    Append,
    Delete,
    InsertBefore,
    InsertAfter,
    Rename,
    Move,
}

/// Where a `move` places its section, relative to an anchor section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Before,
    After,
}

/// The destination of a `move` operation.
#[derive(Clone, Debug)]
pub struct Destination {
    pub heading_path: Vec<String>,
    pub occurrence: Option<usize>,
    pub position: Position,
}

/// One edit in an `edit_sections` batch.
#[derive(Clone, Debug, Default)]
pub struct Edit {
    pub heading_path: Vec<String>,
    pub occurrence: Option<usize>,
    pub operation: Option<Operation>,
    pub scope: Scope,
    pub content: Option<String>,
    pub new_heading: Option<String>,
    pub destination: Option<Destination>,
    pub expected_hash: Option<String>,
}

/// A per-item batch error, keyed by the edit's index.
#[derive(Clone, Debug)]
pub struct BatchError {
    pub index: usize,
    pub error: Error,
}

struct Splice {
    start: usize,
    end: usize,
    replacement: String,
    /// The byte region (zero-width for insertions) used for overlap detection.
    footprint: (usize, usize),
}

/// Apply a batch of section edits to LF-normalized source. All-or-nothing: on any
/// rejection nothing is changed and every offending item is reported.
pub fn patch_sections(source: &str, edits: &[Edit]) -> Result<String, Vec<BatchError>> {
    let normalized = normalize_newlines(source);
    let source = normalized.as_ref();
    let doc = Document::parse(source);

    let mut errors: Vec<BatchError> = Vec::new();
    let mut splices: Vec<(usize, Splice)> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        match resolve_edit(&doc, source, edit) {
            Ok(sps) => splices.extend(sps.into_iter().map(|s| (i, s))),
            Err(error) => errors.push(BatchError { index: i, error }),
        }
    }

    detect_overlaps(&splices, &mut errors);

    if !errors.is_empty() {
        errors.sort_by_key(|e| e.index);
        return Err(errors);
    }

    let mut result = source.to_string();
    let mut sp: Vec<Splice> = splices.into_iter().map(|(_, s)| s).collect();
    sp.sort_by_key(|s| (s.start, s.end));
    for s in sp.into_iter().rev() {
        result.replace_range(s.start..s.end, &s.replacement);
    }
    Ok(result)
}

fn resolve_edit(doc: &Document, source: &str, edit: &Edit) -> Result<Vec<Splice>, Error> {
    let op = edit
        .operation
        .ok_or_else(|| Error::new(Code::MissingContent, "operation is required"))?;
    let index = if edit.heading_path.is_empty() {
        None
    } else {
        Some(doc.resolve_heading(&edit.heading_path, edit.occurrence)?)
    };

    check_expected_hash(doc, source, edit, index, op)?;

    let section = section_bounds(doc, index);
    let content_span = doc.content_span(index, edit.scope);

    match op {
        Operation::Replace => {
            let content = require_content(edit)?;
            validate_inserted_levels(content, inside_level(doc, index))?;
            let bytes = source.as_bytes();
            let mut rep = content.to_string();
            if !rep.is_empty() {
                let (lead, trail) = span_seams(source, content_span.start, content_span.end);
                // The blank lines the span began with are the seam under the
                // heading, not body text — put them back unless the content
                // already carries its own (e.g. echoed straight from a read).
                if !lead.is_empty() && !starts_with_blank_line(&rep) {
                    rep.insert_str(0, lead);
                }
                // Separate the body from the heading line above it (e.g. replacing
                // the empty body of a heading with no trailing newline).
                if content_span.start > 0 && bytes[content_span.start - 1] != b'\n' {
                    rep.insert(0, '\n');
                }
                // Keep the replacement newline-terminated when content follows the
                // splice (a heading/text) or when the replaced span itself ended
                // with a newline — so it neither merges into the following heading
                // nor drops the note's trailing newline.
                let following_is_content =
                    content_span.end < source.len() && bytes[content_span.end] != b'\n';
                let span_ended_nl =
                    content_span.end > content_span.start && bytes[content_span.end - 1] == b'\n';
                if !rep.ends_with('\n') && (following_is_content || span_ended_nl) {
                    rep.push('\n');
                }
                // Same for the seam ahead of whatever followed the span.
                if !trail.is_empty() && !ends_with_blank_line(&rep) {
                    rep.push_str(trail);
                }
            }
            Ok(vec![Splice {
                start: content_span.start,
                end: content_span.end,
                replacement: rep,
                footprint: (content_span.start, content_span.end),
            }])
        }
        Operation::Append => {
            let content = require_content(edit)?;
            validate_inserted_levels(content, inside_level(doc, index))?;
            let pos = append_pos(source, content_span.start, content_span.end);
            let mut rep = String::new();
            if pos > 0 && source.as_bytes()[pos - 1] != b'\n' {
                rep.push('\n');
            }
            rep.push_str(content);
            if !rep.ends_with('\n') {
                rep.push('\n');
            }
            Ok(vec![Splice {
                start: pos,
                end: pos,
                replacement: rep,
                footprint: (content_span.start, content_span.end),
            }])
        }
        Operation::Delete => {
            let (start, end) = match edit.scope {
                Scope::Body => (content_span.start, content_span.end),
                Scope::Section => section,
            };
            Ok(vec![Splice {
                start,
                end,
                replacement: String::new(),
                footprint: (start, end),
            }])
        }
        Operation::InsertBefore | Operation::InsertAfter => {
            let content = require_content(edit)?;
            validate_inserted_levels(content, sibling_level(doc, index))?;
            let pos = if op == Operation::InsertBefore {
                section.0
            } else {
                section.1
            };
            let mut rep = content.to_string();
            pad_block_seams(&mut rep, source, pos);
            Ok(vec![Splice {
                start: pos,
                end: pos,
                replacement: rep,
                footprint: (pos, pos),
            }])
        }
        Operation::Rename => {
            let Some(i) = index else {
                return Err(Error::new(
                    Code::NotFound,
                    "cannot rename the root (it has no heading)",
                ));
            };
            let new_heading = edit.new_heading.as_deref().ok_or_else(|| {
                Error::new(Code::MissingContent, "new_heading is required for rename")
            })?;
            if new_heading.trim().is_empty() {
                return Err(Error::new(
                    Code::InvalidHeading,
                    "new_heading must not be empty",
                ));
            }
            if new_heading.contains(['\n', '\r']) {
                return Err(Error::new(
                    Code::InvalidHeading,
                    "new_heading must not contain a line break",
                ));
            }
            let h = &doc.headings[i];
            let hashes = "#".repeat(h.level as usize);
            let had_nl = source.as_bytes().get(h.span.end - 1) == Some(&b'\n');
            let line = format!("{hashes} {new_heading}");
            let rep = if had_nl { format!("{line}\n") } else { line };
            Ok(vec![Splice {
                start: h.span.start,
                end: h.span.end,
                replacement: rep,
                footprint: (h.span.start, h.span.end),
            }])
        }
        Operation::Move => resolve_move(doc, source, edit, index),
    }
}

/// Resolve a `move`: cut the source subtree and re-insert it at the destination,
/// as two splices (delete at source, insert at destination).
fn resolve_move(
    doc: &Document,
    source: &str,
    edit: &Edit,
    index: Option<usize>,
) -> Result<Vec<Splice>, Error> {
    let Some(src_i) = index else {
        return Err(Error::new(Code::NotFound, "cannot move the root"));
    };
    let dest = edit
        .destination
        .as_ref()
        .ok_or_else(|| Error::new(Code::MissingContent, "destination is required for move"))?;
    let dest_idx = if dest.heading_path.is_empty() {
        None
    } else {
        Some(doc.resolve_heading(&dest.heading_path, dest.occurrence)?)
    };

    // Reject moving a section into itself or its own subtree, by heading
    // ancestry — a sibling move whose destination touches the source span
    // boundary is legitimate and must not be a false self-move.
    if let Some(d) = dest_idx
        && doc.is_within(d, src_i)
    {
        return Err(Error::new(
            Code::Overlap,
            "cannot move a section into its own subtree",
        ));
    }

    // The moved section keeps its heading level, so it must land as a sibling
    // of the destination anchor (top-level at the root) — any other level would
    // silently re-parent surrounding sections (spec: content heading 계층 검증).
    let moved_level = doc.headings[src_i].level;
    let expected = sibling_level(doc, dest_idx);
    if moved_level != expected {
        return Err(Error::new(
            Code::HeadingLevel,
            format!(
                "moved section's heading is level {moved_level}, expected {expected} at the destination"
            ),
        ));
    }

    let src_span = doc.section_span(src_i);
    let (d_start, d_end) = section_bounds(doc, dest_idx);
    let dest_pos = match dest.position {
        Position::Before => d_start,
        Position::After => d_end,
    };

    let mut moved = src_span.of(source).to_string();
    pad_block_seams(&mut moved, source, dest_pos);

    Ok(vec![
        Splice {
            start: src_span.start,
            end: src_span.end,
            replacement: String::new(),
            footprint: (src_span.start, src_span.end),
        },
        Splice {
            start: dest_pos,
            end: dest_pos,
            replacement: moved,
            footprint: (dest_pos, dest_pos),
        },
    ])
}

/// Pad `block` so a blank line separates it from both neighbours of the
/// zero-width insertion at `pos`
/// ([ADR-0025](../../../docs/adr/0025-insertion-placement.md)): a sibling
/// block — inserted or moved — is never glued to the sections around it.
/// Only ever adds, never removes; nothing is added at the document edges.
fn pad_block_seams(block: &mut String, source: &str, pos: usize) {
    if !block.ends_with('\n') {
        block.push('\n');
    }
    let bytes = source.as_bytes();
    if pos > 0 {
        if bytes[pos - 1] != b'\n' {
            block.insert_str(0, "\n\n");
        } else if pos >= 2 && bytes[pos - 2] != b'\n' {
            block.insert(0, '\n');
        }
    }
    if pos < source.len() && !block.ends_with("\n\n") {
        block.push('\n');
    }
}

/// The separator blank lines at each end of the span `start..end`: the runs of
/// whitespace-only lines it opens and closes with. `replace` restores these so
/// an edit keeps the note's own spacing around a heading
/// ([ADR-0025](../../../docs/adr/0025-insertion-placement.md) seams, applied to
/// an existing span instead of a new block). A span that is *only* separator
/// yields one line for each side rather than duplicating the whole run.
fn span_seams(source: &str, start: usize, end: usize) -> (&str, &str) {
    let lead_end = blank_run_end(source, start, end);
    if lead_end == end {
        let one = &source[start..first_line_end(source, start, end)];
        return (one, one);
    }
    (
        &source[start..lead_end],
        &source[blank_run_start(source, lead_end, end)..end],
    )
}

/// End of the run of whole whitespace-only lines starting at `start`.
fn blank_run_end(source: &str, start: usize, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut pos = start;
    while let Some(nl) = memchr_nl(bytes, pos, end) {
        if !is_blank(&bytes[pos..nl]) {
            break;
        }
        pos = nl + 1;
    }
    pos
}

/// Start of the run of whole whitespace-only lines ending at `end`.
fn blank_run_start(source: &str, start: usize, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut pos = end;
    while pos > start && bytes[pos - 1] == b'\n' {
        let line_start = bytes[start..pos - 1]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(start, |i| start + i + 1);
        if !is_blank(&bytes[line_start..pos - 1]) {
            break;
        }
        pos = line_start;
    }
    pos
}

/// End of the first line in `start..end`, newline included; `end` if unterminated.
fn first_line_end(source: &str, start: usize, end: usize) -> usize {
    memchr_nl(source.as_bytes(), start, end).map_or(end, |nl| nl + 1)
}

fn memchr_nl(bytes: &[u8], from: usize, to: usize) -> Option<usize> {
    bytes[from..to]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| from + i)
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|b| matches!(b, b' ' | b'\t' | b'\r'))
}

fn starts_with_blank_line(s: &str) -> bool {
    s.split_once('\n')
        .is_some_and(|(first, _)| is_blank(first.as_bytes()))
}

fn ends_with_blank_line(s: &str) -> bool {
    s.strip_suffix('\n').is_some_and(|body| {
        let last = body.rsplit_once('\n').map_or(body, |(_, l)| l);
        is_blank(last.as_bytes())
    })
}

/// Where `append` inserts ([ADR-0025](../../../docs/adr/0025-insertion-placement.md)):
/// `append` continues the section's text, so when the span is followed by a
/// heading the point backs up over the span's trailing whitespace-only lines —
/// the separator ahead of that heading — to sit directly after the last
/// non-blank line. A span ending at EOF keeps the literal end, matching
/// `append_notes` at the note level.
fn append_pos(source: &str, start: usize, end: usize) -> usize {
    if end == source.len() {
        return end;
    }
    let bytes = source.as_bytes();
    let mut pos = end;
    while pos > start {
        // The line ending at `pos` (its newline is at pos - 1).
        let line_start = bytes[start..pos - 1]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(start, |i| start + i + 1);
        if bytes[line_start..pos]
            .iter()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            pos = line_start;
        } else {
            break;
        }
    }
    pos
}

/// The `(start, end)` of a target's whole section (heading line included for a
/// heading, the whole body for the root).
fn section_bounds(doc: &Document, index: Option<usize>) -> (usize, usize) {
    match index {
        Some(i) => {
            let s = doc.section_span(i);
            (s.start, s.end)
        }
        None => {
            let b = doc.whole_body_span();
            (b.start, b.end)
        }
    }
}

/// The level inserted content nests *under* (replace/append): one deeper than the
/// target heading, or top-level (1) at the root.
fn inside_level(doc: &Document, index: Option<usize>) -> u8 {
    index.map_or(1, |i| doc.headings[i].level + 1)
}

/// The level inserted content sits *beside* (insert_before/after): the same as
/// the anchor heading, or top-level (1) at the root.
fn sibling_level(doc: &Document, index: Option<usize>) -> u8 {
    index.map_or(1, |i| doc.headings[i].level)
}

fn require_content(edit: &Edit) -> Result<&str, Error> {
    edit.content.as_deref().ok_or_else(|| {
        Error::new(
            Code::MissingContent,
            "content is required for this operation",
        )
    })
}

/// Reject content whose shallowest heading does not sit at exactly `expected`
/// (shallower would escape the section; deeper would skip a level).
fn validate_inserted_levels(content: &str, expected: u8) -> Result<(), Error> {
    let doc = Document::parse(content);
    let Some(min) = doc.headings.iter().map(|h| h.level).min() else {
        return Ok(());
    };
    if min == expected {
        Ok(())
    } else {
        Err(Error::new(
            Code::HeadingLevel,
            format!("inserted content's shallowest heading is level {min}, expected {expected}"),
        ))
    }
}

fn check_expected_hash(
    doc: &Document,
    source: &str,
    edit: &Edit,
    index: Option<usize>,
    op: Operation,
) -> Result<(), Error> {
    let Some(expected) = edit.expected_hash.as_deref() else {
        return Ok(());
    };
    // insert/rename compare the anchor at section scope; others use the edit scope.
    let scope = match op {
        Operation::InsertBefore | Operation::InsertAfter | Operation::Rename => Scope::Section,
        _ => edit.scope,
    };
    let actual = doc.content_hash(source, index, scope);
    if actual == expected {
        Ok(())
    } else {
        Err(Error::new(
            Code::HashMismatch,
            "expected_hash does not match the current section; check that the read \
             scope (body|section) matches this edit's scope, then re-read the section",
        ))
    }
}

fn detect_overlaps(splices: &[(usize, Splice)], errors: &mut Vec<BatchError>) {
    let n = splices.len();
    let mut conflict = vec![false; n];
    for i in 0..n {
        for j in (i + 1)..n {
            if splices[i].0 == splices[j].0 {
                continue; // two splices of one edit (e.g. move) never self-conflict
            }
            let a = &splices[i].1;
            let b = &splices[j].1;
            // Real byte-range intersection (open interval): a zero-width insertion
            // footprint collides only when it lies strictly inside another range,
            // so editing different byte regions of the same heading (e.g.
            // insert_after + replace-body) is allowed while overlapping content
            // edits still reject.
            let overlap = a.footprint.0 < b.footprint.1 && b.footprint.0 < a.footprint.1;
            // Two zero-width insertions at the very same offset (e.g. append-to-A's
            // end == insert-before-B's start at the A|B seam) would apply in an
            // arbitrary, order-dependent order — reject so the result is defined.
            let same_point_insert = a.start == a.end && b.start == b.end && a.start == b.start;
            if overlap || same_point_insert {
                conflict[i] = true;
                conflict[j] = true;
            }
        }
    }
    for (k, &c) in conflict.iter().enumerate() {
        if c {
            errors.push(BatchError {
                index: splices[k].0,
                error: Error::new(
                    Code::Overlap,
                    "edit overlaps another edit in the batch; merge into one",
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(path: &[&str], op: Operation) -> Edit {
        Edit {
            heading_path: path.iter().map(|s| (*s).to_string()).collect(),
            operation: Some(op),
            ..Edit::default()
        }
    }

    fn apply(source: &str, edits: Vec<Edit>) -> String {
        patch_sections(source, &edits).unwrap()
    }

    #[test]
    fn replace_body_keeps_subsections() {
        let src = "# A\nold lead\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("new lead".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\nnew lead\n## B\nsub\n");
    }

    #[test]
    fn replace_section_replaces_subtree() {
        let src = "# A\nold lead\n## B\nsub\n# C\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("fresh\n".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\nfresh\n# C\n");
    }

    #[test]
    fn append_body_adds_before_first_subheading() {
        let src = "# A\nlead\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("more".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "# A\nlead\nmore\n## B\nsub\n");
    }

    #[test]
    fn append_section_adds_at_end_of_subtree() {
        let src = "# A\nlead\n## B\nsub\n# C\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("tail".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "# A\nlead\n## B\nsub\ntail\n# C\n");
    }

    #[test]
    fn append_body_joins_text_before_the_separator_blank() {
        // ADR-0025: the blank line ahead of the next heading is a separator,
        // not content — appended text continues the body and leaves it be.
        let src = "# A\nlead\n\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("more".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "# A\nlead\nmore\n\n## B\nsub\n");
    }

    #[test]
    fn append_section_joins_text_before_the_separator_blank() {
        let src = "# A\nlead\n## B\nsub\n\n# C\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("tail".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "# A\nlead\n## B\nsub\ntail\n\n# C\n");
    }

    #[test]
    fn append_skips_whitespace_only_lines_not_just_empty_ones() {
        let src = "# A\nlead\n \t\n\n## B\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("more".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "# A\nlead\nmore\n \t\n\n## B\n");
    }

    #[test]
    fn append_to_blank_only_body_lands_under_the_heading() {
        let src = "## A\n\n## B\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("more".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "## A\nmore\n\n## B\n");
    }

    #[test]
    fn append_at_eof_keeps_the_literal_end() {
        // No following heading, no separator to preserve: append stays literal,
        // matching append_notes at the note level.
        let src = "# A\ntext\n\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("more".into()),
            ..edit(&["A"], Operation::Append)
        };
        assert_eq!(apply(src, vec![e]), "# A\ntext\n\nmore\n");
    }

    #[test]
    fn replace_empty_body_does_not_glue_to_next_heading() {
        // Regression: replacing the (empty) body of a heading that is immediately
        // followed by a subheading must not merge into it.
        let src = "# A\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("X".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\nX\n## B\nsub\n");
    }

    #[test]
    fn replace_body_on_heading_only_note_separates_from_heading() {
        // Regression: a heading-only note with no trailing newline.
        let src = "# A";
        let e = Edit {
            scope: Scope::Body,
            content: Some("X".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\nX");
    }

    #[test]
    fn replace_body_preserves_the_blank_seams_around_it() {
        // The blank line under a heading and the one ahead of the next heading
        // are separators, not body content: replacing the body keeps both.
        let src = "# A\n\nold lead\n\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("new lead".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\n\nnew lead\n\n## B\nsub\n");
    }

    #[test]
    fn replace_section_preserves_the_blank_seams_around_it() {
        let src = "# A\n\nold\n\n# B\nb\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("new".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\n\nnew\n\n# B\nb\n");
    }

    #[test]
    fn replace_does_not_double_seams_the_content_already_carries() {
        // Content echoed back from a read already starts and ends with the
        // separators; restoring them again would grow the note on every edit.
        let src = "# A\n\nold\n\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("\nnew\n\n".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\n\nnew\n\n## B\nsub\n");
    }

    #[test]
    fn replace_blank_only_body_keeps_the_blank_on_both_seams() {
        let src = "## A\n\n## B\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("X".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "## A\n\nX\n\n## B\n");
    }

    #[test]
    fn replace_root_body_preserves_the_blank_after_frontmatter() {
        let src = "---\nt: x\n---\n\nintro\n\n# A\na\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("new intro".into()),
            ..edit(&[], Operation::Replace)
        };
        assert_eq!(
            apply(src, vec![e]),
            "---\nt: x\n---\n\nnew intro\n\n# A\na\n"
        );
    }

    #[test]
    fn move_to_adjacent_sibling_boundary_is_allowed() {
        // Regression: a sibling move whose destination touches the source span
        // boundary must not be a false self-move.
        let src = "# A\nax\n# B\nbx\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["A".into()],
            destination: Some(Destination {
                heading_path: vec!["B".into()],
                occurrence: None,
                position: Position::Before,
            }),
            ..Edit::default()
        };
        assert!(patch_sections(src, &[e]).is_ok());
    }

    #[test]
    fn insert_after_and_replace_body_of_same_heading_do_not_conflict() {
        // Regression: editing different byte regions of one heading is allowed.
        let src = "# A\nbody\n# B\nb\n";
        let ins = Edit {
            content: Some("# New\nn\n".into()),
            ..edit(&["A"], Operation::InsertAfter)
        };
        let rep = Edit {
            scope: Scope::Body,
            content: Some("new".into()),
            ..edit(&["A"], Operation::Replace)
        };
        let out = apply(src, vec![ins, rep]);
        assert_eq!(out, "# A\nnew\n\n# New\nn\n\n# B\nb\n");
    }

    #[test]
    fn delete_body_keeps_heading_and_subsections() {
        let src = "# A\nlead\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Body,
            ..edit(&["A"], Operation::Delete)
        };
        assert_eq!(apply(src, vec![e]), "# A\n## B\nsub\n");
    }

    #[test]
    fn delete_section_removes_everything() {
        let src = "# A\nlead\n## B\nsub\n# C\n";
        let e = Edit {
            scope: Scope::Section,
            ..edit(&["A"], Operation::Delete)
        };
        assert_eq!(apply(src, vec![e]), "# C\n");
    }

    #[test]
    fn insert_before_creates_sibling() {
        // ADR-0025: an inserted block gets a blank line at both seams.
        let src = "# B\nx\n";
        let e = Edit {
            content: Some("# A\nnew\n".into()),
            ..edit(&["B"], Operation::InsertBefore)
        };
        assert_eq!(apply(src, vec![e]), "# A\nnew\n\n# B\nx\n");
    }

    #[test]
    fn insert_after_creates_sibling() {
        let src = "# A\nx\n";
        let e = Edit {
            content: Some("# B\nnew".into()),
            ..edit(&["A"], Operation::InsertAfter)
        };
        assert_eq!(apply(src, vec![e]), "# A\nx\n\n# B\nnew\n");
    }

    #[test]
    fn insert_between_sections_separates_both_seams() {
        let src = "# A\nx\n# B\ny\n";
        let e = Edit {
            content: Some("# N\nn\n".into()),
            ..edit(&["A"], Operation::InsertAfter)
        };
        assert_eq!(apply(src, vec![e]), "# A\nx\n\n# N\nn\n\n# B\ny\n");
    }

    #[test]
    fn insert_does_not_double_an_existing_blank_seam() {
        let src = "# A\nbody\n\n# B\nx\n";
        let e = Edit {
            content: Some("# N\nn\n".into()),
            ..edit(&["B"], Operation::InsertBefore)
        };
        assert_eq!(apply(src, vec![e]), "# A\nbody\n\n# N\nn\n\n# B\nx\n");
    }

    #[test]
    fn rename_changes_heading_only() {
        let src = "# Old\nbody\n## Sub\n";
        let e = Edit {
            new_heading: Some("New".into()),
            ..edit(&["Old"], Operation::Rename)
        };
        assert_eq!(apply(src, vec![e]), "# New\nbody\n## Sub\n");
    }

    #[test]
    fn move_section_before_another() {
        let src = "# A\nax\n# B\nbx\n# C\ncx\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["C".into()],
            destination: Some(Destination {
                heading_path: vec!["A".into()],
                occurrence: None,
                position: Position::Before,
            }),
            ..Edit::default()
        };
        assert_eq!(apply(src, vec![e]), "# C\ncx\n\n# A\nax\n# B\nbx\n");
    }

    #[test]
    fn move_section_after_another() {
        let src = "# A\nax\n# B\nbx\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["A".into()],
            destination: Some(Destination {
                heading_path: vec!["B".into()],
                occurrence: None,
                position: Position::After,
            }),
            ..Edit::default()
        };
        assert_eq!(apply(src, vec![e]), "# B\nbx\n\n# A\nax\n");
    }

    #[test]
    fn move_into_own_subtree_is_rejected() {
        let src = "# A\nlead\n## B\nsub\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["A".into()],
            destination: Some(Destination {
                heading_path: vec!["A".into(), "B".into()],
                occurrence: None,
                position: Position::After,
            }),
            ..Edit::default()
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::Overlap
        );
    }

    #[test]
    fn rename_rejects_newline_in_new_heading() {
        // A newline would split the heading line: `## Multi` + a stray body
        // line `Line`, and the echoed heading_path could never be resolved.
        let src = "# A\nbody\n";
        for bad in ["Multi\nLine", "CR\rLine", "", "   "] {
            let e = Edit {
                new_heading: Some(bad.into()),
                ..edit(&["A"], Operation::Rename)
            };
            assert_eq!(
                patch_sections(src, &[e]).unwrap_err()[0].error.code,
                Code::InvalidHeading,
                "new_heading {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn move_with_shallower_level_than_destination_is_rejected() {
        // Moving `# B` (h1) beside `## A1` (h2) would cut section A in half.
        let src = "# A\nlead\n## A1\na1\n## A2\na2\n# B\nb\n## B1\nb1\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["B".into()],
            destination: Some(Destination {
                heading_path: vec!["A".into(), "A1".into()],
                occurrence: None,
                position: Position::Before,
            }),
            ..Edit::default()
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::HeadingLevel
        );
    }

    #[test]
    fn move_with_deeper_level_than_destination_is_rejected() {
        // Moving `## A1` (h2) beside `# B` (h1) would nest it under B, not
        // beside it — the echoed sibling heading_path would be a lie.
        let src = "# A\nlead\n## A1\na1\n# B\nb\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["A".into(), "A1".into()],
            destination: Some(Destination {
                heading_path: vec!["B".into()],
                occurrence: None,
                position: Position::After,
            }),
            ..Edit::default()
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::HeadingLevel
        );
    }

    #[test]
    fn move_to_root_requires_top_level_heading() {
        let src = "# A\nlead\n## A1\na1\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["A".into(), "A1".into()],
            destination: Some(Destination {
                heading_path: vec![],
                occurrence: None,
                position: Position::After,
            }),
            ..Edit::default()
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::HeadingLevel
        );
    }

    #[test]
    fn move_same_level_subsection_between_parents() {
        let src = "# A\nax\n## A1\na1\n# B\nbx\n## B1\nb1\n";
        let e = Edit {
            operation: Some(Operation::Move),
            heading_path: vec!["A".into(), "A1".into()],
            destination: Some(Destination {
                heading_path: vec!["B".into(), "B1".into()],
                occurrence: None,
                position: Position::After,
            }),
            ..Edit::default()
        };
        assert_eq!(
            apply(src, vec![e]),
            "# A\nax\n# B\nbx\n## B1\nb1\n\n## A1\na1\n"
        );
    }

    #[test]
    fn root_replace_and_insert() {
        let src = "intro\n# A\n";
        let replace = Edit {
            scope: Scope::Body,
            content: Some("new intro\n".into()),
            ..edit(&[], Operation::Replace)
        };
        assert_eq!(apply(src, vec![replace]), "new intro\n# A\n");
    }

    // --- batch snapshot + overlap ------------------------------------------

    #[test]
    fn batch_two_disjoint_sections_apply_against_snapshot() {
        let src = "# A\na-body\n# B\nb-body\n";
        let ea = Edit {
            scope: Scope::Body,
            content: Some("AA".into()),
            ..edit(&["A"], Operation::Replace)
        };
        let eb = Edit {
            scope: Scope::Body,
            content: Some("BB".into()),
            ..edit(&["B"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![ea, eb]), "# A\nAA\n# B\nBB\n");
    }

    #[test]
    fn batch_same_section_twice_is_rejected() {
        let src = "# A\nbody\n";
        let e1 = Edit {
            scope: Scope::Body,
            content: Some("x".into()),
            ..edit(&["A"], Operation::Replace)
        };
        let e2 = Edit {
            scope: Scope::Body,
            content: Some("y".into()),
            ..edit(&["A"], Operation::Append)
        };
        let errs = patch_sections(src, &[e1, e2]).unwrap_err();
        assert_eq!(errs.len(), 2);
        assert!(errs.iter().all(|e| e.error.code == Code::Overlap));
    }

    #[test]
    fn batch_parent_and_child_section_is_rejected() {
        let src = "# A\nlead\n## B\nsub\n";
        let parent = Edit {
            scope: Scope::Section,
            ..edit(&["A"], Operation::Delete)
        };
        let child = Edit {
            scope: Scope::Body,
            content: Some("x".into()),
            ..edit(&["A", "B"], Operation::Replace)
        };
        let errs = patch_sections(src, &[parent, child]).unwrap_err();
        assert!(errs.iter().any(|e| e.error.code == Code::Overlap));
    }

    #[test]
    fn batch_two_zero_width_inserts_at_one_seam_are_rejected() {
        // Appending to A's end and inserting before B both land at the A|B seam;
        // their order would be arbitrary, so the batch is rejected.
        let src = "# A\nax\n# B\nbx\n";
        let append_a = Edit {
            scope: Scope::Section,
            content: Some("APP".into()),
            ..edit(&["A"], Operation::Append)
        };
        let insert_b = Edit {
            content: Some("# New\nn\n".into()),
            ..edit(&["B"], Operation::InsertBefore)
        };
        let errs = patch_sections(src, &[append_a, insert_b]).unwrap_err();
        assert_eq!(errs.len(), 2);
        assert!(errs.iter().all(|e| e.error.code == Code::Overlap));
    }

    #[test]
    fn batch_body_and_child_section_do_not_conflict() {
        // Editing A's lead body and its subsection B are disjoint byte ranges.
        let src = "# A\nlead\n## B\nsub\n";
        let body = Edit {
            scope: Scope::Body,
            content: Some("new".into()),
            ..edit(&["A"], Operation::Replace)
        };
        let child = Edit {
            scope: Scope::Body,
            content: Some("newsub".into()),
            ..edit(&["A", "B"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![body, child]), "# A\nnew\n## B\nnewsub\n");
    }

    // --- validation ---------------------------------------------------------

    #[test]
    fn missing_content_is_rejected() {
        let src = "# A\n";
        let e = edit(&["A"], Operation::Replace); // no content
        let errs = patch_sections(src, &[e]).unwrap_err();
        assert_eq!(errs[0].error.code, Code::MissingContent);
    }

    #[test]
    fn unresolved_heading_is_rejected() {
        let src = "# A\n";
        let e = Edit {
            scope: Scope::Body,
            content: Some("x".into()),
            ..edit(&["Nope"], Operation::Replace)
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::NotFound
        );
    }

    #[test]
    fn heading_level_escape_is_rejected() {
        // Replacing a level-2 section's content with a level-1 heading escapes it.
        let src = "# A\n## B\nsub\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("# Escapes\n".into()),
            ..edit(&["A", "B"], Operation::Replace)
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::HeadingLevel
        );
    }

    #[test]
    fn heading_level_correct_nesting_is_accepted() {
        let src = "# A\nlead\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("lead\n## Child\nx\n".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\nlead\n## Child\nx\n");
    }

    #[test]
    fn expected_hash_mismatch_is_rejected() {
        let src = "# A\nbody\n";
        let e = Edit {
            scope: Scope::Section,
            content: Some("x\n".into()),
            expected_hash: Some("deadbeef".into()),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(
            patch_sections(src, &[e]).unwrap_err()[0].error.code,
            Code::HashMismatch
        );
    }

    #[test]
    fn expected_hash_match_is_accepted() {
        let src = "# A\nbody\n";
        let doc = Document::parse(src);
        let i = doc.resolve_heading(&["A".into()], None).unwrap();
        let hash = doc.content_hash(src, Some(i), Scope::Section);
        let e = Edit {
            scope: Scope::Section,
            content: Some("changed\n".into()),
            expected_hash: Some(hash),
            ..edit(&["A"], Operation::Replace)
        };
        assert_eq!(apply(src, vec![e]), "# A\nchanged\n");
    }
}
