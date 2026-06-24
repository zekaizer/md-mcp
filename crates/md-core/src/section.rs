//! Section scope, content extraction, and per-section content hashing.
//!
//! The same [`Document::content_span`] feeds both reading a section's content
//! and computing its [`Document::content_hash`], so a hash produced by a read
//! always matches what an edit recomputes
//! ([ADR-0005](../../../docs/adr/0005-content-hash.md)). The target heading line
//! is always excluded from the span.

use crate::document::Document;
use crate::text::{Span, normalize_newlines};

/// Which span of a section a read or edit addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scope {
    /// Lead body only — subsections excluded.
    Body,
    /// Lead body plus all subsections — the whole subtree.
    #[default]
    Section,
}

impl Document {
    /// The content span of a target at `scope`, excluding the target heading
    /// line. `index = None` addresses the root (empty heading path).
    #[must_use]
    pub fn content_span(&self, index: Option<usize>, scope: Scope) -> Span {
        match (index, scope) {
            (None, Scope::Body) => self.preamble_span(),
            (None, Scope::Section) => self.whole_body_span(),
            (Some(i), Scope::Body) => self.own_body_span(i),
            (Some(i), Scope::Section) => {
                let sub = self.section_span(i);
                Span::new(self.headings[i].span.end, sub.end)
            }
        }
    }

    /// The content text of a section at `scope` (heading line excluded).
    #[must_use]
    pub fn section_content<'s>(
        &self,
        source: &'s str,
        index: Option<usize>,
        scope: Scope,
    ) -> &'s str {
        self.content_span(index, scope).of(source)
    }

    /// The `content_hash` of a section at `scope`: blake3 of its LF-normalized
    /// content bytes, as 64-char lowercase hex.
    #[must_use]
    pub fn content_hash(&self, source: &str, index: Option<usize>, scope: Scope) -> String {
        let span = self.content_span(index, scope);
        let bytes = normalize_newlines(span.of(source));
        blake3::hash(bytes.as_bytes()).to_hex().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(src: &str, path: &[&str], scope: Scope) -> String {
        let doc = Document::parse(src);
        let idx = if path.is_empty() {
            None
        } else {
            let owned: Vec<String> = path.iter().map(|s| (*s).to_string()).collect();
            Some(doc.resolve_heading(&owned, None).unwrap())
        };
        doc.content_hash(src, idx, scope)
    }

    #[test]
    fn hash_is_64_char_hex() {
        let h = hash_of("# A\nbody\n", &["A"], Scope::Section);
        assert_eq!(h.len(), 64);
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn body_and_section_scope_differ_when_subsections_present() {
        let src = "# A\nlead\n## B\nsub\n";
        assert_ne!(
            hash_of(src, &["A"], Scope::Body),
            hash_of(src, &["A"], Scope::Section)
        );
    }

    #[test]
    fn heading_line_is_excluded_from_the_hash() {
        // Renaming the heading must not change the body's hash.
        let a = hash_of("# A\nbody\n", &["A"], Scope::Section);
        let b = hash_of("# RENAMED\nbody\n", &["RENAMED"], Scope::Section);
        assert_eq!(a, b);
    }

    #[test]
    fn hash_is_lf_invariant() {
        let lf = hash_of("# A\nbody line\n", &["A"], Scope::Section);
        let crlf = hash_of("# A\r\nbody line\r\n", &["A"], Scope::Section);
        assert_eq!(lf, crlf);
    }

    #[test]
    fn root_body_vs_section_differ() {
        let src = "intro\n# A\nsub\n";
        assert_ne!(
            hash_of(src, &[], Scope::Body),
            hash_of(src, &[], Scope::Section)
        );
    }

    #[test]
    fn section_content_excludes_heading_line() {
        let src = "# A\nlead\n## B\nsub\n";
        let doc = Document::parse(src);
        let i = doc.resolve_heading(&["A".into()], None).unwrap();
        assert_eq!(
            doc.section_content(src, Some(i), Scope::Section),
            "lead\n## B\nsub\n"
        );
        assert_eq!(doc.section_content(src, Some(i), Scope::Body), "lead\n");
    }

    #[test]
    fn read_then_hash_matches_independent_hash() {
        // The content a read returns hashes to the same value the edit recomputes.
        let src = "# A\nlead\n## B\nsub\n";
        let doc = Document::parse(src);
        let i = doc.resolve_heading(&["A".into()], None).unwrap();
        let read = doc
            .section_content(src, Some(i), Scope::Section)
            .to_string();
        let standalone = blake3::hash(read.as_bytes()).to_hex().to_string();
        assert_eq!(doc.content_hash(src, Some(i), Scope::Section), standalone);
    }
}
