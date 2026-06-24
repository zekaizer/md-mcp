//! Shared structured-output types: the error shape every tool reports.

use rmcp::schemars::JsonSchema;
use serde::Serialize;

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
