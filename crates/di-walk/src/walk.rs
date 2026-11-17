//! Parallel directory walk.
//!
//! The unit of work is a directory. Workers pop a [`DirJob`] from a shared LIFO stack
//! (depth-first order keeps locality), list it, send one [`DirBatch`] to the sink and
//! then push the subdirectories. Because a directory's batch is always sent before
//! its children are queued, the consumer sees every parent before its children, and
//! a persisted batch stream is a valid checkpoint: the remaining work is exactly the
//! set of directories recorded as pending.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;
use di_model::{FileKind, flags, names};

use crate::list::{Entry, EntryKind, Lister, Meta};

#[derive(Debug, Clone)]
pub struct WalkConfig {
    pub threads: usize,
    /// Directory names whose contents are not listed (exact match on the NFC name).
    pub skip_dir_names: Vec<String>,
    /// Do not descend into other mounted filesystems.
    pub one_file_system: bool,
    pub record_atime: bool,
}

impl Default for WalkConfig {
    fn default() -> Self {
        WalkConfig {
            threads: default_threads(),
            skip_dir_names: names::DEFAULT_SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
            one_file_system: true,
            record_atime: false,
        }
    }
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8)
}

/// Pause / cancel switches shared with the UI.
#[derive(Debug, Default)]
pub struct Control {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl Control {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Live counters, sampled by the UI. No per-file messages.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub alloc_bytes: AtomicU64,
    pub errors: AtomicU64,
    /// Workers currently listing a directory (0 while paused or idle).
    pub busy_workers: AtomicUsize,
    /// Content phase (hashes + media metadata): files to process / done / bytes read.
    pub content_total: AtomicU64,
    pub content_done: AtomicU64,
    pub content_bytes: AtomicU64,
    current: Mutex<String>,
}

impl Progress {
    pub fn current_path(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    pub fn set_current(&self, path: &str) {
        if let Ok(mut cur) = self.current.try_lock() {
            cur.clear();
            cur.push_str(path);
        }
    }

    /// Zero all counters (between filesystems of one drive).
    pub fn reset(&self) {
        for a in [&self.files, &self.dirs, &self.bytes, &self.alloc_bytes, &self.errors, &self.content_total, &self.content_done, &self.content_bytes] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

/// A directory waiting to be listed.
#[derive(Debug, Clone)]
pub struct DirJob {
    pub id: u64,
    /// Raw path bytes relative to the scan root; empty for the root.
    pub os_path: Vec<u8>,
    /// Normalized display path: `/` for the root, `/a/b` below it.
    pub path: String,
    pub depth: u32,
    pub in_trash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DirState {
    Pending = 0,
    Listed = 1,
    Error = 2,
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub name: String,
    pub name_raw: Option<Vec<u8>>,
    pub ext: Option<String>,
    pub kind: FileKind,
    pub meta: Meta,
    pub link_target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DirRow {
    pub id: u64,
    pub parent_id: u64,
    pub name: String,
    pub name_raw: Option<Vec<u8>>,
    pub path: String,
    pub os_path: Vec<u8>,
    pub depth: u32,
    pub meta: Meta,
    /// Pending when queued for listing; Listed when deliberately not descended.
    pub state: DirState,
}

#[derive(Debug, Clone)]
pub struct ErrorRow {
    pub path: String,
    pub op: &'static str,
    pub errno: Option<i32>,
    pub message: String,
}

/// Result of listing one directory.
#[derive(Debug, Clone)]
pub struct DirBatch {
    pub dir_id: u64,
    pub state: DirState,
    pub entry_count: u32,
    /// Flags to OR into the directory's own row (e.g. LIST_ERROR).
    pub dir_flags: u32,
    pub files: Vec<FileRow>,
    pub subdirs: Vec<DirRow>,
    pub errors: Vec<ErrorRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkOutcome {
    Completed,
    Cancelled,
}

struct Queue {
    stack: Vec<DirJob>,
    busy: usize,
    done: bool,
}

/// Walk from `jobs` (the root for a fresh scan, or all pending directories when
/// resuming). `next_id` is the first unused directory id. `root_dev` is the device
/// of the scan root, used to stay on one filesystem.
#[allow(clippy::too_many_arguments)]
pub fn walk(
    root: &Path,
    root_dev: Option<u64>,
    jobs: Vec<DirJob>,
    next_id: u64,
    cfg: &WalkConfig,
    lister: &dyn Lister,
    control: &Control,
    progress: &Progress,
    sink: &Sender<DirBatch>,
) -> WalkOutcome {
    // Nothing to do (a resumed scan whose directories were all listed): workers would
    // otherwise wait for a job that never comes, since `done` is set after a job.
    let queue = Mutex::new(Queue { done: jobs.is_empty(), stack: jobs, busy: 0 });
    let cv = Condvar::new();
    let ids = AtomicU64::new(next_id);
    let threads = cfg.threads.max(1);

    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut entries = Vec::new();
                let mut buf = Vec::new();
                while let Some(job) = next_job(&queue, &cv, control) {
                    progress.busy_workers.fetch_add(1, Ordering::Relaxed);
                    let (batch, children) =
                        list_dir(root, root_dev, &job, cfg, lister, &ids, &mut entries, &mut buf);
                    record_progress(progress, &job, &batch);
                    progress.busy_workers.fetch_sub(1, Ordering::Relaxed);
                    // Send before queueing children: parents always reach the sink first.
                    let sent = sink.send(batch).is_ok();
                    let mut q = queue.lock().unwrap();
                    if sent {
                        q.stack.extend(children);
                    } else {
                        // Consumer gone: stop everything.
                        q.done = true;
                    }
                    q.busy -= 1;
                    if q.stack.is_empty() && q.busy == 0 {
                        q.done = true;
                    }
                    drop(q);
                    cv.notify_all();
                }
            });
        }
    });

    if control.is_cancelled() { WalkOutcome::Cancelled } else { WalkOutcome::Completed }
}

fn next_job(queue: &Mutex<Queue>, cv: &Condvar, control: &Control) -> Option<DirJob> {
    let mut q = queue.lock().unwrap();
    loop {
        if q.done || control.is_cancelled() {
            q.done = true;
            cv.notify_all();
            return None;
        }
        if !control.is_paused() {
            if let Some(job) = q.stack.pop() {
                q.busy += 1;
                return Some(job);
            }
        }
        // Timeout so pause/cancel flips are noticed without explicit notification.
        q = cv.wait_timeout(q, Duration::from_millis(50)).unwrap().0;
    }
}

fn record_progress(progress: &Progress, job: &DirJob, batch: &DirBatch) {
    progress.dirs.fetch_add(1, Ordering::Relaxed);
    progress.files.fetch_add(batch.files.len() as u64, Ordering::Relaxed);
    let (bytes, alloc) = batch
        .files
        .iter()
        .fold((0u64, 0u64), |(b, a), f| (b + f.meta.size, a + f.meta.alloc.unwrap_or(0)));
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    progress.alloc_bytes.fetch_add(alloc, Ordering::Relaxed);
    progress.errors.fetch_add(batch.errors.len() as u64, Ordering::Relaxed);
    if let Ok(mut cur) = progress.current.try_lock() {
        cur.clear();
        cur.push_str(&job.path);
    }
}

#[allow(clippy::too_many_arguments)]
fn list_dir(
    root: &Path,
    root_dev: Option<u64>,
    job: &DirJob,
    cfg: &WalkConfig,
    lister: &dyn Lister,
    ids: &AtomicU64,
    entries: &mut Vec<Entry>,
    buf: &mut Vec<u8>,
) -> (DirBatch, Vec<DirJob>) {
    let full: PathBuf = if job.os_path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(OsStr::from_bytes(&job.os_path))
    };

