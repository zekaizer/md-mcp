//! Frontmatter: typed parse (read) and byte-preserving splice (write).
//!
//! Reads deserialize the `---`…`---` block into a [`serde_json::Value`] with full
//! type fidelity; writes copy every byte outside the edited key verbatim and emit
//! only the changed field with a fixed serializer
//! ([ADR-0004](../../../docs/adr/0004-frontmatter-parse-and-typed-splice.md)).

use serde_json::Value;

use crate::document::Document;
use crate::error::{Code, Error, Result};
use crate::text::{Span, nfc, normalize_newlines};

fn de_options() -> serde_saphyr::Options {
    serde_saphyr::options! { strict_booleans: true }
}

/// Parse a note's frontmatter into a JSON value.
///
/// Returns `Ok(None)` if the note has no frontmatter block, `Ok(Some(value))`
/// for a parsed block (an empty block is an empty object), or
/// `Err(FrontmatterParse)` for an unparseable block.
pub fn parse(source: &str) -> Result<Option<Value>> {
    let normalized = normalize_newlines(source);
    let source = normalized.as_ref();
    let Some(span) = Document::parse(source).frontmatter else {
        return Ok(None);
    };
    let inner = inner_text(source, span);
    if inner.trim().is_empty() {
        return Ok(Some(Value::Object(serde_json::Map::new())));
    }
    let value = serde_saphyr::from_str_with_options::<Value>(inner, de_options())
        .map_err(|e| parse_error(&e))?;
    Ok(Some(value))
}

/// Build a note from a frontmatter value and a body. An empty/null frontmatter
/// yields just the body; otherwise a `---`…`---` block is prepended.
pub fn with_frontmatter(body: &str, fm: &Value) -> Result<String> {
    let is_empty = fm.is_null() || fm.as_object().is_some_and(serde_json::Map::is_empty);
    if is_empty {
        return Ok(body.to_string());
    }
    let mut yaml = serde_saphyr::to_string(fm)
        .map_err(|e| Error::io(format!("serialize frontmatter: {e}")))?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(format!("---\n{yaml}---\n{body}"))
}

/// Whether the frontmatter has a top-level `key` (NFC-compared).
pub fn has_property(source: &str, key: &str) -> Result<bool> {
    Ok(parse(source)?
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|m| m.keys().any(|k| nfc(k) == nfc(key))))
}

/// Set a top-level frontmatter `key` to `value`, preserving all other bytes.
/// Creates the frontmatter block if the note has none.
pub fn set_property(source: &str, key: &str, value: &Value) -> Result<String> {
    let normalized = normalize_newlines(source);
    let source = normalized.as_ref();
    let emitted = emit_field(key, value)?;

    let result = match Document::parse(source).frontmatter {
        None => format!("---\n{}\n---\n{}", emitted.join("\n"), source),
        Some(span) => {
            let inner = inner_text(source, span);
            validate_block(inner)?;
            let mut lines = split_inner(inner);
            replace_or_append(&mut lines, key, &emitted);
            rebuild(source, span, &lines)
        }
    };

    verify_set(&result, key, value)?;
    Ok(result)
}

/// Remove a top-level frontmatter `key`. A no-op if the key (or frontmatter) is
/// absent; removing the last key drops the now-empty block.
pub fn remove_property(source: &str, key: &str) -> Result<String> {
    let normalized = normalize_newlines(source);
    let source = normalized.as_ref();
    let Some(span) = Document::parse(source).frontmatter else {
        return Ok(source.to_string());
    };
    let inner = inner_text(source, span);
    validate_block(inner)?;
    let mut lines = split_inner(inner);
    if !remove_key(&mut lines, key) {
        return Ok(source.to_string());
    }
    let result = if lines.iter().all(|l| l.trim().is_empty()) {
        source[span.end..].to_string()
    } else {
        rebuild(source, span, &lines)
    };
    verify_remove(source, &result, key)?;
    Ok(result)
}

// --- internals --------------------------------------------------------------

/// The YAML text between the frontmatter delimiters (without the `---` lines).
fn inner_text(source: &str, span: Span) -> &str {
    let block = span.of(source);
    let after_open = block.find('\n').map_or(block.len(), |i| i + 1);
    let trimmed = block.trim_end_matches('\n');
    let close_start = trimmed.rfind('\n').map_or(after_open, |i| i + 1);
    if after_open <= close_start {
        &block[after_open..close_start]
    } else {
        ""
    }
}

