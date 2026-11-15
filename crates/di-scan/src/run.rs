//! Scan orchestration: catalog bookkeeping → staging → walk → aggregates → merge.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use di_core::{Catalog, DirSource, NewScan, SourceIds};
use di_walk::{Control, DirBatch, Progress, WalkConfig, WalkOutcome, best_lister, stat_path, walk};

use di_walk::host;
use crate::content::{ContentConfig, ContentOutcome, run_content};
use crate::merge::{MergeStats, merge};
use crate::staging::{Staging, Totals, remove_db_files, run_writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// Continue an interrupted scan of the same filesystem if there is one.
    Auto,
    /// Discard any interrupted scan and start over.
    Restart,
}

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub catalog_path: PathBuf,
    pub dir: PathBuf,
    pub label: Option<String>,
    pub walk: WalkConfig,
    pub resume: ResumeMode,
    /// Content phase (quick hashes + media metadata) after the walk; None skips it.
    pub content: Option<ContentConfig>,
}

/// Coarse phase of a running scan, for progress displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Preparing = 0,
    Walking = 1,
    Aggregating = 2,
    Merging = 3,
    Done = 4,
    /// Reading file contents: quick hashes and media metadata.
    Content = 5,
}

#[derive(Debug, Default)]
pub struct PhaseCell(AtomicU8);

