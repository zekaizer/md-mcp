//! Literal text substitution: the pure text transformation behind `replace_text`
//! ([ADR-0027](../../../docs/adr/0027-literal-text-replacement.md)).
//!
//! [`replace_text`] matches `find` byte-for-byte against the LF-normalized
//! source — no regex, no case folding, no CJK gap folding, no Unicode
//! normalization — inside the note body (or one section of it), and rejects any
//! item whose match count violates its contract. Like [`crate::patch`], every
//! item resolves against the **original** snapshot and the batch is
//! all-or-nothing: overlapping items fail together and nothing is changed.

use crate::document::Document;
use crate::error::{Code, Error};
use crate::patch::BatchError;
use crate::section::Scope;
use crate::text::{line_starts, normalize_newlines};

/// One substitution in a `replace_text` batch.
#[derive(Clone, Debug, Default)]
pub struct Replacement {
    /// The section to search in; empty addresses the whole note body.
    pub heading_path: Vec<String>,
    pub occurrence: Option<usize>,
    pub scope: Scope,
    /// The literal string to find. Must not be empty.
    pub find: String,
    /// What each match becomes (empty deletes the match).
    pub replace: String,
    /// Replace every match instead of requiring exactly one.
    pub replace_all: bool,
    /// Assert the match count; all matches are replaced when it holds.
    pub expected_count: Option<usize>,
    pub expected_hash: Option<String>,
}

/// One applied substitution, addressed in the **new** text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// 1-based line number of the replacement in the resulting note.
    pub line: usize,
    /// That line's text after the replacement, newline excluded.
    pub text: String,
}

struct Splice {
    item: usize,
    start: usize,
    end: usize,
}

/// Apply a batch of literal replacements to LF-normalized source, returning the
/// new text and each item's hits (in item order). All-or-nothing: on any
/// rejection nothing is changed and every offending item is reported.
///
/// # Errors
///
/// Returns one [`BatchError`] per offending item: an empty `find`, an
/// unresolved or ambiguous `heading_path`, a stale `expected_hash`, a match
/// count that violates the item's contract, or a span overlapping another
/// item's.
pub fn replace_text(
    source: &str,
    items: &[Replacement],
) -> Result<(String, Vec<Vec<Hit>>), Vec<BatchError>> {
    let normalized = normalize_newlines(source);
    let source = normalized.as_ref();
    let doc = Document::parse(source);

    let mut errors: Vec<BatchError> = Vec::new();
    let mut splices: Vec<Splice> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match resolve(&doc, source, item) {
            Ok(spans) => splices.extend(spans.into_iter().map(|(start, end)| Splice {
                item: i,
                start,
                end,
            })),
            Err(error) => errors.push(BatchError { index: i, error }),
        }
    }

    detect_overlaps(&splices, &mut errors);

    if !errors.is_empty() {
        errors.sort_by_key(|e| e.index);
        return Err(errors);
    }

    splices.sort_by_key(|s| s.start);
    let mut result = source.to_string();
    for s in splices.iter().rev() {
        result.replace_range(s.start..s.end, &items[s.item].replace);
    }

    // Re-address each match in the new text: everything spliced before it has
    // already shifted it by the accumulated length delta.
    let starts = line_starts(&result);
    let mut hits: Vec<Vec<Hit>> = vec![Vec::new(); items.len()];
    let mut delta: isize = 0;
    for s in &splices {
        let new_start = (s.start as isize + delta) as usize;
        let line = starts.partition_point(|&b| b <= new_start);
        let from = starts[line - 1];
        let to = starts.get(line).map_or(result.len(), |&b| b - 1);
        hits[s.item].push(Hit {
            line,
            text: result[from..to].to_string(),
        });
        delta += items[s.item].replace.len() as isize - (s.end - s.start) as isize;
    }
    Ok((result, hits))
}

