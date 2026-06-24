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