/// Reject a block that does not parse as YAML (never write over broken YAML).
fn validate_block(inner: &str) -> Result<()> {
    if inner.trim().is_empty() {
        return Ok(());
    }
    serde_saphyr::from_str_with_options::<Value>(inner, de_options())
        .map(|_: Value| ())
        .map_err(|e| parse_error(&e))
}

/// Build a `FrontmatterParse` error, stripping the deserializer's Rust-facing
/// remediation hints (e.g. "set DuplicateKeyPolicy in Options if acceptable")
/// that mean nothing to a tool caller.
fn parse_error(e: &impl std::fmt::Display) -> Error {
    let msg = e
        .to_string()
        .replace(", set DuplicateKeyPolicy in Options if acceptable", "");
    Error::new(
        Code::FrontmatterParse,
        format!("invalid frontmatter YAML: {msg}"),
    )
}

#[cfg(test)]
mod parse_error_tests {
    use super::*;

    #[test]
    fn bom_prefixed_frontmatter_is_recognized() {
        // A BOM from a Windows/macOS editor must not turn the frontmatter
        // block into body text.
        let fm = parse("\u{feff}---\ntitle: bom\n---\nbody\n").unwrap().unwrap();
        assert_eq!(fm["title"], serde_json::json!("bom"));
    }

    #[test]
    fn duplicate_key_message_drops_library_hint() {
        let e = parse("---\nk: 1\nk: 2\n---\nbody\n").unwrap_err();
        assert_eq!(e.code, Code::FrontmatterParse);
        assert!(
            !e.message.contains("DuplicateKeyPolicy"),
            "message leaks library internals: {}",
            e.message
        );
        assert!(e.message.contains("duplicate mapping key"));
    }
}

fn split_inner(inner: &str) -> Vec<String> {
    if inner.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = inner.split('\n').map(String::from).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn rebuild(source: &str, span: Span, lines: &[String]) -> String {
    let inner = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    format!(
        "{}---\n{inner}---\n{}",
        &source[..span.start],
        &source[span.end..]
    )
}

/// Serialize the single field `{key: value}` to YAML lines (no trailing blank).
fn emit_field(key: &str, value: &Value) -> Result<Vec<String>> {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), value.clone());
    let yaml = serde_saphyr::to_string(&Value::Object(map))
        .map_err(|e| Error::io(format!("serialize frontmatter field {key}: {e}")))?;
    Ok(yaml
        .trim_end_matches('\n')
        .split('\n')
        .map(String::from)
        .collect())
}

/// Whether `line` is a continuation of the preceding key's value: indented, or a
/// flush-left block sequence item (`- ...`). A blank line or a new key is not.
fn is_continuation(line: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }
    if line.starts_with([' ', '\t']) {
        return true;
    }
    let t = line.trim_start();
    t == "-" || t.starts_with("- ")
}

/// Whether `line` is the top-level key `key` (NFC-compared, quotes stripped).
fn is_top_level_key(line: &str, key: &str) -> bool {
    if line.starts_with([' ', '\t']) || line.starts_with('-') {
        return false;
    }
    // A quoted key may itself contain a colon, so find the colon after the
    // closing quote rather than the first colon on the line.
    if let Some(quote) = line.chars().next().filter(|c| *c == '"' || *c == '\'') {
        if let Some(close) = line[1..].find(quote) {
            let token = &line[1..=close];
            let after = line[close + 2..].trim_start();
            return after.starts_with(':') && nfc(token) == nfc(key);
        }
        return false;
    }
    let Some(colon) = line.find(':') else {
        return false;
    };
    nfc(line[..colon].trim()) == nfc(key)
}

/// The `[start, end)` line range a top-level key occupies (its line + value
/// continuations), tolerating blank lines *inside* a block value (a block list or
/// block scalar) while still treating a blank line before the next key as the
/// terminator.
fn key_extent(lines: &[String], key: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| is_top_level_key(l, key))?;
    let mut end = start + 1;
    while end < lines.len() {
        if is_continuation(&lines[end]) {
            end += 1;
        } else if lines[end].trim().is_empty() && next_nonblank_is_continuation(&lines[end + 1..]) {
            end += 1; // interior blank line within the value
        } else {
            break; // a new top-level key, or a trailing blank before one
        }
    }
    // Trailing blank lines are a separator, not part of the value.
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some((start, end))
}

/// Whether the first non-blank line is a value continuation (vs a new key).
fn next_nonblank_is_continuation(lines: &[String]) -> bool {
    lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| is_continuation(l))
}

