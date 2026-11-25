//! Standard-Markdown link extraction (ADR-0022).
//!
//! [`extract_links`] finds CommonMark inline links `[text](dest)`, images
//! `![alt](dest)`, and reference definitions `[label]: dest` in a note body,
//! returning byte-exact spans so a caller can splice a new destination without
//! touching anything else. Extraction is syntax-level: scheme-bearing and
//! fragment-only destinations are emitted too — filtering them is resolution
//! policy, not the extractor's. Code (fenced blocks, indented blocks, inline
//! spans) is never scanned; wikilinks `[[...]]`, autolinks `<...>`, and
//! reference *usages* `[text][label]` carry no destination and are not emitted.
//!
//! Known deviations from strict CommonMark, chosen for a rewriter over a
//! renderer: a link nested in another link's text is emitted *alongside* the
//! outer one (CommonMark gives the inner precedence), a reference definition is
//! recognized mid-paragraph, and its destination must sit on the label's line.

use std::ops::Range;

use crate::document::{Fence, line_spans};

/// Which link syntax an occurrence is. Non-exhaustive so a future syntax
/// (e.g. wikilinks, should scope ever change) is a variant, not a redesign.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkKind {
    /// `[text](dest)`
    Inline,
    /// `![alt](dest)`
    Image,
    /// `[label]: dest`
    RefDef,
}

/// One link found in a body, with splice-safe byte spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkOccurrence {
    /// The whole construct: `[..](..)`, `![..](..)`, or the refdef line's
    /// `[label]: dest ["title"]` part.
    pub span: Range<usize>,
    /// The path portion of the destination — replace exactly this range to
    /// retarget the link. Empty destinations yield an empty range.
    pub dest_span: Range<usize>,
    /// The path portion, verbatim (no percent-decoding).
    pub dest: String,
    /// The `#fragment` / `?query` tail after the path, verbatim ("" if none).
    pub suffix: String,
    /// Whether the destination is wrapped in `<...>` (may then contain spaces).
    pub angled: bool,
    pub kind: LinkKind,
}

/// Extract standard-Markdown links from a note body (frontmatter excluded by
/// the caller). Occurrences are ordered by span start.
#[must_use]
pub fn extract_links(body: &str) -> Vec<LinkOccurrence> {
    let mut mask = code_mask(body);
    let mut out = Vec::new();

    // Reference definitions first: a recognized refdef line is masked so the
    // inline scan cannot re-read its label as a link opener.
    for &(s, e) in &line_spans(body) {
        if mask[s] {
            continue;
        }
        if let Some(occ) = parse_refdef(body, s, e) {
            mask[occ.span.start..occ.span.end].fill(true);
            out.push(occ);
        }
    }

    let closers = bracket_closers(body, &mask);
    scan_inline(body, &mask, &closers, 0..body.len(), &mut out);
    out.sort_by_key(|o| o.span.start);
    out
}

