//! The content phase: after a filesystem's walk has been merged and
//! while it is still mounted, read file contents for quick hashes and media metadata.
//!
//! "Pending" is a query, not stored state: media files without metadata (or with
//! partial metadata that a now-installed tool can complete), and duplicate candidates
//! without a quick hash. Carry-over at merge keeps unchanged files' rows, so only new
//! or changed files are read; an interrupted phase resumes on the next run.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::RecvTimeoutError;
use rusqlite::{Connection, params};

use di_core::{Catalog, now_secs};
use di_media::{Extracted, Extractor, MediaInfo, MediaKind};
use di_model::flags;
use di_walk::{Control, Progress};

// Note [Drive trouble]
// ~~~~~~~~~~~~~~~~~~~~
// The content phase reads file data, and that is where a failing drive or a flaky
// USB bridge shows itself. A read that hits trouble does not fail quickly: the kernel
// retries it, the bridge times out after 30 s and is reset, the kernel retries again,
// and one 64 KiB read can take minutes. With thousands of files left to read,
// carrying on keeps a struggling drive busy for hours.
//
// So the phase watches for two signs of trouble:
//
//  * a stall: one worker has made no progress for `stall_after` (30 s). That is
//    longer than any healthy read, including ffprobe and exiftool on large files
//    over USB 2; a full hash counts every MiB read as progress.
//  * repeated device errors: `max_device_errors` (3) files failed with EIO and the
//    like (Note [Device errors] in di-walk). Per-file errors such as EACCES don't
//    count.
//
// On either, the workers take no more files, and the phase ends once the reads in
// flight return (a read blocked in the kernel cannot be abandoned). The outcome says
// why. Files not read stay pending, since "pending" is a query, and are read by a
// later scan. A device scan then reads no other filesystem of the same drive (see
// Note [Walk first, read later] in di-scan:device).

#[derive(Debug, Clone)]
pub struct ContentConfig {
    pub threads: usize,
    pub media: bool,
    /// Quick-hash files whose size also occurs elsewhere in the catalog.
    pub hash_candidates: bool,
    pub min_candidate_size: u64,
    pub ffprobe: Option<PathBuf>,
    pub exiftool: Option<PathBuf>,
    /// Full-hash files whose quick hash matches another file's ("likely"
    /// duplicates), turning them into verified ("exact") duplicates.
    pub verify: bool,
    /// Stop when one file makes no progress for this long. See Note [Drive trouble].
    pub stall_after: Duration,
    /// Stop after this many files failed with device errors. See Note [Drive trouble].
    pub max_device_errors: u64,
}

impl Default for ContentConfig {
    fn default() -> Self {
        ContentConfig {
            threads: 2,
            media: true,
            hash_candidates: true,
            min_candidate_size: 1 << 20,
            ffprobe: di_media::locate_tool("ffprobe"),
            exiftool: di_media::locate_tool("exiftool"),
            verify: false,
            stall_after: Duration::from_secs(30),
            max_device_errors: 3,
        }
    }
}

/// Why the content phase stopped reading from a drive. See Note [Drive trouble].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveTrouble {
    /// A read of `path` made no progress for `secs` seconds.
    Stalled { path: String, secs: u64 },
    /// `count` files failed with device errors; `last` describes the latest.
    ReadErrors { count: u64, last: String },
}

