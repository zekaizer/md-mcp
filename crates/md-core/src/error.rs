//! The error model shared across the vault core.
//!
//! Core operations fail with an [`Error`] carrying a machine-readable [`Code`]
//! and a human message. The server layer maps these straight into the tool error
//! envelope `{ index, item?, operation?, code, message }`
//! ([tool_spec §출력 envelope](../../../docs/tool_spec.md)).

/// A machine-readable rejection reason. Serialized as the `code` field of the
/// tool error envelope (see [`Code::as_str`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// A note, directory, or heading path was not found.
    NotFound,
    /// A target already exists and would be overwritten without permission.
    Conflict,
    /// An `expected_hash` did not match the target section's current hash.
    HashMismatch,
    /// A heading path matched more than one heading and no `occurrence` was given.
    Ambiguous,
    /// Two batch items target the same or nested content.
    Overlap,
    /// A literal replacement's match count did not equal its `expected_count`.
    CountMismatch,
    /// Inserted/moved heading content does not fit the destination heading level.
    HeadingLevel,
    /// A path suffix convention was violated (`.md` for notes, `/` for directories).
    Suffix,
    /// A path segment is empty, `.`/`..`, or contains a separator or NUL.
    Segment,
    /// A relocate destination is not a directory.
    DestNotDir,
    /// Two batch items collide on a source or destination (e.g. a swap).
    BatchCollision,
    /// A required `content`/`new_heading`/`destination` field was missing.
    MissingContent,
    /// A `new_heading` cannot form a single valid heading line (empty or
    /// contains a line break).
    InvalidHeading,
    /// A path escaped the vault root (`..`, absolute, symlink, or the root itself).
    Traversal,
    /// A note's frontmatter is not parseable YAML.
    FrontmatterParse,
    /// A single write payload exceeds the server's per-item size limit.
    TooLarge,
    /// A note body that is not valid UTF-8 text.
    Encoding,
    /// An unexpected I/O failure.
    Io,
}

impl Code {
    /// The stable SCREAMING_SNAKE_CASE string used in the error envelope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Code::NotFound => "NOT_FOUND",
            Code::Conflict => "CONFLICT",
            Code::HashMismatch => "HASH_MISMATCH",
            Code::Ambiguous => "AMBIGUOUS",
            Code::Overlap => "OVERLAP",
            Code::CountMismatch => "COUNT_MISMATCH",
            Code::HeadingLevel => "HEADING_LEVEL",
            Code::Suffix => "SUFFIX",
            Code::Segment => "SEGMENT",
            Code::DestNotDir => "DEST_NOT_DIR",
            Code::BatchCollision => "BATCH_COLLISION",
            Code::MissingContent => "MISSING_CONTENT",
            Code::InvalidHeading => "INVALID_HEADING",
            Code::Traversal => "TRAVERSAL",
            Code::FrontmatterParse => "FRONTMATTER_PARSE",
            Code::TooLarge => "TOO_LARGE",
            Code::Encoding => "ENCODING",
            Code::Io => "IO",
        }
    }
}

/// A core error: a [`Code`] plus a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct Error {
    pub code: Code,
    pub message: String,
}

impl Error {
    /// Construct an error from a code and message.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A [`Code::Traversal`] error.
    pub fn traversal(message: impl Into<String>) -> Self {
        Self::new(Code::Traversal, message)
    }

    /// A [`Code::NotFound`] error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Code::NotFound, message)
    }

    /// A [`Code::Conflict`] error.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(Code::Conflict, message)
    }

    /// A [`Code::Io`] error.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(Code::Io, message)
    }
}

/// The core result type.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_strings_are_screaming_snake() {
        assert_eq!(Code::NotFound.as_str(), "NOT_FOUND");
        assert_eq!(Code::FrontmatterParse.as_str(), "FRONTMATTER_PARSE");
        assert_eq!(Code::Traversal.as_str(), "TRAVERSAL");
    }

    #[test]
    fn error_display_includes_code_and_message() {
        let e = Error::traversal("path escapes vault: ../x");
        assert_eq!(e.to_string(), "TRAVERSAL: path escapes vault: ../x");
        assert_eq!(e.code, Code::Traversal);
    }
}
