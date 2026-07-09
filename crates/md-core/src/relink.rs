//! Link retargeting for moves (ADR-0022).
//!
//! Pure logic that turns a batch's move pairs into per-note body rewrites:
//! [`resolve_dest`] maps a link destination to a canonical vault-relative
//! path (note-relative resolution, root-absolute from the vault root),
//! [`MoveMap`] answers "where did this path go", and [`rewrite_body`] splices
//! new destinations into a body — both inbound links from other notes and a
//! moved note's own relative links, whose bases changed with it.

use std::ops::Range;

use crate::links::extract_links;

/// Where a batch moved each path: exact matches for files, prefix matches for
/// directories. Sources/destinations are stored without a trailing `/`.
pub struct MoveMap {
    pairs: Vec<(String, String)>,
}

impl MoveMap {
    #[must_use]
    pub fn new(moves: &[(String, String)]) -> MoveMap {
        MoveMap {
            pairs: moves
                .iter()
                .map(|(f, t)| {
                    (
                        f.trim_end_matches('/').to_string(),
                        t.trim_end_matches('/').to_string(),
                    )
                })
                .collect(),
        }
    }

    /// The batch's new location for `path`, if the batch moved it (directly or
    /// via a moved ancestor directory). Overlapping sources are rejected
    /// upstream (ADR-0009), so at most one pair can match.
    #[must_use]
    pub fn apply(&self, path: &str) -> Option<String> {
        for (from, to) in &self.pairs {
            if path == from {
                return Some(to.clone());
            }
            if let Some(rest) = path.strip_prefix(from.as_str())
                && rest.starts_with('/')
            {
                return Some(format!("{to}{rest}"));
            }
        }
        None
    }
}

/// Resolve a link destination against the note that contains it, yielding a
/// canonical vault-relative path. `None` for destinations that are not vault
/// notes: empty, fragment/query-only, scheme-bearing (`https:`, `mailto:`),
/// invalid percent-encoding, or escaping the vault root via `..`.
#[must_use]
pub fn resolve_dest(note_path: &str, dest: &str) -> Option<String> {
    if dest.is_empty() || has_scheme(dest) {
        return None;
    }
    let decoded = percent_decode(dest)?;
    let (base, rest) = match decoded.strip_prefix('/') {
        Some(rest) => ("", rest),
        None => (parent_dir(note_path), decoded.as_str()),
    };
    let mut segs: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop()?;
            }
            s => segs.push(s),
        }
    }
    if segs.is_empty() {
        return None;
    }
    Some(segs.join("/"))
}

/// The relative destination string that points at vault path `target` from
/// inside note `from_note` — the inverse of [`resolve_dest`], before encoding.
#[must_use]
pub fn relative_dest(from_note: &str, target: &str) -> String {
    let from: Vec<&str> = parent_dir(from_note)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let tgt: Vec<&str> = target.split('/').collect();
    // Common prefix against the target's *directory* part only, so a file
    // never pairs off against a directory segment of the same name.
    let common = from
        .iter()
        .zip(&tgt[..tgt.len() - 1])
        .take_while(|(a, b)| a == b)
        .count();
    let mut out: Vec<&str> = vec![".."; from.len() - common];
    out.extend(&tgt[common..]);
    out.join("/")
}

/// Rewrite `body`'s links for a batch: the note itself moves `old_path` →
/// `new_path` (equal when unmoved), targets move per `map`. Returns the new
/// body only if something changed. A destination's original form survives:
/// root-absolute stays absolute, `<angled>` stays raw, others are
/// percent-encoded where required; `#fragment`/`?query` tails are untouched
/// (they sit outside the spliced range).
#[must_use]
pub fn rewrite_body(body: &str, old_path: &str, new_path: &str, map: &MoveMap) -> Option<String> {
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for occ in extract_links(body) {
        let Some(resolved) = resolve_dest(old_path, &occ.dest) else {
            continue;
        };
        let moved_target = map.apply(&resolved);
        // Rewrite only when the target moved or the note itself did — never
        // churn an equivalent spelling (./a.md) of an untouched link.
        if moved_target.is_none() && old_path == new_path {
            continue;
        }
        let target = moved_target.unwrap_or(resolved);
        let plain = if occ.dest.starts_with('/') {
            format!("/{target}")
        } else {
            relative_dest(new_path, &target)
        };
        let new_dest = if occ.angled {
            plain
        } else {
            percent_encode(&plain)
        };
        if new_dest != occ.dest {
            edits.push((occ.dest_span, new_dest));
        }
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
    let mut out = body.to_string();
    for (range, dest) in edits {
        out.replace_range(range, &dest);
    }
    Some(out)
}

/// `scheme:` per CommonMark: a letter then letters/digits/`+.-`, then `:`.
fn has_scheme(dest: &str) -> bool {
    let b = dest.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_alphabetic) {
        return false;
    }
    for &c in &b[1..] {
        match c {
            b':' => return true,
            c if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-') => {}
            _ => return false,
        }
    }
    false
}

