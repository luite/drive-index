//! Scan pipeline: walker → staging DB → aggregates → merge into the catalog.

pub mod content;
pub mod controller;
pub mod device;
pub mod merge;
pub mod run;
pub mod staging;

pub use run::{FsTarget, Phase, PhaseCell, ResumeMode, ScanOutcome, ScanRequest, ScanStatus, run_dir_scan, run_fs_scan};
pub use staging::Totals;
