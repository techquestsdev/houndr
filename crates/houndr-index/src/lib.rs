//! Trigram-based code search engine library.
//!
//! Provides index building, binary serialization, memory-mapped reading, and
//! query execution with parallel candidate verification.

#![warn(missing_docs)]

/// Index building: collect documents, extract trigrams, produce a `BuiltIndex`.
pub mod builder;
/// Posting list intersection utilities.
pub mod posting;
/// Query planning and parallel search execution.
pub mod query;
/// Memory-mapped index reader with zero-copy lookups.
pub mod reader;
/// 3-byte trigram primitive and extraction.
pub mod trigram;
/// Binary index serialization to disk.
pub mod writer;

pub use builder::IndexBuilder;
pub use query::{FileMatch, LineMatch, MatchBlock, QueryPlan, SearchResult};
pub use reader::IndexReader;
pub use trigram::Trigram;