/// Decode `%XX` escapes; a `%` not followed by two hex digits stays literal.
/// `None` if the decoded bytes are not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (
                b.get(i + 1).and_then(|c| (*c as char).to_digit(16)),
                b.get(i + 2).and_then(|c| (*c as char).to_digit(16)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Encode the characters that would break a bare (un-angled) destination:
/// whitespace/control, `%`, `#`, `?`, parens, and angle brackets. Everything
/// else — including non-ASCII — passes through raw.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            ' ' | '%' | '#' | '?' | '(' | ')' | '<' | '>' => {
                out.push_str(&format!("%{:02X}", ch as u8));
            }
            c if c.is_ascii_control() => out.push_str(&format!("%{:02X}", c as u8)),
            c => out.push(c),
        }
    }
    out
}

fn parent_dir(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- resolve_dest ----------------------------------------------------

    #[test]
    fn resolves_relative_to_the_note_directory() {
        assert_eq!(resolve_dest("a/b.md", "c.md").unwrap(), "a/c.md");
        assert_eq!(resolve_dest("a/b.md", "../c.md").unwrap(), "c.md");
        assert_eq!(resolve_dest("a/b.md", "./d/c.md").unwrap(), "a/d/c.md");
        assert_eq!(resolve_dest("top.md", "d/c.md").unwrap(), "d/c.md");
    }

    #[test]
    fn resolves_root_absolute_from_vault_root() {
        assert_eq!(resolve_dest("a/b.md", "/x/y.md").unwrap(), "x/y.md");
    }

    #[test]
    fn rejects_non_note_destinations() {
        assert_eq!(resolve_dest("a.md", ""), None);
        assert_eq!(resolve_dest("a.md", "https://e.com/x"), None);
        assert_eq!(resolve_dest("a.md", "mailto:x@y.z"), None);
        assert_eq!(resolve_dest("a.md", "../escape.md"), None);
        assert_eq!(resolve_dest("a/b.md", "../../escape.md"), None);
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(
            resolve_dest("d/n.md", "my%20note.md").unwrap(),
            "d/my note.md"
        );
        // A stray % stays literal rather than failing the whole link.
        assert_eq!(resolve_dest("d/n.md", "100%.md").unwrap(), "d/100%.md");
    }

    // --- relative_dest ---------------------------------------------------

    #[test]
    fn relative_dest_inverts_resolution() {
        assert_eq!(relative_dest("a/b.md", "a/c.md"), "c.md");
        assert_eq!(relative_dest("a/b.md", "c.md"), "../c.md");
        assert_eq!(relative_dest("a/b.md", "x/y/z.md"), "../x/y/z.md");
        assert_eq!(relative_dest("top.md", "d/c.md"), "d/c.md");
        // A file must not pair off against a same-named directory segment.
        assert_eq!(relative_dest("a/b.md", "a/b.md/c.md"), "b.md/c.md");
    }

    // --- MoveMap -----------------------------------------------------------

    #[test]
    fn move_map_matches_files_and_directory_prefixes() {
        let map = MoveMap::new(&[
            ("old/a.md".into(), "new/a.md".into()),
            ("dir/".into(), "moved/dir/".into()),
        ]);
        assert_eq!(map.apply("old/a.md").unwrap(), "new/a.md");
        assert_eq!(map.apply("dir/deep/x.md").unwrap(), "moved/dir/deep/x.md");
        assert_eq!(map.apply("other.md"), None);
        assert_eq!(map.apply("dirx/x.md"), None, "prefix must respect '/'");
    }

    // --- rewrite_body ------------------------------------------------------

    fn map(pairs: &[(&str, &str)]) -> MoveMap {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect();
        MoveMap::new(&owned)
    }

    #[test]
    fn inbound_link_repointed_others_untouched() {
        let body = "see [a](target.md) and [b](other.md#top)";
        let m = map(&[("target.md", "sub/target.md")]);
        assert_eq!(
            rewrite_body(body, "note.md", "note.md", &m).unwrap(),
            "see [a](sub/target.md) and [b](other.md#top)"
        );
    }

    #[test]
    fn outbound_links_recomputed_when_the_note_moves() {
        let body = "[s](sib.md) [d](dir/x.md)";
        let m = map(&[("note.md", "deep/note.md")]);
        assert_eq!(
            rewrite_body(body, "note.md", "deep/note.md", &m).unwrap(),
            "[s](../sib.md) [d](../dir/x.md)"
        );
    }

    #[test]
    fn both_note_and_target_moved() {
        let body = "[b](b.md)";
        let m = map(&[("a.md", "x/a.md"), ("b.md", "y/b.md")]);
        assert_eq!(
            rewrite_body(body, "a.md", "x/a.md", &m).unwrap(),
            "[b](../y/b.md)"
        );
    }

    #[test]
    fn link_into_a_moved_directory() {
        let body = "[deep](dir/sub/x.md)";
        let m = map(&[("dir/", "archive/dir/")]);
        assert_eq!(
            rewrite_body(body, "note.md", "note.md", &m).unwrap(),
            "[deep](archive/dir/sub/x.md)"
        );
    }

    #[test]
    fn root_absolute_stays_absolute() {
        let body = "[a](/target.md)";
        let m = map(&[("target.md", "sub/target.md")]);
        assert_eq!(
            rewrite_body(body, "n.md", "n.md", &m).unwrap(),
            "[a](/sub/target.md)"
        );
    }

    #[test]
    fn fragment_survives_a_rewrite() {
        let body = "[a](target.md#sec?x=1)";
        let m = map(&[("target.md", "sub/target.md")]);
        assert_eq!(
            rewrite_body(body, "n.md", "n.md", &m).unwrap(),
            "[a](sub/target.md#sec?x=1)"
        );
    }

    #[test]
    fn spaces_encode_bare_but_stay_raw_in_angles() {
        let body = "[a](<my note.md>) [b](my%20note.md)";
        let m = map(&[("my note.md", "sub dir/my note.md")]);
        assert_eq!(
            rewrite_body(body, "n.md", "n.md", &m).unwrap(),
            "[a](<sub dir/my note.md>) [b](sub%20dir/my%20note.md)"
        );
    }

    #[test]
    fn korean_paths_pass_through_raw() {
        let body = "[메모](노트/메모.md)";
        let m = map(&[("노트/", "보관/노트/")]);
        assert_eq!(
            rewrite_body(body, "n.md", "n.md", &m).unwrap(),
            "[메모](보관/노트/메모.md)"
        );
    }

    #[test]
    fn untouched_notes_and_spellings_do_not_churn() {
        let m = map(&[("moved.md", "sub/moved.md")]);
        // No link involves the move; equivalent spellings stay verbatim.
        assert_eq!(
            rewrite_body("[a](./odd.md) [b](x/../y.md)", "n.md", "n.md", &m),
            None
        );
        // Scheme and wikilinks never rewrite.
        assert_eq!(
            rewrite_body("[x](https://e.com) [[moved]]", "n.md", "n.md", &m),
            None
        );
    }

    #[test]
    fn refdef_rewritten_too() {
        let body = "[r]: target.md \"t\"\n\nuse [x][r]\n";
        let m = map(&[("target.md", "sub/target.md")]);
        assert_eq!(
            rewrite_body(body, "n.md", "n.md", &m).unwrap(),
            "[r]: sub/target.md \"t\"\n\nuse [x][r]\n"
        );
    }

    #[test]
    fn code_regions_never_rewrite() {
        let body = "`[a](target.md)`\n\n```\n[b](target.md)\n```\n";
        let m = map(&[("target.md", "sub/target.md")]);
        assert_eq!(rewrite_body(body, "n.md", "n.md", &m), None);
    }
}
