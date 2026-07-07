//! Typed errors for the engine. The shells (CLI/MCP) add user-facing context;
//! the engine itself never panics on a production path.

/// Convenience alias used across the engine.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors surfaced by the engine's public API.
///
/// `#[non_exhaustive]`: variants will grow with the engine (M1–M3) without a
/// breaking change for downstream matchers.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Underlying file I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A page checksum did not validate — silent corruption was detected
    /// (format guarantee G1). This is never skipped or masked.
    #[error("corrupt page {page_no}: checksum mismatch")]
    CorruptPage {
        /// Page number within the `.mind` file.
        page_no: u64,
    },

    /// The file's header is not a valid `.mind` header.
    #[error("not an EmbedMind file (bad magic or malformed header)")]
    BadHeader,

    /// The file declares a `format_version` newer than this build understands
    /// (format guarantee G4: refuse clearly, never guess).
    #[error(
        "unsupported format version {found} (this build reads up to {supported}); run `embedmind migrate` with a newer build"
    )]
    UnsupportedVersion {
        /// Version found in the file header.
        found: u32,
        /// Highest version this build can read.
        supported: u32,
    },

    /// The `encrypted` header flag is set. Encryption is a premium module and
    /// is not supported by this build (see docs/adr/0007).
    #[error("file is encrypted; this build does not support encrypted files")]
    Encrypted,

    /// Another process holds the write lock on this file.
    #[error("another process is writing to this file (single-writer; see docs/adr/0006)")]
    WriteLocked,

    /// A page number outside the file was requested. Engine bug or corrupt
    /// pointer — surfaced as a typed error, never a panic.
    #[error("page {page_no} is out of bounds (page_count {page_count})")]
    PageOutOfBounds {
        /// Requested page number.
        page_no: u64,
        /// Current total page count.
        page_count: u64,
    },

    /// An API precondition was violated by the caller (wrong buffer size,
    /// unsupported page size, …). The engine never panics on misuse.
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),
}
