//! Shared structured-output types: the error shape every tool reports.

use rmcp::ErrorData;
use rmcp::schemars::JsonSchema;
use serde::Serialize;

/// The maximum number of items any batch tool accepts (server-enforced).
pub const MAX_BATCH: usize = 100;

/// Reject an over-sized batch as a protocol-level invalid-params error (a
/// schema-class violation, not a business rejection — [tool_spec §표기 규약]).
pub fn batch_limit(n: usize) -> Result<(), ErrorData> {
    if n > MAX_BATCH {
        Err(ErrorData::invalid_params(
            format!("batch of {n} exceeds the limit of {MAX_BATCH} items"),
            None,
        ))
    } else {
        Ok(())
    }
}

/// Reject an out-of-range `limit` as invalid params instead of silently
/// clamping — the inputSchema's declared bounds are otherwise decorative
/// (the framework does not enforce numeric min/max).
pub fn limit_bounds(limit: usize, max: usize) -> Result<(), ErrorData> {
    if limit == 0 || limit > max {
        Err(ErrorData::invalid_params(
            format!("limit must be between 1 and {max}, got {limit}"),
            None,
        ))
    } else {
        Ok(())
    }
}

/// Total content-byte budget for one content-bearing read response
/// ([tool_spec §4 토큰 비용 통제]). Items past the budget are dropped whole and
/// reported by index in `omitted` — content is never truncated mid-item; a big
/// note is read via read_outlines → read_sections instead.
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// Enforce [`MAX_CONTENT_BYTES`] over a response's items, in request order.
///
/// An item whose content would push the running total past the budget is
/// dropped from `items` and its request index recorded; later, smaller items
/// may still fit. Zero-content items (missing notes, flags off) are always
/// kept. Returns the omitted request indexes.
pub fn enforce_content_budget<T>(
    items: &mut Vec<T>,
    content_len: impl Fn(&T) -> usize,
) -> Vec<usize> {
    let mut total = 0usize;
    let mut omitted = Vec::new();
    let mut kept = Vec::with_capacity(items.len());
    for (i, item) in items.drain(..).enumerate() {
        let len = content_len(&item);
        if len > 0 && total + len > MAX_CONTENT_BYTES {
            omitted.push(i);
        } else {
            total += len;
            kept.push(item);
        }
    }
    *items = kept;
    omitted
}

/// Maximum byte size of a single note write ([tool_spec §4]: "item당 content
/// 크기에 서버 상한을 둬 단일 거대 입력을 막는다"). Bounds one create/append/edit
/// payload so a runaway or hostile client cannot fill the vault; comfortably
/// above any real note, well below a denial-of-service payload.
pub const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

/// Reject a write payload that exceeds [`MAX_WRITE_BYTES`]. `what` names the
/// field for the message (e.g. "content", "note"). Returned as a core-style
/// error so it flows into the tool envelope like any other rejection.
pub fn write_size_error(what: &str, len: usize) -> md_core::Error {
    md_core::Error::new(
        md_core::Code::TooLarge,
        format!("{what} is {len} bytes, over the {MAX_WRITE_BYTES}-byte limit for a single write"),
    )
}

/// Test-only invariant behind the condensed wire format: every `required`
/// property of a response schema must survive serialization of the most
/// condensed value (all skippable fields empty/false) — otherwise clients
/// validating structuredContent against outputSchema reject the response.
#[cfg(test)]
pub(crate) fn assert_condensed_satisfies_schema(schema: impl Serialize, condensed: impl Serialize) {
    let schema = serde_json::to_value(schema).unwrap();
    let value = serde_json::to_value(condensed).unwrap();
    for key in schema["required"].as_array().into_iter().flatten() {
        let key = key.as_str().unwrap();
        assert!(
            value.get(key).is_some(),
            "schema marks `{key}` required but the condensed response omits it \
             (pair skip_serializing_if with #[serde(default)])"
        );
    }
}

/// A machine-readable error embedded in a tool response (per-item or per-batch).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ApiError {
    /// Stable machine code, e.g. `NOT_FOUND`, `TRAVERSAL`, `HASH_MISMATCH`.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// The batch index this error refers to, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

impl ApiError {
    /// Build from a core error, without an index.
    #[must_use]
    pub fn from_core(e: &md_core::Error) -> Self {
        Self {
            code: e.code.as_str().to_string(),
            message: e.message.clone(),
            index: None,
        }
    }

    /// Build from a core error at a batch index.
    #[must_use]
    pub fn at(index: usize, e: &md_core::Error) -> Self {
        Self {
            code: e.code.as_str().to_string(),
            message: e.message.clone(),
            index: Some(index),
        }
    }
}
