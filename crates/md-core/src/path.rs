//! Segment-array paths at the tool boundary
//! ([ADR-0029](../../../docs/adr/0029-path-segments.md)).
//!
//! The vault addresses notes by `/`-joined relative strings; the tool contract
//! carries the same path as `["dir", "note.md"]`. These two functions are the
//! only conversion between the forms, so a separator can never be smuggled in
//! through a segment.

use crate::error::{Code, Error, Result};

/// Join validated segments into a vault-relative path; `[]` is the root `""`.
///
/// Rejects with [`Code::Segment`] a segment that is empty, `.`, `..`, or that
/// contains `/`, `\` or NUL — the message names the offending segment.
pub fn join_segments<S: AsRef<str>>(segments: &[S]) -> Result<String> {
    let mut out = String::new();
    for seg in segments {
        let seg = seg.as_ref();
        let why = if seg.is_empty() {
            Some("is empty")
        } else if seg == "." || seg == ".." {
            Some("is a relative reference")
        } else if seg.contains(['/', '\\']) {
            Some("contains a separator (a '/' in a title splits it into a folder and a note)")
        } else if seg.contains('\0') {
            Some("contains NUL")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(Error::new(
                Code::Segment,
                format!("path segment {seg:?} {why}"),
            ));
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(seg);
    }
    Ok(out)
}

/// Split a vault-relative path into segments, dropping empty ones (the root
/// `""` and a directory's trailing `/` both yield nothing).
#[must_use]
pub fn split_rel(rel: &str) -> Vec<String> {
    rel.split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_segments_with_slash_and_empty_is_root() {
        assert_eq!(join_segments(&["a", "b", "c.md"]).unwrap(), "a/b/c.md");
        assert_eq!(join_segments::<&str>(&[]).unwrap(), "");
    }

    #[test]
    fn rejects_separator_inside_a_segment_naming_it() {
        let e = join_segments(&["research", "I/O terms.md"]).unwrap_err();
        assert_eq!(e.code, Code::Segment);
        assert!(e.message.contains("I/O terms.md"), "{}", e.message);
        let e = join_segments(&["a\\b.md"]).unwrap_err();
        assert_eq!(e.code, Code::Segment);
    }

    #[test]
    fn rejects_empty_dot_dotdot_and_nul_segments() {
        for bad in [vec!["a", ""], vec!["."], vec!["a", ".."], vec!["a\0b.md"]] {
            let e = join_segments(&bad).unwrap_err();
            assert_eq!(e.code, Code::Segment, "{bad:?}");
        }
    }

    #[test]
    fn split_drops_root_and_trailing_slash() {
        assert_eq!(split_rel("a/b/c.md"), vec!["a", "b", "c.md"]);
        assert_eq!(split_rel("a/b/"), vec!["a", "b"]);
        assert!(split_rel("").is_empty());
        assert!(split_rel("/").is_empty());
    }
}
