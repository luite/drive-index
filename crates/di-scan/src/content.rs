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
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, params};

use di_core::{Catalog, now_secs};
use di_media::{Extracted, Extractor, MediaInfo, MediaKind};
use di_model::flags;
use di_walk::{Control, Progress};

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
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentOutcome {
    pub processed: u64,
    pub media: u64,
    pub hashed: u64,
    pub verified: u64,
    pub errors: u64,
    pub bytes_read: u64,
    pub cancelled: bool,
    pub ms: u64,
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
    type Res = (i64, std::io::Result<Extracted>, Option<[u8; 32]>);
    let (res_tx, res_rx) = crossbeam_channel::bounded::<Res>(256);

    std::thread::scope(|s| -> Result<()> {
        for _ in 0..cfg.threads.max(1) {
            let (work_rx, res_tx) = (work_rx.clone(), res_tx.clone());
            s.spawn(move || {
                let mut ex = Extractor::new(cfg.ffprobe.clone(), cfg.exiftool.clone());
                while let Ok(w) = work_rx.recv() {
                    while control.is_paused() && !control.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if control.is_cancelled() {
                        break;
                    }
                    progress.set_current(&w.path.to_string_lossy());
                    let r = if w.kind.is_some() || w.want_hash {
                        ex.extract(&w.path, w.kind, w.want_hash)
                    } else {
                        Ok(Extracted { quick_hash: None, media: None, bytes_read: 0 })
                    };
                    let full = if w.want_full && r.is_ok() {
                        di_media::full_hash(&w.path, &|| control.is_cancelled(), &|n| {
                            progress.content_bytes.fetch_add(n, Ordering::Relaxed);
                        })
                        .ok()
                        .flatten()
                    } else {
                        None
                    };
                    if res_tx.send((w.file_id, r, full)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(res_tx);

        // Single writer: batched transactions.
        let conn = catalog.conn_mut();
        let mut tx = conn.transaction()?;
        let mut in_tx = 0usize;
        let mut last = Instant::now();
        for (file_id, r, full) in res_rx.iter() {
            progress.content_done.fetch_add(1, Ordering::Relaxed);
            outcome.processed += 1;
            match r {
                Ok(e) => {
                    progress.content_bytes.fetch_add(e.bytes_read, Ordering::Relaxed);
                    outcome.bytes_read += e.bytes_read;
                    if let Some(h) = e.quick_hash {
                        tx.execute(
                            "INSERT INTO file_hashes (file_id, quick, full, hashed_at) VALUES (?1, ?2, NULL, ?3)
                             ON CONFLICT (file_id) DO UPDATE SET quick = excluded.quick, hashed_at = excluded.hashed_at",
                            params![file_id, h.as_slice(), now_secs()],
                        )?;
                        outcome.hashed += 1;
                    }
                    if let Some(m) = &e.media {
                        save_media(&tx, file_id, m)?;
                        outcome.media += 1;
                    }
                    if let Some(full) = full {
                        tx.execute("UPDATE file_hashes SET full = ?2 WHERE file_id = ?1", params![file_id, full.as_slice()])?;
                        outcome.verified += 1;
                    }
                }
                // Unreadable now (permissions, unplugged): not recorded, retried next time.
                Err(_) => outcome.errors += 1,
            }
            in_tx += 1;
            if in_tx >= 500 || last.elapsed() >= Duration::from_millis(500) {
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
        params![fs, outcome.hashed as i64, outcome.media as i64, outcome.cancelled],
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