impl DriveTrouble {
    pub fn describe(&self) -> String {
        match self {
            DriveTrouble::Stalled { path, secs } => format!("a read made no progress for {secs} s ({path})"),
            DriveTrouble::ReadErrors { count, last } => format!("{count} files failed with read errors (last: {last})"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentOutcome {
    pub processed: u64,
    pub media: u64,
    pub hashed: u64,
    pub verified: u64,
    pub errors: u64,
    pub bytes_read: u64,
    pub cancelled: bool,
    /// Stopped early because the drive is not reading reliably.
    pub stopped: Option<DriveTrouble>,
    pub ms: u64,
}

impl ContentOutcome {
    /// Files remain to be read on a later scan.
    pub fn unfinished(&self) -> bool {
        self.cancelled || self.stopped.is_some()
    }
}

/// Decides when a drive is too unreliable to keep reading from. See Note [Drive trouble].
struct TroubleMonitor {
    stall_after: Duration,
    max_device_errors: u64,
    device_errors: u64,
}

impl TroubleMonitor {
    fn new(cfg: &ContentConfig) -> Self {
        TroubleMonitor { stall_after: cfg.stall_after, max_device_errors: cfg.max_device_errors, device_errors: 0 }
    }

    /// A file could not be read.
    fn on_error(&mut self, e: &std::io::Error, path: &Path) -> Option<DriveTrouble> {
        if !e.raw_os_error().is_some_and(di_walk::is_device_errno) {
            return None;
        }
        self.device_errors += 1;
        (self.device_errors >= self.max_device_errors)
            .then(|| DriveTrouble::ReadErrors { count: self.device_errors, last: format!("{}: {e}", path.display()) })
    }

    /// A worker has been reading `path` without progress since `since`.
    fn stalled(&self, now: Instant, since: Instant, path: &Path) -> Option<DriveTrouble> {
        let idle = now.saturating_duration_since(since);
        (idle >= self.stall_after).then(|| DriveTrouble::Stalled { path: path.display().to_string(), secs: idle.as_secs() })
    }
}

/// What a worker is reading, and when it last made progress.
struct Busy {
    since: Instant,
    path: PathBuf,
}

/// One file's results, from a worker to the writer.
struct Done {
    file_id: i64,
    path: PathBuf,
    extracted: std::io::Result<Extracted>,
    /// Ok(None) when no full hash was wanted, or it was stopped midway.
    full: std::io::Result<Option<[u8; 32]>>,
}

struct Work {
    file_id: i64,
    path: PathBuf,
    kind: Option<MediaKind>,
    want_hash: bool,
    want_full: bool,
    inode: i64,
}

/// (id, parent, name, raw name)
type DirNameRow = (i64, Option<i64>, String, Option<Vec<u8>>);

/// Raw (on-disk) relative paths of all directories of a filesystem.
fn dir_paths(conn: &Connection, fs: i64) -> Result<HashMap<i64, Vec<u8>>> {
    let mut stmt = conn.prepare("SELECT id, parent_id, name, name_raw FROM dirs WHERE filesystem_id = ?1")?;
    let rows: Vec<DirNameRow> =
        stmt.query_map([fs], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let nodes: HashMap<i64, (Option<i64>, Vec<u8>)> =
        rows.into_iter().map(|(id, p, n, raw)| (id, (p, raw.unwrap_or_else(|| n.into_bytes())))).collect();
    let mut out: HashMap<i64, Vec<u8>> = HashMap::with_capacity(nodes.len());
    for &id in nodes.keys() {
        if out.contains_key(&id) {
            continue;
        }
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if out.contains_key(&c) {
                break;
            }
            chain.push(c);
            cur = nodes.get(&c).and_then(|n| n.0);
        }
        for c in chain.into_iter().rev() {
            let (parent, name) = &nodes[&c];
            let path = match parent.and_then(|p| out.get(&p)) {
                Some(pp) if pp.is_empty() => name.clone(),
                Some(pp) => [pp.as_slice(), b"/", name.as_slice()].concat(),
                None => Vec::new(), // the root
            };
            out.insert(c, path);
        }
    }
    Ok(out)
}

/// Does this filesystem have media files without (complete) metadata?
pub fn has_pending_media(conn: &Connection, fs: i64, ffprobe_available: bool) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(
                SELECT 1 FROM files f JOIN ext_categories c ON c.ext = f.ext
                LEFT JOIN media m ON m.file_id = f.id
                WHERE f.filesystem_id = ?1 AND f.kind = 0 AND (f.flags & {dup}) = 0
                  AND c.category IN ('photo', 'photo_raw', 'video', 'audio')
                  AND (m.file_id IS NULL OR (?2 AND m.status = 1 AND m.extractor NOT LIKE '%ffprobe%')))",
            dup = flags::HARDLINK_DUP
        ),
        params![fs, ffprobe_available],
        |r| r.get(0),
    )?)
}

