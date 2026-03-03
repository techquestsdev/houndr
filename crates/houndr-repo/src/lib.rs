//! Git repository management, configuration, and indexing pipeline.
//!
//! Handles cloning, fetching, incremental manifest diffing, and trigram
//! index construction for configured repositories.

#![warn(missing_docs)]

/// TOML configuration parsing and validation.
pub mod config;
/// End-to-end indexing pipeline: fetch, diff, build, write.
pub mod pipeline;
/// Git operations: clone, fetch, tree walk, blob read.
pub mod vcs;
/// Polling watcher that periodically re-indexes all repos.
pub mod watcher;

pub use config::Config;