    let mut batch = DirBatch {
        dir_id: job.id,
        state: DirState::Listed,
        entry_count: 0,
        dir_flags: 0,
        files: Vec::new(),
        subdirs: Vec::new(),
        errors: Vec::new(),
    };
    let mut children = Vec::new();

    if let Err(e) = lister.list(&full, entries, buf) {
        batch.state = DirState::Error;
        batch.dir_flags |= flags::LIST_ERROR;
        batch.errors.push(ErrorRow {
            path: job.path.clone(),
            op: "readdir",
            errno: e.raw_os_error(),
            message: e.to_string(),
        });
        return (batch, children);
    }

    batch.entry_count = entries.len() as u32;
    let inherited = if job.in_trash { flags::IN_TRASH } else { 0 };

    for entry in entries.drain(..) {
        let stored = names::normalize(&entry.name);
        // Display paths are only needed for directories and error rows.
        let child_path = || {
            if job.path == "/" { format!("/{}", stored.name) } else { format!("{}/{}", job.path, stored.name) }
        };
        if let Some((op, errno, message)) = &entry.error {
            batch.errors.push(ErrorRow {
                path: child_path(),
                op,
                errno: *errno,
                message: message.clone(),
            });
        }
        let mut meta = entry.meta;
        if !cfg.record_atime {
            meta.atime_ns = None;
        }
        if stored.name.starts_with('.') {
            meta.flags |= flags::HIDDEN;
        }
        meta.flags |= inherited;

        match entry.kind {
            EntryKind::Dir => {
                let child_path = child_path();
                let id = ids.fetch_add(1, Ordering::Relaxed);
                let os_path = if job.os_path.is_empty() {
                    entry.name.clone()
                } else {
                    let mut p = job.os_path.clone();
                    p.push(b'/');
                    p.extend_from_slice(&entry.name);
                    p
                };
                let trash = job.in_trash || names::is_trash_dir(&stored.name);
                if trash {
                    meta.flags |= flags::IN_TRASH;
                }
                let other_fs = cfg.one_file_system
                    && (meta.mount_point
                        || matches!((meta.dev, root_dev), (Some(d), Some(r)) if d != r));
                let skip = other_fs || cfg.skip_dir_names.contains(&stored.name);
                let state = if skip {
                    meta.flags |= flags::SKIPPED;
                    DirState::Listed
                } else {
                    children.push(DirJob {
                        id,
                        os_path: os_path.clone(),
                        path: child_path.clone(),
                        depth: job.depth + 1,
                        in_trash: trash,
                    });
                    DirState::Pending
                };
                batch.subdirs.push(DirRow {
                    id,
                    parent_id: job.id,
                    name: stored.name,
                    name_raw: stored.raw,
                    path: child_path,
                    os_path,
                    depth: job.depth + 1,
                    meta,
                    state,
                });
            }
            EntryKind::NonDir(kind) => {
                if kind == FileKind::File && is_sparse(&meta) {
                    meta.flags |= flags::SPARSE;
                }
                let ext = names::extension(&stored.name);
                let link_target = entry
                    .link_target
                    .map(|t| String::from_utf8_lossy(&t).into_owned());
                batch.files.push(FileRow {
                    name: stored.name,
                    name_raw: stored.raw,
                    ext,
                    kind,
                    meta,
                    link_target,
                });
            }
        }
    }
    (batch, children)
}

/// Conservative sparse-file heuristic: at least 1 MiB logical, less than half of it
/// allocated, and not compressed (compressed files also allocate less than their size).
fn is_sparse(meta: &Meta) -> bool {
    match meta.alloc {
        Some(alloc) => {
            meta.size >= 1 << 20 && alloc < meta.size / 2 && meta.flags & flags::COMPRESSED == 0
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::list::{GenericLister, best_lister};
    use std::collections::BTreeSet;
    use std::fs;

    fn make_tree(root: &Path) {
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join(".Spotlight-V100/Store")).unwrap();
        fs::create_dir_all(root.join(".Trashes/501")).unwrap();
        fs::write(root.join("top.txt"), b"hello").unwrap();
        fs::write(root.join("a/one.CR2"), vec![0u8; 1000]).unwrap();
        fs::write(root.join("a/b/two.jpg"), vec![1u8; 2000]).unwrap();
        fs::write(root.join("a/b/c/.hidden"), b"x").unwrap();
        fs::write(root.join(".Spotlight-V100/Store/db"), b"skip me").unwrap();
        fs::write(root.join(".Trashes/501/deleted.mov"), b"gone").unwrap();
        std::os::unix::fs::symlink("a/one.CR2", root.join("link")).unwrap();
        fs::hard_link(root.join("top.txt"), root.join("a/top-hardlink.txt")).unwrap();
    }

    fn run(root: &Path, lister: &dyn Lister, threads: usize) -> Vec<DirBatch> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let cfg = WalkConfig { threads, ..WalkConfig::default() };
        let (_, root_meta) = crate::stat_path(root).unwrap();
        let job = DirJob { id: 1, os_path: vec![], path: "/".into(), depth: 0, in_trash: false };
        let control = Control::default();
        let progress = Progress::default();
        let out = walk(root, root_meta.dev, vec![job], 2, &cfg, lister, &control, &progress, &tx);
        assert_eq!(out, WalkOutcome::Completed);
        drop(tx);
        rx.iter().collect()
    }

    fn file_paths(batches: &[DirBatch]) -> BTreeSet<String> {
        let mut dir_paths = std::collections::HashMap::new();
        dir_paths.insert(1u64, "/".to_string());
        for b in batches {
            for d in &b.subdirs {
                dir_paths.insert(d.id, d.path.clone());
            }
        }
        let mut out = BTreeSet::new();
        for b in batches {
            let dp = &dir_paths[&b.dir_id];
            for f in &b.files {
                let p = if dp == "/" { format!("/{}", f.name) } else { format!("{dp}/{}", f.name) };
                out.insert(p);
            }
        }
        out
    }

    #[test]
    fn walks_tree_with_both_listers() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let expected: BTreeSet<String> = [
            "/top.txt",
            "/link",
            "/a/one.CR2",
            "/a/top-hardlink.txt",
            "/a/b/two.jpg",
            "/a/b/c/.hidden",
            "/.Trashes/501/deleted.mov",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        for (lister, threads) in [(best_lister(), 4usize), (Box::new(GenericLister) as Box<dyn Lister>, 1)] {
            let batches = run(tmp.path(), lister.as_ref(), threads);
            assert_eq!(file_paths(&batches), expected, "lister {}", lister.method());

            let all_files: Vec<&FileRow> = batches.iter().flat_map(|b| &b.files).collect();
            let cr2 = all_files.iter().find(|f| f.name == "one.CR2").unwrap();
            assert_eq!(cr2.meta.size, 1000);
            assert_eq!(cr2.ext.as_deref(), Some("cr2"));
            assert!(cr2.meta.mtime_ns.is_some() && cr2.meta.inode.is_some());

            let link = all_files.iter().find(|f| f.name == "link").unwrap();
            assert_eq!(link.kind, FileKind::Symlink);
            assert_eq!(link.link_target.as_deref(), Some("a/one.CR2"));

            let hidden = all_files.iter().find(|f| f.name == ".hidden").unwrap();
            assert!(hidden.meta.flags & flags::HIDDEN != 0);

            let trashed = all_files.iter().find(|f| f.name == "deleted.mov").unwrap();
            assert!(trashed.meta.flags & flags::IN_TRASH != 0);

            let top = all_files.iter().find(|f| f.name == "top.txt").unwrap();
            assert_eq!(top.meta.nlink, Some(2));

            let spotlight = batches
                .iter()
                .flat_map(|b| &b.subdirs)
                .find(|d| d.name == ".Spotlight-V100")
                .unwrap();
            assert_eq!(spotlight.state, DirState::Listed);
            assert!(spotlight.meta.flags & flags::SKIPPED != 0);

            // Every subdirectory batch arrives after the batch that discovered it.
            let mut seen = std::collections::HashSet::from([1u64]);
            for b in &batches {
                assert!(seen.contains(&b.dir_id), "child batch before parent");
                for d in &b.subdirs {
                    seen.insert(d.id);
                }
            }
        }
    }

    #[test]
    fn unreadable_dir_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("locked")).unwrap();
        fs::write(tmp.path().join("locked/secret"), b"x").unwrap();
        fs::set_permissions(tmp.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let is_root = unsafe { libc::geteuid() } == 0;
        let batches = run(tmp.path(), best_lister().as_ref(), 2);
        fs::set_permissions(tmp.path().join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if is_root {
            return; // root can read anything
        }
        let err_batch = batches.iter().find(|b| b.state == DirState::Error).expect("error batch");
        assert_eq!(err_batch.errors[0].op, "readdir");
        assert_eq!(err_batch.errors[0].path, "/locked");
    }

    #[test]
    fn nothing_to_do_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let out = walk(
            tmp.path(),
            None,
            vec![],
            2,
            &WalkConfig::default(),
            best_lister().as_ref(),
            &Control::default(),
            &Progress::default(),
            &tx,
        );
        assert_eq!(out, WalkOutcome::Completed);
        drop(tx);
        assert_eq!(rx.iter().count(), 0);
    }

    #[test]
    fn cancel_stops_walk() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        let control = Control::default();
        control.cancel();
        let progress = Progress::default();
        let job = DirJob { id: 1, os_path: vec![], path: "/".into(), depth: 0, in_trash: false };
        let out = walk(
            tmp.path(),
            None,
            vec![job],
            2,
            &WalkConfig::default(),
            best_lister().as_ref(),
            &control,
            &progress,
            &tx,
        );
        assert_eq!(out, WalkOutcome::Cancelled);
        drop(tx);
        assert_eq!(rx.iter().count(), 0);
    }
}