impl PhaseCell {
    pub fn set(&self, p: Phase) {
        self.0.store(p as u8, Ordering::SeqCst);
    }
    pub fn get(&self) -> Phase {
        match self.0.load(Ordering::SeqCst) {
            0 => Phase::Preparing,
            1 => Phase::Walking,
            2 => Phase::Aggregating,
            3 => Phase::Merging,
            5 => Phase::Content,
            _ => Phase::Done,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStatus {
    Complete,
    /// Cancelled; staging kept, resumable.
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub status: ScanStatus,
    pub scan_id: i64,
    pub drive_id: i64,
    pub filesystem_id: i64,
    pub method: &'static str,
    /// Directories already done before this run (resumed scans).
    pub resumed_dirs: Option<u64>,
    pub totals: Totals,
    pub merge: MergeStats,
    pub walk_ms: u64,
    pub merge_ms: u64,
    pub content: Option<ContentOutcome>,
}

pub fn run_dir_scan(req: &ScanRequest, control: &Control, progress: &Progress, phase: &PhaseCell) -> Result<ScanOutcome> {
    phase.set(Phase::Preparing);
    let root = req
        .dir
        .canonicalize()
        .with_context(|| format!("cannot access {}", req.dir.display()))?;
    let (kind, _) = stat_path(&root)?;
    if kind != di_walk::EntryKind::Dir {
        bail!("{} is not a directory", root.display());
    }
    let root_str = root.to_str().context("scan path is not valid UTF-8")?.to_string();
    let host = host::hostname();

    let mut catalog = Catalog::open(&req.catalog_path)?;
    let ids = catalog.upsert_dir_source(&DirSource {
        host,
        path: root_str,
        label: req.label.clone(),
        fs: host::fs_usage(&root),
    })?;
    let target = FsTarget { drive_id: ids.drive_id, filesystem_id: ids.filesystem_id, root };
    let mut out = run_fs_scan(&mut catalog, &target, &req.walk, req.resume, control, progress, phase)?;
    if let (ScanStatus::Complete, Some(cfg)) = (out.status, &req.content) {
        phase.set(Phase::Content);
        out.content = Some(run_content(&mut catalog, target.filesystem_id, &target.root, cfg, control, progress)?);
        phase.set(Phase::Done);
    }
    Ok(out)
}

/// A filesystem in the catalog and where it is mounted right now.
#[derive(Debug, Clone)]
pub struct FsTarget {
    pub drive_id: i64,
    pub filesystem_id: i64,
    /// Canonical mount point / directory to walk.
    pub root: PathBuf,
}

/// Scan one catalog filesystem at `target.root`: staging, walk, aggregates, merge.
/// Resumes an interrupted scan of the same filesystem unless `resume` is Restart.
pub fn run_fs_scan(
    catalog: &mut Catalog,
    target: &FsTarget,
    walk_cfg: &WalkConfig,
    resume: ResumeMode,
    control: &Control,
    progress: &Progress,
    phase: &PhaseCell,
) -> Result<ScanOutcome> {
    phase.set(Phase::Preparing);
    let root = &target.root;
    let (kind, root_meta) = stat_path(root)?;
    if kind != di_walk::EntryKind::Dir {
        bail!("{} is not a directory", root.display());
    }
    let root_str = root.to_str().context("scan path is not valid UTF-8")?.to_string();
    let host = host::hostname();
    let ids = SourceIds { drive_id: target.drive_id, filesystem_id: target.filesystem_id };

    let lister = best_lister();
    let staging_dir = catalog.staging_dir();
    std::fs::create_dir_all(&staging_dir)?;
    let staging_path = |id: i64| staging_dir.join(format!("{id}.db"));

    // Resume an interrupted scan if possible.
    let mut resumed = None;
    if let Some(prev) = catalog.resumable_scan(ids.filesystem_id)? {
        let path = staging_path(prev);
        if resume == ResumeMode::Auto && path.exists() {
            match Staging::open(&path) {
                Ok(st) => resumed = Some((prev, st)),
                Err(_) => {
                    catalog.set_scan_status(prev, "aborted")?;
                    remove_db_files(&path);
                }
            }
        } else {
            catalog.set_scan_status(prev, "aborted")?;
            remove_db_files(&path);
        }
    }

    let (scan_id, staging, resumed_dirs) = match resumed {
        Some((id, st)) => {
            catalog.set_scan_status(id, "running")?;
            let done = st.listed_dirs()?;
            (id, st, Some(done))
        }
        None => {
            let id = catalog.create_scan(&NewScan {
                filesystem_id: ids.filesystem_id,
                method: lister.method().to_string(),
                host,
                host_tz: host::time_zone(),
                mount_point: root_str.clone(),
                threads: walk_cfg.threads,
                options_json: Some(format!(
                    "{{\"one_file_system\":{},\"record_atime\":{},\"skip_dirs\":{}}}",
                    walk_cfg.one_file_system,
                    walk_cfg.record_atime,
                    json_str_array(&walk_cfg.skip_dir_names)
                )),
            })?;
            (id, Staging::create(&staging_path(id), id, &root_meta)?, None)
        }
    };

    let jobs = staging.pending_jobs()?;
    let next_id = staging.next_dir_id()?;
    let hardlinks = staging.hardlink_inodes()?;

    // Walk, with the writer persisting batches concurrently.
    phase.set(Phase::Walking);
    let walk_start = Instant::now();
    let (tx, rx) = crossbeam_channel::bounded::<DirBatch>(512);
    let writer = std::thread::Builder::new()
        .name("staging-writer".into())
        .spawn(move || run_writer(staging, rx, hardlinks))?;
    let outcome = walk(root, root_meta.dev, jobs, next_id, walk_cfg, lister.as_ref(), control, progress, &tx);
    drop(tx);
    let mut staging = writer.join().map_err(|_| anyhow::anyhow!("staging writer panicked"))??;
    let walk_ms = walk_start.elapsed().as_millis() as u64;
    catalog.add_scan_active_ms(scan_id, walk_ms, walk_cfg.threads)?;

    let mut result = ScanOutcome {
        status: ScanStatus::Interrupted,
        scan_id,
        drive_id: ids.drive_id,
        filesystem_id: ids.filesystem_id,
        method: lister.method(),
        resumed_dirs,
        totals: Totals::default(),
        merge: MergeStats::default(),
        walk_ms,
        merge_ms: 0,
        content: None,
    };

    if outcome == WalkOutcome::Cancelled {
        catalog.set_scan_status(scan_id, "interrupted")?;
        phase.set(Phase::Done);
        return Ok(result);
    }

    phase.set(Phase::Aggregating);
    catalog.set_scan_status(scan_id, "finalizing")?;
    let mut timer = crate::merge::StepTimer::new("finalize");
    let totals = staging.compute_aggregates()?;
    timer.step("aggregates");
    let staging_file = staging.path.clone();
    drop(staging);

    phase.set(Phase::Merging);
    let merge_start = Instant::now();
    let stats = merge(catalog, &staging_file, scan_id, ids.filesystem_id, &totals)?;
    result.merge_ms = merge_start.elapsed().as_millis() as u64;
    remove_db_files(&staging_file);

    result.status = ScanStatus::Complete;
    result.totals = totals;
    result.merge = stats;
    phase.set(Phase::Done);
    Ok(result)
}

fn json_str_array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(","))
}

/// Default catalog location for this user.
pub fn default_catalog_path(data_dir: &Path) -> PathBuf {
    data_dir.join("index.db")
}
