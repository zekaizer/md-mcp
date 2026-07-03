//! Text canonicalization primitives shared across the vault model.
//!
//! Every other module addresses note content as **bytes** and compares headings
//! and paths after the same two normalizations defined here, so they live in one
//! place: newline normalization (write is always LF), Unicode NFC normalization
//! (for path / heading comparison across NFD-authoring platforms), a byte
//! [`Span`], and a line-start index.

use std::borrow::Cow;

use unicode_normalization::{UnicodeNormalization, is_nfc};

/// A half-open byte range `[start, end)` into a source string.
///
/// All section/heading/frontmatter addressing is done with `Span`s over the
/// original bytes so edits are surgical byte splices that preserve every
/// untouched byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// Construct a span. In debug builds, panics if `start > end`.
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "span start {start} must not exceed end {end}");
        Self { start, end }
    }

    /// The number of bytes the span covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether the span is empty (zero-width).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Borrow the slice of `src` this span covers.
    #[must_use]
    pub fn of<'s>(&self, src: &'s str) -> &'s str {
        &src[self.start..self.end]
    }
}

/// Canonicalize text for parsing and writes: strip a leading UTF-8 BOM and
/// normalize CRLF / lone-CR line endings to LF, borrowing when already
/// canonical. A BOM would otherwise sit in front of `---` and silently defeat
/// frontmatter recognition; like CRLF, it is dropped when a note is rewritten.
#[must_use]
pub fn normalize_newlines(s: &str) -> Cow<'_, str> {
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    if s.contains('\r') {
        Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(s)
    }
}

/// NFC-normalize `s`, borrowing when it is already in NFC.
#[must_use]
pub fn nfc(s: &str) -> Cow<'_, str> {
    if is_nfc(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.nfc().collect())
    }
}

/// Byte offsets at which each line starts: `0`, then the index after every `\n`.
#[must_use]
pub fn line_starts(s: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_slices_and_measures() {
        let s = "hello world";
        let sp = Span::new(0, 5);
        assert_eq!(sp.of(s), "hello");
        assert_eq!(sp.len(), 5);
        assert!(!sp.is_empty());
        assert!(Span::new(3, 3).is_empty());
    }

    #[test]
    fn span_slices_multibyte_by_byte_offset() {
        let s = "## 한글 제목\n";
        // "## " is 3 bytes; "한글" is 6 bytes (3 each); slice the heading text run.
        let sp = Span::new(0, "## 한글 제목".len());
        assert_eq!(sp.of(s), "## 한글 제목");
    }

    #[test]
    fn normalize_strips_leading_bom() {
        assert_eq!(normalize_newlines("\u{feff}---\nk: 1"), "---\nk: 1");
        // BOM + CRLF together; a mid-text U+FEFF is content, not a BOM.
        assert_eq!(normalize_newlines("\u{feff}a\r\nb"), "a\nb");
        assert_eq!(normalize_newlines("a\u{feff}b"), "a\u{feff}b");
    }

    #[test]
    fn normalize_crlf_to_lf() {
        assert_eq!(normalize_newlines("a\r\nb\r\nc"), "a\nb\nc");
    }

    #[test]
    fn normalize_lone_cr_to_lf() {
        assert_eq!(normalize_newlines("a\rb\rc"), "a\nb\nc");
    }

    #[test]
    fn normalize_mixed_endings() {
        assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn normalize_already_lf_borrows() {
        let s = "a\nb\nc";
        assert!(matches!(normalize_newlines(s), Cow::Borrowed(_)));
    }

    #[test]
    fn nfc_ascii_borrows_and_is_identity() {
        let s = "Design/Schema";
        assert!(matches!(nfc(s), Cow::Borrowed(_)));
        assert_eq!(nfc(s), "Design/Schema");
    }

    #[test]
    fn nfc_composes_decomposed() {
        // NFD "e" + combining acute -> NFC "é".
        assert_eq!(nfc("e\u{0301}"), "é");
        // NFD Hangul jamo -> NFC precomposed "가".
        assert_eq!(nfc("\u{1100}\u{1161}"), "가");
    }

    #[test]
    fn nfc_already_composed_borrows() {
        assert!(matches!(nfc("가"), Cow::Borrowed(_)));
    }

    #[test]
    fn line_starts_basic() {
        assert_eq!(line_starts("ab\ncd\nef"), vec![0, 3, 6]);
    }

    #[test]
    fn line_starts_trailing_newline_points_past_end() {
        // A trailing newline yields a final start equal to the string length.
        assert_eq!(line_starts("a\n"), vec![0, 2]);
        assert_eq!(line_starts(""), vec![0]);
    }
}