/// The byte spans one item replaces, or why it is rejected.
fn resolve(doc: &Document, source: &str, item: &Replacement) -> Result<Vec<(usize, usize)>, Error> {
    if item.find.is_empty() {
        return Err(Error::new(Code::MissingContent, "find must not be empty"));
    }
    let index = if item.heading_path.is_empty() {
        None
    } else {
        Some(doc.resolve_heading(&item.heading_path, item.occurrence)?)
    };
    if let Some(expected) = item.expected_hash.as_deref() {
        let actual = doc.content_hash(source, index, item.scope);
        if actual != expected {
            return Err(Error::new(
                Code::HashMismatch,
                "expected_hash does not match the current section; check that the read \
                 scope (body|section) matches this item's scope, then re-read the section",
            ));
        }
    }

    let span = doc.content_span(index, item.scope);
    let spans: Vec<(usize, usize)> = span
        .of(source)
        .match_indices(&item.find)
        .map(|(at, m)| (span.start + at, span.start + at + m.len()))
        .collect();

    let count = spans.len();
    if let Some(expected) = item.expected_count {
        if count != expected {
            return Err(Error::new(
                Code::CountMismatch,
                format!("find matches {count} times, expected_count is {expected}"),
            ));
        }
    } else if count == 0 {
        return Err(Error::new(
            Code::NotFound,
            "find does not occur in the searched text; it is matched literally, so \
             check spacing, inline markup, and case",
        ));
    } else if count > 1 && !item.replace_all {
        return Err(Error::new(
            Code::Ambiguous,
            format!(
                "find matches {count} times; narrow it (a longer find, or a heading_path), \
                 or pass replace_all/expected_count to accept every match"
            ),
        ));
    }
    Ok(spans)
}

