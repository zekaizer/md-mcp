//! Fold the gaps that CJK spacing and inline style open inside a single term.
//!
//! Korean spaces words optionally, so a reader searching `전역지침` expects to
//! find `전역 지침`, and inline style (`**전역** 지침`) splits a term in the
//! source that reads as one. [`fold`] builds a shadow copy of the text with
//! those gaps removed plus an offset map back to the original bytes, so
//! `search_notes` can match on the shadow while snippets still address the real
//! note ([ADR-0010](../../../docs/adr/0010-search-strategy.md)).
//!
//! A gap is only removed when the characters on **both** sides are CJK. That
//! scope, not any understanding of Markdown, is what makes the crude character
//! test safe: `snake_case` and `a * b` have ASCII neighbours, so the rule never
//! fires on them. Do not widen it to ASCII — the ambiguity it avoids is real
//! there.

/// Characters a bridgeable gap may contain besides whitespace: inline style
/// markers, which sit *inside* a term. Block markers (`#`, `|`, `-`, `>`) are
/// deliberately absent — they mark a heading, cell, item, or quote boundary, so
/// the terms they separate are genuinely separate. `_` is absent too: between
/// CJK characters `전역_지침` reads as an identifier far more often than
/// `__전역__` reads as emphasis.
fn is_style_marker(c: char) -> bool {
    matches!(c, '*' | '~' | '`')
}

/// Whether `c` is Hangul, kana, or a CJK ideograph — a script written without
/// obligatory word spacing, where a gap inside a term carries no meaning.
fn is_cjk(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x11FF        // Hangul Jamo
        | 0x3040..=0x30FF      // Hiragana, Katakana
        | 0x3130..=0x318F      // Hangul Compatibility Jamo
        | 0x31F0..=0x31FF      // Katakana Phonetic Extensions
        | 0x3400..=0x4DBF      // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0xA960..=0xA97F      // Hangul Jamo Extended-A
        | 0xAC00..=0xD7A3      // Hangul Syllables
        | 0xD7B0..=0xD7FF      // Hangul Jamo Extended-B
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0xFF66..=0xFF9F      // Halfwidth Katakana
        | 0x20000..=0x2FA1F    // CJK Unified Ideographs Extension B onwards
    )
}

/// Whether `s` holds any CJK character. A cheap gate for callers: [`fold`] only
/// ever removes a gap between two of them, so it cannot change the outcome for a
/// query without one.
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// A shadow copy of some text with CJK-internal gaps removed.
pub struct Folded {
    text: String,
    /// `(shadow offset, bytes removed before it)`, ascending — one entry per
    /// removed gap, which is far smaller than one entry per character.
    removals: Vec<(usize, usize)>,
}

impl Folded {
    /// The gap-free text to match against.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Map a byte offset in [`text`](Self::text) back to the original string.
    pub fn to_original(&self, shadow_offset: usize) -> usize {
        let i = self
            .removals
            .partition_point(|&(at, _)| at <= shadow_offset);
        shadow_offset + if i == 0 { 0 } else { self.removals[i - 1].1 }
    }
}

/// Remove every gap in `s` that sits between two CJK characters and consists
/// only of whitespace and inline style markers, without spanning a blank line.
///
/// Returns `None` when there is nothing to fold, letting the caller skip the
/// second scan entirely for the overwhelmingly common note.
pub fn fold(s: &str) -> Option<Folded> {
    let mut text = String::new();
    let mut removals: Vec<(usize, usize)> = Vec::new();
    let mut removed = 0usize;
    let mut copied = 0usize; // start of the not-yet-copied region of `s`
    let mut i = 0usize;

    while i < s.len() {
        let Some(c) = s[i..].chars().next() else {
            break;
        };
        let after = i + c.len_utf8();
        if !is_cjk(c) {
            i = after;
            continue;
        }
        match bridgeable_gap(s, after) {
            Some(gap_end) => {
                text.push_str(&s[copied..after]);
                removed += gap_end - after;
                removals.push((text.len(), removed));
                copied = gap_end;
                i = gap_end; // the character there is CJK; it may open another gap
            }
            None => i = after,
        }
    }

    if removals.is_empty() {
        return None;
    }
    text.push_str(&s[copied..]);
    Some(Folded { text, removals })
}

