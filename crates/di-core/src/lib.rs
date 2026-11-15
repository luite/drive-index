//! The drive-index catalog: SQLite schema, bookkeeping and the search language.

pub mod catalog;
pub mod drives;
pub mod query;
pub mod search;
pub mod sessions;
mod seed;

pub use catalog::{Catalog, DirSource, FsUsage, NewScan, SourceIds, now_secs};