/// Scan `range` for inline links/images, recursing into link text so a nested
/// image (`[![alt](img)](page)`) is emitted too.
fn scan_inline(
    src: &str,
    mask: &[bool],
    closers: &Closers,
    range: Range<usize>,
    out: &mut Vec<LinkOccurrence>,
) {
    let b = src.as_bytes();
    let mut i = range.start;
    while i < range.end {
        if mask[i] {
            i += 1;
            continue;
        }
        match b[i] {
            b'\\' => i += 2,
            b'!' if i + 1 < range.end && b[i + 1] == b'[' && !mask[i + 1] => {
                if let Some((occ, text)) = parse_inline_at(src, closers, i + 1, i, LinkKind::Image)
                {
                    let end = occ.span.end;
                    out.push(occ);
                    scan_inline(src, mask, closers, text, out);
                    i = end;
                } else {
                    i += 2;
                }
            }
            b'[' => {
                if let Some((occ, text)) = parse_inline_at(src, closers, i, i, LinkKind::Inline) {
                    let end = occ.span.end;
                    out.push(occ);
                    scan_inline(src, mask, closers, text, out);
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
}

/// Each link-text `[` mapped to the `]` that balances it.
type Closers = std::collections::HashMap<usize, usize>;

/// Pair every `[` with its balancing `]` in one pass: escapes and masked
/// (code-span) bytes are structurally inert, and a blank line ends the
/// paragraph, leaving what is still open unpaired. Scanning forward from each
/// `[` instead is quadratic in a run of unclosed brackets.
fn bracket_closers(src: &str, mask: &[bool]) -> Closers {
    let b = src.as_bytes();
    let mut closers = Closers::new();
    let mut open: Vec<usize> = Vec::new();
    let mut j = 0;
    while j < src.len() {
        if mask[j] {
            j += 1;
            continue;
        }
        match b[j] {
            b'\\' => j += 2,
            b'[' => {
                open.push(j);
                j += 1;
            }
            b']' => {
                if let Some(o) = open.pop() {
                    closers.insert(o, j);
                }
                j += 1;
            }
            b'\n' if blank_line_follows(src, j) => {
                open.clear();
                j += 1;
            }
            _ => j += 1,
        }
    }
    closers
}

/// Parse `[text](dest ["title"])` with `open` at the `[`; `span_start` is the
/// `!` for an image. Returns the occurrence and the text range (for nesting).
fn parse_inline_at(
    src: &str,
    closers: &Closers,
    open: usize,
    span_start: usize,
    kind: LinkKind,
) -> Option<(LinkOccurrence, Range<usize>)> {
    let b = src.as_bytes();
    let j = *closers.get(&open)?;
    let text = open + 1..j;
    if b.get(j + 1) != Some(&b'(') {
        return None;
    }
    let mut k = skip_ws(src, j + 2);
    let (dest_raw, angled) = parse_dest(src, k, b')')?;
    k = if angled {
        dest_raw.end + 1
    } else {
        dest_raw.end
    };
    let after = skip_ws(src, k);
    let k = if after > k && after < src.len() && b[after] != b')' {
        skip_ws(src, parse_title(src, after)?)
    } else {
        after
    };
    if b.get(k) != Some(&b')') {
        return None;
    }
    let (dest_span, dest, suffix) = split_suffix(src, dest_raw);
    Some((
        LinkOccurrence {
            span: span_start..k + 1,
            dest_span,
            dest,
            suffix,
            angled,
            kind,
        },
        text,
    ))
}

/// Parse a `[label]: dest ["title"]` line (`s..e` one line, unmasked start).
fn parse_refdef(src: &str, s: usize, e: usize) -> Option<LinkOccurrence> {
    let b = src.as_bytes();
    let line_end = src[s..e].trim_end_matches(['\r', '\n']).len() + s;
    let indent = src[s..line_end].len() - src[s..line_end].trim_start_matches(' ').len();
    if indent > 3 || b.get(s + indent) != Some(&b'[') {
        return None;
    }
    // Label: no unescaped brackets inside, non-empty, closed on this line.
    let mut j = s + indent + 1;
    while j < line_end {
        match b[j] {
            b'\\' => j += 2,
            b'[' => return None,
            b']' => break,
            _ => j += 1,
        }
    }
    if j >= line_end || j == s + indent + 1 || b.get(j + 1) != Some(&b':') {
        return None;
    }
    let k = skip_spaces(src, j + 2, line_end);
    if k >= line_end {
        return None; // a refdef requires a destination (same line here)
    }
    let (dest_raw, angled) = parse_dest(src, k, 0)?;
    if dest_raw.is_empty() || dest_raw.end > line_end {
        return None;
    }
    let mut k = if angled {
        dest_raw.end + 1
    } else {
        dest_raw.end
    };
    let after = skip_spaces(src, k, line_end);
    if after < line_end {
        if after == k {
            return None; // junk glued to the destination
        }
        k = parse_title(src, after)?;
        if skip_spaces(src, k, line_end) < line_end {
            return None; // trailing junk after the title
        }
    }
    let (dest_span, dest, suffix) = split_suffix(src, dest_raw);
    Some(LinkOccurrence {
        span: s + indent..k,
        dest_span,
        dest,
        suffix,
        angled,
        kind: LinkKind::RefDef,
    })
}

/// Parse a destination at `k`: `<...>` (no newline, escapes) or a bare run
/// (no whitespace/control, balanced parens; `terminator` also ends it, pass 0
/// for none). Returns the raw destination range and whether it was angled.
fn parse_dest(src: &str, k: usize, terminator: u8) -> Option<(Range<usize>, bool)> {
    let b = src.as_bytes();
    if b.get(k) == Some(&b'<') {
        let mut j = k + 1;
        while j < src.len() {
            match b[j] {
                b'\\' => j += 2,
                b'>' => return Some((k + 1..j, true)),
                b'\n' => return None,
                _ => j += 1,
            }
        }
        return None;
    }
    let mut j = k;
    let mut parens = 0usize;
    while j < src.len() {
        match b[j] {
            b'\\' => j += 2,
            b'(' => {
                parens += 1;
                j += 1;
            }
            b')' => {
                if parens == 0 {
                    break;
                }
                parens -= 1;
                j += 1;
            }
            c if c == terminator || c.is_ascii_whitespace() || c.is_ascii_control() => break,
            _ => j += 1,
        }
    }
    if parens != 0 {
        return None;
    }
    Some((k..j, false))
}

/// Parse a `"title"`, `'title'`, or `(title)` starting at `k`; returns the
/// index just past the closing delimiter.
fn parse_title(src: &str, k: usize) -> Option<usize> {
    let b = src.as_bytes();
    let close = match b.get(k)? {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let mut j = k + 1;
    while j < src.len() {
        match b[j] {
            b'\\' => j += 2,
            c if c == close => return Some(j + 1),
            b'\n' if blank_line_follows(src, j) => return None,
            _ => j += 1,
        }
    }
    None
}

/// Split a raw destination range into (path span, path, `#…`/`?…` suffix).
fn split_suffix(src: &str, raw: Range<usize>) -> (Range<usize>, String, String) {
    let text = &src[raw.clone()];
    let cut = text.find(['#', '?']).unwrap_or(text.len());
    (
        raw.start..raw.start + cut,
        text[..cut].to_string(),
        text[cut..].to_string(),
    )
}

/// Whether `content` (indent stripped) starts a list item: `-`, `*`, `+` or
/// digits with `.`/`)`, then a space.
fn is_list_marker(content: &str) -> bool {
    let rest = content.strip_prefix(['-', '*', '+']).or_else(|| {
        let digits = content.trim_start_matches(|c: char| c.is_ascii_digit());
        (digits.len() < content.len())
            .then(|| digits.strip_prefix(['.', ')']))
            .flatten()
    });
    rest.is_some_and(|r| r.starts_with([' ', '\t']))
}

/// Byte mask covering fenced code blocks (including the fence lines), indented
/// code blocks, and inline code spans — regions the link scan must never read.
fn code_mask(src: &str) -> Vec<bool> {
    let mut mask = vec![false; src.len()];
    let lines = line_spans(src);

    // Line pass: fenced blocks and (approximate) indented code blocks. The
    // indented-code approximation treats a ≥4-space line as code unless it
    // lazily continues a paragraph — the common cases, not full CommonMark.
    //
    // Under a list item the item's own content is indented, so there the
    // threshold is one level deeper and a fence opens at any indent. A list
    // lasts until unindented text resumes after a blank line.
    let mut fence: Option<(Fence, bool)> = None;
    let mut in_paragraph = false;
    let mut in_list = false;
    for &(s, e) in &lines {
        let line = src[s..e].trim_end_matches(['\r', '\n']);
        let indent = line.len() - line.trim_start_matches(' ').len();
        let trimmed = &line[indent..];
        let content = line.trim_start_matches([' ', '\t']);
        let width: usize = line[..line.len() - content.len()]
            .bytes()
            .map(|b| if b == b'\t' { 4 } else { 1 })
            .sum();
        if let Some((f, listed)) = fence {
            mask[s..e].fill(true);
            if (listed && f.closes(content)) || (indent <= 3 && f.closes(trimmed)) {
                fence = None;
            }
            continue;
        }
        if trimmed.is_empty() {
            in_paragraph = false;
            continue;
        }
        let opened = if in_list {
            Fence::opens(content)
        } else if indent <= 3 {
            Fence::opens(trimmed)
        } else {
            None
        };
        if let Some(f) = opened {
            fence = Some((f, in_list));
            mask[s..e].fill(true);
            continue;
        }
        let marker = is_list_marker(content);
        if width >= if in_list { 8 } else { 4 } && !in_paragraph && !marker {
            mask[s..e].fill(true);
            continue;
        }
        if marker {
            in_list = true;
        } else if width == 0 && !in_paragraph {
            in_list = false;
        }
        in_paragraph = true;
    }

    // Inline pass: a backtick run of length n closes only on a run of exactly
    // n; an unclosed run is literal. A blank line ends the paragraph and any
    // span candidate with it.
    let b = src.as_bytes();
    let mut i = 0;
    while i < src.len() {
        if mask[i] || b[i] != b'`' {
            i += 1;
            continue;
        }
        let n = run_len(b, i, b'`');
        let mut j = i + n;
        let close = loop {
            if j >= src.len() || mask[j] || (b[j] == b'\n' && blank_line_follows(src, j)) {
                break None;
            }
            if b[j] == b'`' {
                let m = run_len(b, j, b'`');
                if m == n {
                    break Some(j + m);
                }
                j += m;
            } else {
                j += 1;
            }
        };
        match close {
            Some(end) => {
                mask[i..end].fill(true);
                i = end;
            }
            None => i += n,
        }
    }
    mask
}

fn run_len(b: &[u8], i: usize, ch: u8) -> usize {
    b[i..].iter().take_while(|&&c| c == ch).count()
}

/// Whether the `\n` at `j` is followed by a blank line (paragraph break).
fn blank_line_follows(src: &str, j: usize) -> bool {
    src[j + 1..]
        .split('\n')
        .next()
        .is_some_and(|l| l.trim().is_empty())
}

/// Skip spaces, tabs, and at most one newline (CommonMark allows the
/// destination on the line after `(`, but not across a blank line).
fn skip_ws(src: &str, mut k: usize) -> usize {
    let b = src.as_bytes();
    let mut newlines = 0;
    while k < src.len() {
        match b[k] {
            b' ' | b'\t' => k += 1,
            b'\n' if newlines == 0 && !blank_line_follows(src, k) => {
                newlines = 1;
                k += 1;
            }
            _ => break,
        }
    }
    k
}

/// Skip spaces/tabs only, bounded by `end`.
fn skip_spaces(src: &str, mut k: usize, end: usize) -> usize {
    let b = src.as_bytes();
    while k < end && (b[k] == b' ' || b[k] == b'\t') {
        k += 1;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(body: &str) -> Vec<LinkOccurrence> {
        extract_links(body)
    }

    fn dests(body: &str) -> Vec<String> {
        links(body).into_iter().map(|o| o.dest).collect()
    }

    #[test]
    fn an_indented_line_under_a_list_item_is_text_not_code() {
        assert_eq!(dests("- item\n\n    see [x](t.md)\n"), ["t.md"]);
        assert_eq!(dests("- a\n\n    - [x](t.md)\n"), ["t.md"]);
        assert_eq!(dests("- a\n\n\t- [x](t.md)\n"), ["t.md"]);
        assert_eq!(dests("1. a\n\n   b\n\n    [x](t.md)\n"), ["t.md"]);
    }

    #[test]
    fn code_under_a_list_item_is_still_code() {
        assert!(dests("1. step\n    ~~~\n    [x](t.md)\n    ~~~\n").is_empty());
        assert!(dests("- step\n\t~~~\n\t[x](t.md)\n\t~~~\n").is_empty());
        assert!(dests("- step\n\n        [x](t.md)\n").is_empty());
    }

    #[test]
    fn a_list_ends_where_unindented_text_resumes() {
        assert!(dests("- item\n\npara\n\n    [x](t.md)\n").is_empty());
    }

    #[test]
    fn unclosed_brackets_scan_in_linear_time() {
        let body = "[".repeat(200_000);
        let started = std::time::Instant::now();

        assert!(links(&body).is_empty());

        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    // --- basic kinds ---------------------------------------------------

    #[test]
    fn inline_link_basic() {
        let l = links("see [note](a.md) here");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].kind, LinkKind::Inline);
        assert_eq!(l[0].dest, "a.md");
        assert_eq!(&"see [note](a.md) here"[l[0].span.clone()], "[note](a.md)");
        assert_eq!(&"see [note](a.md) here"[l[0].dest_span.clone()], "a.md");
    }

    #[test]
    fn image_link() {
        let l = links("![alt](img/pic.png)");
        assert_eq!(l[0].kind, LinkKind::Image);
        assert_eq!(l[0].dest, "img/pic.png");
        assert_eq!(l[0].span, 0..19);
    }

    #[test]
    fn refdef_basic() {
        let src = "[ref]: notes/a.md\n\nuses [text][ref]\n";
        let l = links(src);
        assert_eq!(l.len(), 1, "usage [text][ref] has no dest of its own");
        assert_eq!(l[0].kind, LinkKind::RefDef);
        assert_eq!(l[0].dest, "notes/a.md");
    }

    #[test]
    fn refdef_with_title_excludes_title() {
        let src = "[r]: a.md \"the title\"\n";
        let l = links(src);
        assert_eq!(l[0].dest, "a.md");
        assert_eq!(&src[l[0].dest_span.clone()], "a.md");
    }

    #[test]
    fn refdef_four_space_indent_is_code_not_refdef() {
        assert!(links("    [r]: a.md\n").is_empty());
        assert_eq!(links("   [r]: a.md\n").len(), 1); // 3 spaces is fine
    }

    #[test]
    fn refdef_requires_destination_and_clean_tail() {
        assert!(links("[r]:\n").is_empty());
        assert!(links("[r]: a.md junk junk\n").is_empty());
        assert!(links("[]: a.md\n").is_empty());
    }

    // --- destination anatomy -------------------------------------------

    #[test]
    fn fragment_and_query_split_into_suffix() {
        let l = links("[t](a.md#sec) [u](b.md?x=1#h)");
        assert_eq!((l[0].dest.as_str(), l[0].suffix.as_str()), ("a.md", "#sec"));
        assert_eq!(
            (l[1].dest.as_str(), l[1].suffix.as_str()),
            ("b.md", "?x=1#h")
        );
    }

    #[test]
    fn title_excluded_from_dest() {
        for src in ["[t](a.md \"hi\")", "[t](a.md 'hi')", "[t](a.md (hi))"] {
            let l = links(src);
            assert_eq!(l[0].dest, "a.md", "in {src:?}");
        }
    }

    #[test]
    fn angle_dest_allows_spaces() {
        let src = "[t](<my note.md#top>)";
        let l = links(src);
        assert!(l[0].angled);
        assert_eq!(l[0].dest, "my note.md");
        assert_eq!(l[0].suffix, "#top");
        assert_eq!(&src[l[0].dest_span.clone()], "my note.md");
    }

    #[test]
    fn empty_dest_yields_empty_range() {
        let l = links("[t]()");
        assert_eq!(l[0].dest, "");
        assert!(l[0].dest_span.is_empty());
    }

    #[test]
    fn percent_encoding_kept_verbatim() {
        assert_eq!(dests("[t](my%20note.md)"), ["my%20note.md"]);
    }

    #[test]
    fn balanced_parens_in_bare_dest() {
        assert_eq!(dests("[t](a(1).md)"), ["a(1).md"]);
        assert!(links("[t](a(1.md)").is_empty(), "unbalanced paren");
    }

    #[test]
    fn scheme_urls_are_emitted_extraction_is_syntax_level() {
        let l = links("[x](https://e.com/p) [m](mailto:a@b.c)");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].dest, "https://e.com/p");
    }

    #[test]
    fn dest_on_next_line_after_paren() {
        assert_eq!(dests("[t](\n  a.md)"), ["a.md"]);
    }

    // --- what must NOT be extracted ------------------------------------

    #[test]
    fn wikilinks_and_embeds_not_extracted() {
        assert!(links("[[wiki]] and ![[embed.png]] stay").is_empty());
    }

    #[test]
    fn autolink_not_extracted() {
        assert!(links("<https://example.com>").is_empty());
    }

    #[test]
    fn reference_usages_not_extracted() {
        assert!(links("[full][ref] [collapsed][] [shortcut]").is_empty());
    }

    #[test]
    fn escaped_bracket_is_not_a_link() {
        assert!(links("\\[t](a.md)").is_empty());
    }

    #[test]
    fn unterminated_forms_not_extracted() {
        assert!(links("[t](a.md").is_empty());
        assert!(links("[t (a.md)").is_empty());
        assert!(links("[t](<a.md)").is_empty());
    }

    #[test]
    fn blank_line_breaks_a_link() {
        assert!(links("[t\n\n](a.md)").is_empty());
    }

    // --- code masking ---------------------------------------------------

    #[test]
    fn inline_code_masked() {
        assert!(links("`[t](a.md)`").is_empty());
        // matching run lengths: `` `a` `` closes only on exactly one backtick
        assert_eq!(dests("``code`` then [t](a.md)"), ["a.md"]);
    }

    #[test]
    fn code_span_backtick_lengths_must_match() {
        // The ``…`` span swallows the single ` inside it.
        assert!(links("`` `[t](a.md)` ``").is_empty());
    }

    #[test]
    fn fenced_block_masked() {
        for fence in ["```", "~~~"] {
            let src = format!("{fence}\n[t](a.md)\n{fence}\nafter [u](b.md)\n");
            assert_eq!(dests(&src), ["b.md"], "fence {fence}");
        }
    }

    #[test]
    fn indented_code_masked_but_lazy_continuation_is_not() {
        assert!(links("\n    [t](a.md)\n").is_empty());
        // A ≥4-space line continuing a paragraph is still paragraph text.
        assert_eq!(dests("text\n    [t](a.md)\n"), ["a.md"]);
    }

    #[test]
    fn unclosed_backtick_run_is_literal() {
        assert_eq!(dests("a ` b [t](a.md)"), ["a.md"]);
    }

    // --- structure ------------------------------------------------------

    #[test]
    fn brackets_in_link_text_balance() {
        let l = links("[a[b]c](x.md)");
        assert_eq!(l[0].dest, "x.md");
    }

    #[test]
    fn code_span_inside_link_text_is_inert() {
        let src = "[see `]` here](x.md)";
        let l = links(src);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].dest, "x.md");
    }

    #[test]
    fn nested_image_inside_link_both_emitted() {
        let src = "[![badge](b.png)](target.md)";
        let l = links(src);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].kind, LinkKind::Inline);
        assert_eq!(l[0].dest, "target.md");
        assert_eq!(l[1].kind, LinkKind::Image);
        assert_eq!(l[1].dest, "b.png");
    }

    #[test]
    fn multiple_links_ordered_with_exact_spans() {
        let src = "[a](1.md) mid [b](2.md)";
        let l = links(src);
        assert_eq!(&src[l[0].span.clone()], "[a](1.md)");
        assert_eq!(&src[l[1].span.clone()], "[b](2.md)");
    }

    #[test]
    fn korean_text_and_path() {
        let l = links("[한글 노트](노트/메모.md#섹션)");
        assert_eq!(l[0].dest, "노트/메모.md");
        assert_eq!(l[0].suffix, "#섹션");
    }

    #[test]
    fn link_text_may_span_a_soft_break() {
        assert_eq!(dests("[two\nlines](a.md)"), ["a.md"]);
    }

    // --- the anti-corruption property ------------------------------------

    #[test]
    fn splicing_dest_spans_rewrites_only_destinations() {
        let src = "# H\n\n[a](old/a.md#top) text ![i](old/i.png)\n\n[r]: old/r.md \"t\"\n\n`[k](old/k.md)`\n";
        let mut out = src.to_string();
        // Replace every dest with a marker, back to front so spans stay valid.
        let mut occ = extract_links(src);
        occ.sort_by_key(|o| std::cmp::Reverse(o.dest_span.start));
        for o in &occ {
            out.replace_range(o.dest_span.clone(), "NEW");
        }
        assert_eq!(
            out,
            "# H\n\n[a](NEW#top) text ![i](NEW)\n\n[r]: NEW \"t\"\n\n`[k](old/k.md)`\n"
        );
    }
}