fn replace_or_append(lines: &mut Vec<String>, key: &str, emitted: &[String]) {
    if let Some((start, end)) = key_extent(lines, key) {
        lines.splice(start..end, emitted.iter().cloned());
    } else {
        lines.extend(emitted.iter().cloned());
    }
}

fn remove_key(lines: &mut Vec<String>, key: &str) -> bool {
    if let Some((start, end)) = key_extent(lines, key) {
        lines.drain(start..end);
        true
    } else {
        false
    }
}

/// After a set, confirm the key round-trips to the intended value.
fn verify_set(result: &str, key: &str, value: &Value) -> Result<()> {
    let got = parse(result)?
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|m| m.get(key))
        .cloned();
    if got.as_ref() == Some(value) {
        Ok(())
    } else {
        Err(Error::io(format!(
            "frontmatter write invariant failed for key {key}"
        )))
    }
}

/// After a remove, confirm the key is gone and every other key round-trips.
fn verify_remove(original: &str, result: &str, key: &str) -> Result<()> {
    let mut expected = parse(original)?
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    expected.retain(|k, _| nfc(k) != nfc(key));
    let after = parse(result)?
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if after == expected {
        Ok(())
    } else {
        Err(Error::io(format!(
            "frontmatter remove invariant failed for key {key}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- parse --------------------------------------------------------------

    #[test]
    fn parse_preserves_scalar_types() {
        let src = "---\ns: \"123\"\nn: 123\nf: 1.5\nb: true\nz: null\n---\nbody\n";
        let v = parse(src).unwrap().unwrap();
        assert_eq!(v["s"], json!("123"));
        assert_eq!(v["n"], json!(123));
        assert_eq!(v["f"], json!(1.5));
        assert_eq!(v["b"], json!(true));
        assert_eq!(v["z"], Value::Null);
    }

    #[test]
    fn parse_norway_problem_stays_string() {
        let v = parse("---\ncountry: no\n---\n").unwrap().unwrap();
        assert_eq!(v["country"], json!("no"));
    }

    #[test]
    fn parse_lists_and_maps() {
        let src = "---\ntags:\n  - a\n  - b\nmeta:\n  x: 1\n---\n";
        let v = parse(src).unwrap().unwrap();
        assert_eq!(v["tags"], json!(["a", "b"]));
        assert_eq!(v["meta"], json!({ "x": 1 }));
    }

    #[test]
    fn parse_none_and_empty_and_broken() {
        assert_eq!(parse("# no frontmatter\n").unwrap(), None);
        assert_eq!(parse("---\n---\nbody\n").unwrap(), Some(json!({})));
        assert_eq!(
            parse("---\nkey: : :\n bad\n---\n").unwrap_err().code,
            Code::FrontmatterParse
        );
    }

    #[test]
    fn unterminated_block_is_not_frontmatter() {
        assert_eq!(parse("---\nkey: v\nbody with no close\n").unwrap(), None);
    }

    // --- set ----------------------------------------------------------------

    #[test]
    fn set_creates_block_when_absent() {
        let out = set_property("# Title\n", "status", &json!("draft")).unwrap();
        assert_eq!(out, "---\nstatus: draft\n---\n# Title\n");
    }

    #[test]
    fn set_new_key_inserts_before_close_and_preserves_body() {
        let src = "---\ntitle: x\n---\n# Body\ntext\n";
        let out = set_property(src, "status", &json!("draft")).unwrap();
        assert_eq!(out, "---\ntitle: x\nstatus: draft\n---\n# Body\ntext\n");
    }

    #[test]
    fn set_existing_key_replaces_in_place() {
        let src = "---\na: 1\ntitle: old\nb: 2\n---\nbody\n";
        let out = set_property(src, "title", &json!("new")).unwrap();
        // order preserved, neighbours and body untouched.
        assert_eq!(out, "---\na: 1\ntitle: new\nb: 2\n---\nbody\n");
    }

    #[test]
    fn set_string_that_looks_numeric_is_quoted() {
        let out = set_property("---\nx: 1\n---\n", "code", &json!("123")).unwrap();
        assert!(out.contains("code: \"123\""), "got: {out}");
        // and it round-trips as a string
        assert_eq!(parse(&out).unwrap().unwrap()["code"], json!("123"));
    }

    #[test]
    fn set_null_writes_null_not_removal() {
        let out = set_property("---\nx: 1\n---\n", "reviewed", &Value::Null).unwrap();
        assert!(out.contains("reviewed: null"), "got: {out}");
        let v = parse(&out).unwrap().unwrap();
        assert!(v.as_object().unwrap().contains_key("reviewed"));
        assert_eq!(v["reviewed"], Value::Null);
    }

    #[test]
    fn set_list_replaces_block_list() {
        let src = "---\ntags:\n  - old\nkeep: 1\n---\nbody\n";
        let out = set_property(src, "tags", &json!(["a", "b"])).unwrap();
        assert_eq!(parse(&out).unwrap().unwrap()["tags"], json!(["a", "b"]));
        assert!(out.contains("keep: 1"));
        assert!(out.ends_with("---\nbody\n"));
    }

    #[test]
    fn set_over_broken_yaml_is_rejected() {
        let src = "---\nkey: : :\n bad\n---\nbody\n";
        assert_eq!(
            set_property(src, "x", &json!(1)).unwrap_err().code,
            Code::FrontmatterParse
        );
    }

    // --- remove -------------------------------------------------------------

    #[test]
    fn remove_existing_key_preserves_others() {
        let src = "---\na: 1\ndrop: 2\nb: 3\n---\nbody\n";
        let out = remove_property(src, "drop").unwrap();
        assert_eq!(out, "---\na: 1\nb: 3\n---\nbody\n");
    }

    #[test]
    fn remove_block_list_key() {
        let src = "---\ntags:\n  - a\n  - b\nkeep: 1\n---\nbody\n";
        let out = remove_property(src, "tags").unwrap();
        assert_eq!(out, "---\nkeep: 1\n---\nbody\n");
    }

    #[test]
    fn remove_last_key_drops_block() {
        let out = remove_property("---\nonly: 1\n---\n# Body\n", "only").unwrap();
        assert_eq!(out, "# Body\n");
    }

    #[test]
    fn remove_absent_key_is_noop() {
        let src = "---\na: 1\n---\nbody\n";
        assert_eq!(remove_property(src, "nope").unwrap(), src);
    }

    // --- with_frontmatter ---------------------------------------------------

    #[test]
    fn with_frontmatter_builds_block_or_passes_body() {
        let out = with_frontmatter("# Body\n", &json!({ "title": "T", "n": 1 })).unwrap();
        assert!(out.starts_with("---\n") && out.ends_with("---\n# Body\n"));
        let v = parse(&out).unwrap().unwrap();
        assert_eq!(v["title"], json!("T"));
        assert_eq!(v["n"], json!(1));
        // empty / null frontmatter yields just the body.
        assert_eq!(
            with_frontmatter("# Body\n", &json!({})).unwrap(),
            "# Body\n"
        );
        assert_eq!(
            with_frontmatter("# Body\n", &Value::Null).unwrap(),
            "# Body\n"
        );
    }

    // --- block values spanning blank lines ----------------------------------

    #[test]
    fn remove_key_with_block_scalar_spanning_blank_line() {
        let src = "---\ndesc: |\n  line1\n\n  line2\nstatus: draft\n---\nbody\n";
        let out = remove_property(src, "desc").unwrap();
        let v = parse(&out).unwrap().unwrap();
        assert!(v.as_object().unwrap().get("desc").is_none());
        assert_eq!(v["status"], json!("draft"));
        assert!(out.ends_with("---\nbody\n"));
    }

    #[test]
    fn set_key_whose_old_value_spanned_blank_line() {
        let src = "---\ntags:\n  - a\n\n  - b\nkeep: 1\n---\nbody\n";
        let out = set_property(src, "tags", &json!(["x"])).unwrap();
        assert_eq!(parse(&out).unwrap().unwrap()["tags"], json!(["x"]));
        assert_eq!(parse(&out).unwrap().unwrap()["keep"], json!(1));
    }

    #[test]
    fn quoted_key_with_colon_is_editable() {
        let src = "---\n\"a:b\": old\nx: 1\n---\n";
        let out = set_property(src, "a:b", &json!("new")).unwrap();
        let v = parse(&out).unwrap().unwrap();
        assert_eq!(v["a:b"], json!("new"));
        assert_eq!(v["x"], json!(1));
    }

    // --- has_property -------------------------------------------------------

    #[test]
    fn has_property_reports_presence() {
        let src = "---\nstatus: draft\n---\n";
        assert!(has_property(src, "status").unwrap());
        assert!(!has_property(src, "missing").unwrap());
        assert!(!has_property("# no fm\n", "status").unwrap());
    }
}