/// A file the content phase still has to read.
struct PendingRow {
    file_id: i64,
    dir_id: i64,
    name: Vec<u8>,
    inode: i64,
    kind: Option<MediaKind>,
    want_hash: bool,
    want_full: bool,
}

/// Does this filesystem have files the content phase would read with `cfg`?
pub fn has_pending(conn: &Connection, fs: i64, cfg: &ContentConfig) -> Result<bool> {
    let mut any = false;
    select_pending(conn, fs, cfg, &mut |_| {
        any = true;
        false
    })?;
    Ok(any)
}

fn pending(conn: &Connection, fs: i64, root: &Path, cfg: &ContentConfig) -> Result<Vec<Work>> {
    let dirs = dir_paths(conn, fs)?;
    let mut out = Vec::new();
    select_pending(conn, fs, cfg, &mut |p| {
        if let Some(dir) = dirs.get(&p.dir_id) {
            let rel = if dir.is_empty() { p.name } else { [dir.as_slice(), b"/", p.name.as_slice()].concat() };
            out.push(Work {
                file_id: p.file_id,
                path: root.join(OsStr::from_bytes(&rel)),
                kind: p.kind,
                want_hash: p.want_hash,
                want_full: p.want_full,
                inode: p.inode,
            });
        }
        true
    })?;
    // Roughly on-disk order: fewer seeks on hard disks.
    out.sort_by_key(|w| w.inode);
    Ok(out)
}

