//! Merge a finished staging DB into the main catalog.
//!
//! One transaction: insert the new tree with ids offset above the current maximum,
//! carry hashes and media metadata over to unchanged files, delete the previous
//! scan's rows (and their FTS entries), index the new names, update the bookkeeping.
//! Readers keep their WAL snapshot throughout, so search never sees a half-merge.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use di_core::{Catalog, now_secs};

use crate::staging::Totals;

/// Per-step timings on stderr when `DRIVE_INDEX_TIMING` is set (development aid).
pub(crate) struct StepTimer {
    enabled: bool,
    last: std::time::Instant,
    scope: &'static str,
}

impl StepTimer {
    pub(crate) fn new(scope: &'static str) -> Self {
        StepTimer { enabled: std::env::var_os("DRIVE_INDEX_TIMING").is_some(), last: std::time::Instant::now(), scope }
    }
    pub(crate) fn step(&mut self, what: &str) {
        if self.enabled {
            let now = std::time::Instant::now();
            eprintln!("[timing] {}: {what}: {:.0} ms", self.scope, (now - self.last).as_secs_f64() * 1000.0);
            self.last = now;
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MergeStats {
    pub files_inserted: u64,
    pub dirs_inserted: u64,
    pub old_files_deleted: u64,
    pub hashes_carried: u64,
    pub media_carried: u64,
}

pub fn merge(
    catalog: &mut Catalog,
    staging_path: &Path,
    scan_id: i64,
    filesystem_id: i64,
    totals: &Totals,
) -> Result<MergeStats> {
    let conn = catalog.conn_mut();
    let path = staging_path.to_str().context("staging path is not UTF-8")?;
    // The merge keeps references consistent by construction (verified by the
    // end-to-end tests with `pragma_foreign_key_check`); per-row FK checks would
    // roughly double the cost of inserting and deleting hundreds of thousands of rows.
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    conn.execute("ATTACH DATABASE ?1 AS st", [path])?;
    let result = merge_attached(conn, scan_id, filesystem_id, totals);
    let detach = conn.execute("DETACH DATABASE st", []);
    let fk_on = conn.execute_batch("PRAGMA foreign_keys = ON");
    let stats = result?;
    detach?;
    fk_on?;
    Ok(stats)
}

fn merge_attached(conn: &mut Connection, scan_id: i64, fs: i64, totals: &Totals) -> Result<MergeStats> {
    let mut timer = StepTimer::new("merge");
    let tx = conn.transaction()?;
    let dir_base: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) FROM dirs", [], |r| r.get(0))?;
    let file_base: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) FROM files", [], |r| r.get(0))?;
    let dirs_inserted = tx.execute(
        "INSERT INTO dirs (id, filesystem_id, parent_id, name, name_raw, path, depth, mtime_ns, btime_ns, mode,
                           uid, gid, inode, flags, entry_count, tree_files, tree_dirs, tree_bytes, tree_alloc_bytes)
         SELECT id + ?1, ?2, parent_id + ?1, name, name_raw, path, depth, mtime_ns, btime_ns, mode,
                uid, gid, inode, flags, entry_count, tree_files, tree_dirs, tree_bytes, tree_alloc_bytes
         FROM st.dirs ORDER BY id",
        params![dir_base, fs],
    )? as u64;
    timer.step("insert dirs");

