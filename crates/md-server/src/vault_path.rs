//! The wire form of a vault path
//! ([ADR-0029](../../../docs/adr/0029-path-segments.md)): segments root to
//! leaf. The core keeps `/`-joined strings; this type is the only bridge.

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A vault path as every tool carries it: `["dir", "note.md"]`, root to leaf;
/// `[]` is the vault root. A segment holding `/` is rejected by [`Self::rel`]
/// (`SEGMENT`), never joined into a deeper path.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize, JsonSchema,
)]
#[serde(transparent)]
#[schemars(crate = "rmcp::schemars")]
pub struct VaultPath(pub Vec<String>);

impl VaultPath {
    /// The `/`-joined vault-relative path the core operates on; `""` for the
    /// root. Fails with `SEGMENT` on an invalid segment.
    pub fn rel(&self) -> md_core::Result<String> {
        md_core::join_segments(&self.0)
    }

    /// From a core path. The root `""` and a directory's trailing `/` add no
    /// segment.
    #[must_use]
    pub fn from_rel(rel: &str) -> Self {
        Self(md_core::split_rel(rel))
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for VaultPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

// Test sugar: unit tests spell paths as the `/`-joined strings the core uses,
// so a test reads like the vault it sets up. The wire form is exercised by
// the protocol-level suites in `tests/`.
#[cfg(test)]
impl From<&str> for VaultPath {
    fn from(rel: &str) -> Self {
        Self::from_rel(rel)
    }
}

#[cfg(test)]
impl From<String> for VaultPath {
    fn from(rel: String) -> Self {
        Self::from_rel(&rel)
    }
}

#[cfg(test)]
impl PartialEq<&str> for VaultPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == md_core::split_rel(other)
    }
}

#[cfg(test)]
impl PartialEq<str> for VaultPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == md_core::split_rel(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_form_is_a_bare_array() {
        let p: VaultPath = serde_json::from_str(r#"["a","b.md"]"#).unwrap();
        assert_eq!(p.rel().unwrap(), "a/b.md");
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"["a","b.md"]"#);
        assert!(serde_json::from_str::<VaultPath>(r#""a/b.md""#).is_err());
    }

    #[test]
    fn schema_is_an_array_of_strings() {
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(VaultPath)).unwrap();
        assert_eq!(schema["type"], "array", "{schema}");
        assert_eq!(schema["items"]["type"], "string", "{schema}");
    }

    #[test]
    fn a_separator_in_a_segment_is_a_segment_error() {
        let p = VaultPath(vec!["research".into(), "I/O terms.md".into()]);
        assert_eq!(p.rel().unwrap_err().code, md_core::Code::Segment);
    }
}