/// The end of the removable gap starting at `from`, or `None` if the run there
/// is empty, reaches a blank line, holds anything but whitespace and style, or
/// does not land back on a CJK character.
fn bridgeable_gap(s: &str, from: usize) -> Option<usize> {
    let mut end = from;
    let mut newlines = 0usize;
    for c in s[from..].chars() {
        if c == '\n' {
            newlines += 1;
            // A blank line ends a paragraph and is also how `***` reaches a gap;
            // either way the terms across it are separate.
            if newlines >= 2 {
                return None;
            }
        } else if !c.is_whitespace() && !is_style_marker(c) {
            break;
        }
        end += c.len_utf8();
    }
    (end > from && s[end..].chars().next().is_some_and(is_cjk)).then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(s: &str) -> Option<String> {
        fold(s).map(|f| f.text().to_string())
    }

    #[test]
    fn folds_space_and_soft_wrap_between_cjk() {
        assert_eq!(folded("전역 지침").as_deref(), Some("전역지침"));
        assert_eq!(folded("전역\n지침").as_deref(), Some("전역지침"));
        assert_eq!(folded("전역\t지침").as_deref(), Some("전역지침"));
        // U+3000 ideographic space is whitespace like any other.
        assert_eq!(folded("전역\u{3000}지침").as_deref(), Some("전역지침"));
    }

    #[test]
    fn folds_inline_style_markers() {
        assert_eq!(folded("**전역** 지침").as_deref(), Some("**전역지침"));
        assert_eq!(folded("`전역` 지침").as_deref(), Some("`전역지침"));
        assert_eq!(folded("~~전역~~지침").as_deref(), Some("~~전역지침"));
    }

    #[test]
    fn keeps_block_structure_and_sentence_boundaries() {
        for s in [
            "| 전역 | 지침 |",
            "전역\n## 지침",
            "전역\n\n지침",
            "전역\n\n***\n\n지침",
            "전역. 지침",
            "- 전역\n- 지침",
            "전역\n> 지침",
            "__전역__ 지침",
        ] {
            assert!(fold(s).is_none(), "must not fold {s:?}");
        }
    }

    #[test]
    fn never_fires_on_ascii_neighbours() {
        for s in ["hello world", "snake_case_name", "a * b", "2*3", "a`b` c"] {
            assert!(fold(s).is_none(), "must not fold {s:?}");
        }
        // A CJK character next to ASCII does not make the ASCII gap bridgeable.
        assert!(fold("전역 world").is_none());
        assert!(fold("hello 지침").is_none());
    }

    #[test]
    fn maps_offsets_back_to_the_original() {
        let s = "**전역** 지침을";
        let f = fold(s).unwrap();
        assert_eq!(f.text(), "**전역지침을");
        // "전역지침" starts after the two leading asterisks in both strings.
        let hit = f.text().find("전역지침").unwrap();
        assert_eq!(hit, 2);
        assert_eq!(f.to_original(hit), 2);
        assert_eq!(&s[f.to_original(hit)..], "전역** 지침을");
        // The character after the removed gap maps past it.
        let after_gap = f.text().find("지침").unwrap();
        assert_eq!(&s[f.to_original(after_gap)..], "지침을");
    }

    #[test]
    fn maps_offsets_across_several_gaps() {
        let s = "가 나 다";
        let f = fold(s).unwrap();
        assert_eq!(f.text(), "가나다");
        for (word, want) in [("가", 0), ("나", 4), ("다", 8)] {
            let hit = f.text().find(word).unwrap();
            assert_eq!(f.to_original(hit), want, "offset of {word}");
        }
    }
}