    let files_inserted = tx.execute(
        "INSERT INTO files (id, dir_id, filesystem_id, name, name_raw, ext, kind, size, alloc_size, mtime_ns,
                            ctime_ns, atime_ns, btime_ns, mode, uid, gid, inode, nlink, flags, link_target)
         SELECT id + ?1, dir_id + ?2, ?3, name, name_raw, ext, kind, size, alloc_size, mtime_ns,
                ctime_ns, atime_ns, btime_ns, mode, uid, gid, inode, nlink, flags, link_target
         FROM st.files ORDER BY id",
        params![file_base, dir_base, fs],
    )? as u64;
    timer.step("insert files");
    let mut stats = MergeStats { dirs_inserted, files_inserted, ..MergeStats::default() };

    // Old rows of this filesystem are exactly those with ids at or below the bases.
    let old_files = "SELECT id FROM files WHERE filesystem_id = ?1 AND id <= ?2";

    // Carry content-derived data to unchanged files: same dir path, name, size, mtime.
    let carry_join = "FROM files n
        JOIN dirs nd ON nd.id = n.dir_id
        JOIN dirs od ON od.filesystem_id = ?1 AND od.path = nd.path AND od.id <= ?3
        JOIN files o ON o.dir_id = od.id AND o.name = n.name AND o.size = n.size AND o.mtime_ns IS n.mtime_ns";
    // Driven from the (usually small) child table, not from the old file ids.
    let has = |table: &str| -> Result<bool> {
        Ok(tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} t JOIN files f ON f.id = t.file_id
                               WHERE f.filesystem_id = ?1 AND f.id <= ?2)"
            ),
            params![fs, file_base],
            |r| r.get(0),
        )?)
    };
    let (has_hashes, has_media, has_streams) = (has("file_hashes")?, has("media")?, has("media_streams")?);
    if has_hashes {
        stats.hashes_carried = tx.execute(
            &format!(
                "INSERT INTO file_hashes (file_id, quick, full, hashed_at)
                 SELECT n.id, h.quick, h.full, h.hashed_at {carry_join}
                 JOIN file_hashes h ON h.file_id = o.id
                 WHERE n.filesystem_id = ?1 AND n.id > ?2"
            ),
            params![fs, file_base, dir_base],
        )? as u64;
    }
    if has_media {
        stats.media_carried = tx.execute(
            &format!(
                "INSERT INTO media SELECT n.id, m.kind, m.status, m.extractor, m.extracted_at, m.captured_at,
                        m.captured_offset_min, m.captured_offset_inferred, m.width, m.height, m.duration_ms,
                        m.gps_lat, m.gps_lon, m.gps_alt_m, m.camera_make, m.camera_model, m.camera_serial,
                        m.lens_model, m.orientation, m.iso, m.exposure_s, m.f_number, m.focal_mm, m.container,
                        m.bitrate_bps, m.title, m.artist, m.album, m.album_artist, m.track_no, m.disc_no, m.year,
                        m.genre, m.tags
                 {carry_join} JOIN media m ON m.file_id = o.id
                 WHERE n.filesystem_id = ?1 AND n.id > ?2"
            ),
            params![fs, file_base, dir_base],
        )? as u64;
    }
    if has_streams {
        tx.execute(
            &format!(
                "INSERT INTO media_streams SELECT n.id, s.idx, s.type, s.codec, s.profile, s.width, s.height, s.fps,
                        s.bitrate_bps, s.sample_rate, s.channels, s.bit_depth, s.language, s.duration_ms
                 {carry_join} JOIN media_streams s ON s.file_id = o.id
                 WHERE n.filesystem_id = ?1 AND n.id > ?2"
            ),
            params![fs, file_base, dir_base],
        )?;
    }

    timer.step("carry over");
    // Remove the previous scan's rows.
    tx.execute(
        "INSERT INTO files_fts (files_fts, rowid, name)
         SELECT 'delete', id, name FROM files WHERE filesystem_id = ?1 AND id <= ?2",
        params![fs, file_base],
    )?;
    for (table, present) in [("file_hashes", has_hashes), ("media", has_media), ("media_streams", has_streams)] {
        if present {
            tx.execute(&format!("DELETE FROM {table} WHERE file_id IN ({old_files})"), params![fs, file_base])?;
        }
    }
    stats.old_files_deleted =
        tx.execute("DELETE FROM files WHERE filesystem_id = ?1 AND id <= ?2", params![fs, file_base])? as u64;
    tx.execute("DELETE FROM dirs WHERE filesystem_id = ?1 AND id <= ?2", params![fs, dir_base])?;
    timer.step("delete old");

    tx.execute(
        "INSERT INTO files_fts (rowid, name) SELECT id, name FROM files WHERE filesystem_id = ?1",
        [fs],
    )?;
    timer.step("fts insert");
    tx.execute(
        "INSERT INTO scan_errors (scan_id, path, op, errno, message)
         SELECT ?1, path, op, errno, message FROM st.scan_errors",
        [scan_id],
    )?;

    let now = now_secs();
    tx.execute(
        "UPDATE scans SET status = 'superseded' WHERE filesystem_id = ?1 AND status = 'complete' AND id != ?2",
        params![fs, scan_id],
    )?;
    tx.execute(
        "UPDATE scans SET status = 'complete', finished_at = ?2, files = ?3, dirs = ?4, symlinks = ?5,
                          bytes = ?6, alloc_bytes = ?7, errors = ?8
         WHERE id = ?1",
        params![
            scan_id,
            now,
            totals.files as i64,
            totals.dirs as i64,
            totals.symlinks as i64,
            totals.bytes as i64,
            totals.alloc_bytes as i64,
            totals.errors as i64
        ],
    )?;
    tx.execute(
        "UPDATE filesystems SET current_scan_id = ?2, file_count = ?3, dir_count = ?4, file_bytes = ?5
         WHERE id = ?1",
        params![fs, scan_id, totals.files as i64, totals.dirs as i64, totals.bytes as i64],
    )?;
    tx.execute(
        "UPDATE drives SET last_scanned_at = ?2 WHERE id = (SELECT drive_id FROM filesystems WHERE id = ?1)",
        params![fs, now],
    )?;
    tx.commit()?;
    timer.step("bookkeeping + commit");
    Ok(stats)
}