/// Reject items whose spans intersect: their result would depend on which was
/// applied first, and both callers believe they own those bytes.
fn detect_overlaps(splices: &[Splice], errors: &mut Vec<BatchError>) {
    let n = splices.len();
    let mut conflict = vec![false; n];
    for i in 0..n {
        for j in (i + 1)..n {
            if splices[i].item == splices[j].item {
                continue; // one item's own matches never overlap each other
            }
            if splices[i].start < splices[j].end && splices[j].start < splices[i].end {
                conflict[i] = true;
                conflict[j] = true;
            }
        }
    }
    let mut reported: Vec<usize> = Vec::new();
    for (k, &c) in conflict.iter().enumerate() {
        let item = splices[k].item;
        if c && !reported.contains(&item) {
            reported.push(item);
            errors.push(BatchError {
                index: item,
                error: Error::new(
                    Code::Overlap,
                    "replacement overlaps another item in the batch; merge into one",
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(find: &str, replace: &str) -> Replacement {
        Replacement {
            find: find.to_string(),
            replace: replace.to_string(),
            ..Replacement::default()
        }
    }

    fn apply(source: &str, items: Vec<Replacement>) -> String {
        replace_text(source, &items).unwrap().0
    }

    fn errors(source: &str, items: Vec<Replacement>) -> Vec<(usize, Code)> {
        replace_text(source, &items)
            .unwrap_err()
            .into_iter()
            .map(|e| (e.index, e.error.code))
            .collect()
    }

    #[test]
    fn single_match_is_replaced() {
        assert_eq!(
            apply("# A\nthe qiuck fox\n", vec![item("qiuck", "quick")]),
            "# A\nthe quick fox\n"
        );
    }

    #[test]
    fn zero_matches_reject_not_found() {
        assert_eq!(
            errors("# A\nbody\n", vec![item("absent", "x")]),
            vec![(0, Code::NotFound)]
        );
    }

    #[test]
    fn two_matches_reject_ambiguous_by_default() {
        assert_eq!(
            errors("a and a\n", vec![item("a", "b")]),
            vec![(0, Code::Ambiguous)]
        );
    }

    #[test]
    fn replace_all_replaces_every_match() {
        let it = Replacement {
            replace_all: true,
            ..item("a", "b")
        };
        assert_eq!(apply("a and a\n", vec![it]), "b bnd b\n");
    }

    #[test]
    fn replace_all_still_requires_a_match() {
        let it = Replacement {
            replace_all: true,
            ..item("absent", "x")
        };
        assert_eq!(errors("body\n", vec![it]), vec![(0, Code::NotFound)]);
    }

    #[test]
    fn expected_count_asserts_the_match_count() {
        let ok = Replacement {
            expected_count: Some(2),
            ..item("cat", "dog")
        };
        assert_eq!(apply("cat cat\n", vec![ok]), "dog dog\n");

        let wrong = Replacement {
            expected_count: Some(3),
            ..item("cat", "dog")
        };
        assert_eq!(
            errors("cat cat\n", vec![wrong]),
            vec![(0, Code::CountMismatch)]
        );
    }

    #[test]
    fn heading_path_narrows_the_search() {
        let src = "# A\nterm\n# B\nterm\n";
        let it = Replacement {
            heading_path: vec!["B".into()],
            ..item("term", "TERM")
        };
        assert_eq!(apply(src, vec![it]), "# A\nterm\n# B\nTERM\n");
    }

    #[test]
    fn body_scope_excludes_subsections() {
        let src = "# A\nterm\n## B\nterm\n";
        let it = Replacement {
            heading_path: vec!["A".into()],
            scope: Scope::Body,
            ..item("term", "TERM")
        };
        assert_eq!(apply(src, vec![it]), "# A\nTERM\n## B\nterm\n");
    }

    #[test]
    fn frontmatter_is_never_searched() {
        let src = "---\ntitle: draft\n---\n\ndraft\n";
        assert_eq!(
            apply(src, vec![item("draft", "final")]),
            "---\ntitle: draft\n---\n\nfinal\n"
        );
    }

    #[test]
    fn hits_report_the_line_and_its_new_text() {
        let (_, hits) = replace_text("a\nb qiuck c\nd\n", &[item("qiuck", "quick")]).unwrap();
        assert_eq!(
            hits[0],
            vec![Hit {
                line: 2,
                text: "b quick c".into()
            }]
        );
    }

    #[test]
    fn hits_address_the_text_after_earlier_splices_shifted_it() {
        // The first replacement adds a line, so the second hit's line number
        // must be read off the result, not the source.
        let src = "one\ntwo\n";
        let items = vec![item("one", "one\nextra"), item("two", "TWO")];
        let (new, hits) = replace_text(src, &items).unwrap();
        assert_eq!(new, "one\nextra\nTWO\n");
        assert_eq!(hits[0][0].line, 1);
        assert_eq!(
            hits[1][0],
            Hit {
                line: 3,
                text: "TWO".into()
            }
        );
    }

    #[test]
    fn items_resolve_against_the_original_snapshot() {
        // The first item produces "two", but the second must only see the
        // original "two" — order must not change the outcome.
        let items = vec![item("one", "two"), item("two", "three")];
        assert_eq!(apply("one two\n", items), "two three\n");
    }

    #[test]
    fn overlapping_items_reject_both() {
        let items = vec![item("abcd", "x"), item("bc", "y")];
        assert_eq!(
            errors("abcd\n", items),
            vec![(0, Code::Overlap), (1, Code::Overlap)]
        );
    }

    #[test]
    fn expected_hash_mismatch_rejects() {
        let it = Replacement {
            expected_hash: Some("deadbeef".into()),
            ..item("body", "BODY")
        };
        assert_eq!(
            errors("# A\nbody\n", vec![it]),
            vec![(0, Code::HashMismatch)]
        );
    }

    #[test]
    fn expected_hash_of_the_searched_scope_passes() {
        let src = "# A\nbody\n";
        let doc = Document::parse(src);
        let i = doc.resolve_heading(&["A".into()], None).unwrap();
        let it = Replacement {
            heading_path: vec!["A".into()],
            expected_hash: Some(doc.content_hash(src, Some(i), Scope::Section)),
            ..item("body", "BODY")
        };
        assert_eq!(apply(src, vec![it]), "# A\nBODY\n");
    }

    #[test]
    fn empty_find_is_rejected() {
        assert_eq!(
            errors("body\n", vec![item("", "x")]),
            vec![(0, Code::MissingContent)]
        );
    }

    #[test]
    fn crlf_source_is_normalized_before_matching() {
        assert_eq!(
            apply("# A\r\nqiuck\r\n", vec![item("qiuck", "quick")]),
            "# A\nquick\n"
        );
    }

    #[test]
    fn find_may_span_lines() {
        assert_eq!(apply("a\nb\nc\n", vec![item("a\nb", "A B")]), "A B\nc\n");
    }

    #[test]
    fn every_offending_item_is_reported() {
        let items = vec![item("absent", "x"), item("a", "b")];
        assert_eq!(
            errors("a and a\n", items),
            vec![(0, Code::NotFound), (1, Code::Ambiguous)]
        );
    }

    #[test]
    fn matching_is_literal_not_cjk_folded() {
        // search_notes finds 전역 지침 for the query 전역지침; replacement does not.
        assert_eq!(
            errors("전역 지침\n", vec![item("전역지침", "전역 원칙")]),
            vec![(0, Code::NotFound)]
        );
    }

    #[test]
    fn an_empty_replacement_deletes_the_match() {
        assert_eq!(apply("a TODO b\n", vec![item(" TODO", "")]), "a b\n");
    }

    #[test]
    fn unresolved_heading_path_rejects() {
        let it = Replacement {
            heading_path: vec!["Nope".into()],
            ..item("body", "x")
        };
        assert_eq!(errors("# A\nbody\n", vec![it]), vec![(0, Code::NotFound)]);
    }
}
