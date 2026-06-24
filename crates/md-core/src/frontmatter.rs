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
    let value = serde_saphyr::from_str_with_options::<Value>(inner, de_options()).map_err(|e| {
        Error::new(
            Code::FrontmatterParse,
            format!("invalid frontmatter YAML: {e}"),
        )
    })?;
    Ok(Some(value))
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
    if lines.iter().all(|l| l.trim().is_empty()) {
        return Ok(source[span.end..].to_string());
    }
    Ok(rebuild(source, span, &lines))
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
        .map_err(|e| {
            Error::new(
                Code::FrontmatterParse,
                format!("invalid frontmatter YAML: {e}"),
            )
        })
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
    let Some(colon) = line.find(':') else {
        return false;
    };
    let token = line[..colon].trim();
    let token = token
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| token.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        .unwrap_or(token);
    nfc(token) == nfc(key)
}

/// The `[start, end)` line range a top-level key occupies (its line + value
/// continuations).
fn key_extent(lines: &[String], key: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| is_top_level_key(l, key))?;
    let mut end = start + 1;
    while end < lines.len() && is_continuation(&lines[end]) {
        end += 1;
    }
    Some((start, end))
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

    // --- has_property -------------------------------------------------------

    #[test]
    fn has_property_reports_presence() {
        let src = "---\nstatus: draft\n---\n";
        assert!(has_property(src, "status").unwrap());
        assert!(!has_property(src, "missing").unwrap());
        assert!(!has_property("# no fm\n", "status").unwrap());
    }
}