/// Visit the files of a filesystem that the content phase still has to read, until
/// `visit` returns false. The one definition of "pending" (see the module docs).
fn select_pending(conn: &Connection, fs: i64, cfg: &ContentConfig, visit: &mut dyn FnMut(PendingRow) -> bool) -> Result<()> {
    let candidate_sizes: HashSet<i64> = if cfg.hash_candidates {
        let mut stmt = conn.prepare(
            "SELECT size FROM files WHERE kind = 0 AND size >= ?1 GROUP BY size HAVING count(*) > 1",
        )?;
        stmt.query_map([cfg.min_candidate_size as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    } else {
        HashSet::new()
    };
    // Quick hashes shared by more than one file of the same size: verification candidates.
    let likely: HashSet<(Vec<u8>, i64)> = if cfg.verify {
        let mut stmt = conn.prepare(
            "SELECT h.quick, f.size FROM file_hashes h JOIN files f ON f.id = h.file_id
             WHERE h.quick IS NOT NULL GROUP BY h.quick, f.size HAVING count(*) > 1",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    } else {
        HashSet::new()
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT f.id, f.dir_id, f.name, f.name_raw, f.size, COALESCE(f.inode, 0), c.category,
                h.file_id IS NOT NULL, m.status, m.extractor, h.quick, h.full IS NOT NULL
         FROM files f
         LEFT JOIN ext_categories c ON c.ext = f.ext
         LEFT JOIN file_hashes h ON h.file_id = f.id
         LEFT JOIN media m ON m.file_id = f.id
         WHERE f.filesystem_id = ?1 AND f.kind = 0 AND (f.flags & {dup}) = 0",
        dup = flags::HARDLINK_DUP
    ))?;
    let mut rows = stmt.query([fs])?;
    while let Some(r) = rows.next()? {
        let (id, dir_id, name, raw, size, inode): (i64, i64, String, Option<Vec<u8>>, i64, i64) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
        let category: Option<String> = r.get(6)?;
        let has_hash: bool = r.get(7)?;
        let media_status: Option<i64> = r.get(8)?;
        let extractor: Option<String> = r.get(9)?;
        let kind = category.as_deref().and_then(MediaKind::from_category).filter(|_| cfg.media);
        let needs_media = kind.is_some()
            && match media_status {
                None => true,
                // Partial video metadata, and ffprobe has become available since.
                Some(1) => cfg.ffprobe.is_some() && extractor.as_deref().is_some_and(|e| !e.contains("ffprobe")),
                Some(_) => false,
            };
        let want_hash = !has_hash && (needs_media || kind.is_some() || candidate_sizes.contains(&size));
        let quick: Option<Vec<u8>> = r.get(10)?;
        let has_full: bool = r.get(11)?;
        let want_full = cfg.verify && !has_full && quick.is_some_and(|q| likely.contains(&(q, size)));
        if !needs_media && !want_hash && !want_full {
            continue;
        }
        let row = PendingRow {
            file_id: id,
            dir_id,
            name: raw.unwrap_or_else(|| name.into_bytes()),
            inode,
            kind: if needs_media { kind } else { None },
            want_hash,
            want_full,
        };
        if !visit(row) {
            break;
        }
    }
    Ok(())
}

/// Run the content phase for one mounted filesystem. `on_start` is told how many
/// files will be read, once, before reading starts (not at all if there are none).
pub fn run_content(
    catalog: &mut Catalog,
    fs: i64,
    root: &Path,
    cfg: &ContentConfig,
    control: &Control,
    progress: &Progress,
    on_start: &mut dyn FnMut(u64),
) -> Result<ContentOutcome> {
    let start = Instant::now();
    let work = pending(catalog.conn(), fs, root, cfg)?;
    progress.content_total.store(work.len() as u64, Ordering::Relaxed);
    progress.content_done.store(0, Ordering::Relaxed);
    let mut outcome = ContentOutcome::default();
    if work.is_empty() {
        return Ok(outcome);
    }
    on_start(work.len() as u64);

    let (work_tx, work_rx) = crossbeam_channel::unbounded::<Work>();
    for w in work {
        let _ = work_tx.send(w);
    }
    drop(work_tx);
    let (res_tx, res_rx) = crossbeam_channel::bounded::<Done>(256);
    // Set when the drive is in trouble: workers take no more files.
    let halt = AtomicBool::new(false);
    let slots: Vec<Mutex<Option<Busy>>> = (0..cfg.threads.max(1)).map(|_| Mutex::new(None)).collect();
    let mut monitor = TroubleMonitor::new(cfg);

    std::thread::scope(|s| -> Result<()> {
        for slot in &slots {
            let (work_rx, res_tx, halt) = (work_rx.clone(), res_tx.clone(), &halt);
            s.spawn(move || {
                let mut ex = Extractor::new(cfg.ffprobe.clone(), cfg.exiftool.clone());
                let stop = || control.is_cancelled() || halt.load(Ordering::Relaxed);
                while let Ok(w) = work_rx.recv() {
                    while control.is_paused() && !stop() {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if stop() {
                        break;
                    }
                    progress.set_current(&w.path.to_string_lossy());
                    *slot.lock().unwrap() = Some(Busy { since: Instant::now(), path: w.path.clone() });
                    let extracted = if w.kind.is_some() || w.want_hash {
                        ex.extract(&w.path, w.kind, w.want_hash)
                    } else {
                        Ok(Extracted { quick_hash: None, media: None, bytes_read: 0 })
                    };
                    let full = match &extracted {
                        Ok(_) if w.want_full => di_media::full_hash(&w.path, &stop, &|n| {
                            progress.content_bytes.fetch_add(n, Ordering::Relaxed);
                            if let Some(b) = slot.lock().unwrap().as_mut() {
                                b.since = Instant::now();
                            }
                        }),
                        _ => Ok(None),
                    };
                    *slot.lock().unwrap() = None;
                    if res_tx.send(Done { file_id: w.file_id, path: w.path, extracted, full }).is_err() {
                        break;
                    }
                }
            });
        }
        drop(res_tx);

        // Single writer: batched transactions. It also watches for drive trouble.
        let conn = catalog.conn_mut();
        let mut tx = conn.transaction()?;
        let mut in_tx = 0usize;
        let mut last = Instant::now();
        loop {
            match res_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(d) => {
                    progress.content_done.fetch_add(1, Ordering::Relaxed);
                    outcome.processed += 1;
                    in_tx += 1;
                    // Unreadable now (permissions, unplugged): not recorded, retried next time.
                    let mut failed = |e: &std::io::Error, outcome: &mut ContentOutcome| {
                        outcome.errors += 1;
                        let t = monitor.on_error(e, &d.path);
                        if outcome.stopped.is_none() {
                            outcome.stopped = t;
                        }
                    };
                    match &d.extracted {
                        Ok(e) => {
                            progress.content_bytes.fetch_add(e.bytes_read, Ordering::Relaxed);
                            outcome.bytes_read += e.bytes_read;
                            if let Some(h) = e.quick_hash {
                                tx.execute(
                                    "INSERT INTO file_hashes (file_id, quick, full, hashed_at) VALUES (?1, ?2, NULL, ?3)
                                     ON CONFLICT (file_id) DO UPDATE SET quick = excluded.quick, hashed_at = excluded.hashed_at",
                                    params![d.file_id, h.as_slice(), now_secs()],
                                )?;
                                outcome.hashed += 1;
                            }
                            if let Some(m) = &e.media {
                                save_media(&tx, d.file_id, m)?;
                                outcome.media += 1;
                            }
                            match &d.full {
                                Ok(Some(full)) => {
                                    tx.execute(
                                        "UPDATE file_hashes SET full = ?2 WHERE file_id = ?1",
                                        params![d.file_id, full.as_slice()],
                                    )?;
                                    outcome.verified += 1;
                                }
                                Ok(None) => {}
                                Err(e) => failed(e, &mut outcome),
                            }
                        }
                        Err(e) => failed(e, &mut outcome),
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if outcome.stopped.is_none() {
                let now = Instant::now();
                outcome.stopped = slots
                    .iter()
                    .find_map(|s| s.lock().unwrap().as_ref().and_then(|b| monitor.stalled(now, b.since, &b.path)));
            }
            if outcome.stopped.is_some() {
                halt.store(true, Ordering::Relaxed);
            }
            if in_tx > 0 && (in_tx >= 500 || last.elapsed() >= Duration::from_millis(500)) {
                tx.commit()?;
                tx = conn.transaction()?;
                in_tx = 0;
                last = Instant::now();
            }
        }
        tx.commit()?;
        Ok(())
    })?;

    outcome.cancelled = control.is_cancelled();
    outcome.ms = start.elapsed().as_millis() as u64;
    catalog.conn().execute(
        "UPDATE scans SET hashed_quick = hashed_quick + ?2, media_extracted = media_extracted + ?3,
                content_pending = CASE WHEN ?4 THEN 1 ELSE NULL END
         WHERE id = (SELECT current_scan_id FROM filesystems WHERE id = ?1)",
        params![fs, outcome.hashed as i64, outcome.media as i64, outcome.unfinished()],
    )?;
    Ok(outcome)
}

fn save_media(conn: &Connection, file_id: i64, m: &MediaInfo) -> Result<()> {
    let tags = (!m.tags.is_empty()).then(|| serde_json::Value::Object(m.tags.clone()).to_string());
    let mut tags_v = tags;
    if let Some(e) = &m.error {
        let mut obj: serde_json::Map<String, serde_json::Value> =
            tags_v.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
        obj.insert("error".into(), e.clone().into());
        tags_v = Some(serde_json::Value::Object(obj).to_string());
    }
    conn.execute("DELETE FROM media_streams WHERE file_id = ?1", [file_id])?;
    conn.execute(
        "INSERT OR REPLACE INTO media (file_id, kind, status, extractor, extracted_at, captured_at, captured_offset_min,
            captured_offset_inferred, width, height, duration_ms, gps_lat, gps_lon, gps_alt_m, camera_make, camera_model,
            camera_serial, lens_model, orientation, iso, exposure_s, f_number, focal_mm, container, bitrate_bps, title,
            artist, album, album_artist, track_no, disc_no, year, genre, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
                 ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34)",
        rusqlite::params_from_iter([
            &file_id as &dyn rusqlite::ToSql,
            &m.kind.as_str(),
            &(m.status as u8),
            &m.extractor,
            &now_secs(),
            &m.captured_at,
            &m.captured_offset_min,
            &m.captured_offset_inferred,
            &m.width,
            &m.height,
            &m.duration_ms.map(|v| v as i64),
            &m.gps_lat,
            &m.gps_lon,
            &m.gps_alt_m,
            &m.camera_make,
            &m.camera_model,
            &m.camera_serial,
            &m.lens_model,
            &m.orientation,
            &m.iso,
            &m.exposure_s,
            &m.f_number,
            &m.focal_mm,
            &m.container,
            &m.bitrate_bps.map(|v| v as i64),
            &m.title,
            &m.artist,
            &m.album,
            &m.album_artist,
            &m.track_no,
            &m.disc_no,
            &m.year,
            &m.genre,
            &tags_v,
        ]),
    )?;
    let mut ins = conn.prepare_cached(
        "INSERT OR REPLACE INTO media_streams (file_id, idx, type, codec, profile, width, height, fps, bitrate_bps,
            sample_rate, channels, bit_depth, language, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?;
    for s in &m.streams {
        ins.execute(params![
            file_id,
            s.idx,
            s.kind,
            s.codec,
            s.profile,
            s.width,
            s.height,
            s.fps,
            s.bitrate_bps.map(|v| v as i64),
            s.sample_rate,
            s.channels,
            s.bit_depth,
            s.language,
            s.duration_ms.map(|v| v as i64)
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(max_device_errors: u64) -> TroubleMonitor {
        TroubleMonitor { stall_after: Duration::from_secs(30), max_device_errors, device_errors: 0 }
    }

    #[test]
    fn device_errors_stop_reading_but_file_errors_do_not() {
        let mut m = monitor(3);
        let p = Path::new("/x/a.db");
        let os = std::io::Error::from_raw_os_error;
        for _ in 0..10 {
            assert_eq!(m.on_error(&os(libc::EACCES), p), None);
            assert_eq!(m.on_error(&os(libc::ENOENT), p), None);
        }
        assert_eq!(m.on_error(&std::io::Error::other("read cap exceeded"), p), None);
        assert_eq!(m.on_error(&os(libc::EIO), p), None);
        assert_eq!(m.on_error(&os(libc::ENODEV), p), None);
        let t = m.on_error(&os(libc::EIO), p).expect("third device error");
        assert!(matches!(t, DriveTrouble::ReadErrors { count: 3, .. }), "{t:?}");
        assert!(t.describe().contains("/x/a.db"), "{}", t.describe());
    }

    #[test]
    fn a_stall_is_time_without_progress() {
        let m = monitor(3);
        let now = Instant::now();
        let ago = |s| now.checked_sub(Duration::from_secs(s)).unwrap();
        let p = Path::new("/x/b.a");
        assert_eq!(m.stalled(now, ago(29), p), None);
        assert_eq!(m.stalled(now, ago(31), p), Some(DriveTrouble::Stalled { path: "/x/b.a".into(), secs: 31 }));
        // Progress after the stall was noticed (clock order) is not a stall.
        assert_eq!(m.stalled(ago(1), now, p), None);
    }
}
