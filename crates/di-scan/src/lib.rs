//! Scan pipeline: walker → staging DB → aggregates → merge into the catalog.

pub mod merge;
pub mod run;
pub mod staging;

pub use run::{Phase, PhaseCell, ResumeMode, ScanOutcome, ScanRequest, ScanStatus, run_dir_scan};
pub use staging::Totals;
