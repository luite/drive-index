//! The main catalog database: opening, migrations and bookkeeping operations.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::seed;
pub use di_model::FsUsage;

pub const SCHEMA_VERSION: i64 = 3;

pub struct Catalog {
    conn: Connection,
    path: PathBuf,
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Catalog {
    /// Open (creating if needed) the catalog read-write and bring the schema up to date.
    pub fn open(path: &Path) -> Result<Catalog> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating catalog directory {}", dir.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening catalog {}", path.display()))?;
        configure(&conn)?;
        migrate(&conn)?;
        Ok(Catalog { conn, path: path.to_path_buf() })
    }

    /// Open an existing catalog for queries only.
    pub fn open_readonly(path: &Path) -> Result<Catalog> {
        if !path.exists() {
            bail!("no catalog at {} (scan something first)", path.display());
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening catalog {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-131072;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "catalog {} has schema version {version}, newer than this build ({SCHEMA_VERSION}); upgrade drive-index",
                path.display()
            );
        }
        if version < SCHEMA_VERSION {
            // Migrate once through a read-write connection, then reopen read-only.
            drop(conn);
            drop(Catalog::open(path)?);
            return Catalog::open_readonly(path);
        }
        Ok(Catalog { conn, path: path.to_path_buf() })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Directory holding per-scan staging databases.
    pub fn staging_dir(&self) -> PathBuf {
        self.path.parent().unwrap_or(Path::new(".")).join("staging")
    }

    pub fn catalog_id(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'catalog_id'", [], |r| r.get(0))?)
    }

    // ---------------------------------------------------------------------------------
    // Directory sources ("virtual drives": any directory scanned with `scan <DIR>`)
    // ---------------------------------------------------------------------------------

    /// Find or create the virtual drive + filesystem for a scanned directory.
    pub fn upsert_dir_source(&self, src: &DirSource) -> Result<SourceIds> {
        let now = now_secs();
        let ident = format!("{}:{}", src.host, src.path);
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT drive_id FROM drive_identifiers WHERE kind = 'dir_path' AND value = ?1",
                [&ident],
                |r| r.get(0),
            )
            .optional()?;

        let fs = &src.fs;
        let drive_id = match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE drives SET last_seen_at = ?2, connect_count = connect_count + 1,
                            capacity_bytes = COALESCE(?3, capacity_bytes)
                     WHERE id = ?1",
                    params![id, now, fs.size_bytes.map(|v| v as i64)],
                )?;
                self.conn.execute(
                    "UPDATE drive_identifiers SET last_seen_at = ?2 WHERE drive_id = ?1 AND kind = 'dir_path'",
                    params![id, now],
                )?;
                id
            }
            None => {
                let default_label = Path::new(&src.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| src.path.clone());
                self.conn.execute(
                    "INSERT INTO drives (fingerprint, label, bus, capacity_bytes, medium,
                                         first_seen_at, last_seen_at, connect_count)
                     VALUES (?1, ?2, 'virtual', ?3, 'unknown', ?4, ?4, 1)",
                    params![
                        format!("dir:{ident}"),
                        src.label.clone().unwrap_or(default_label),
                        fs.size_bytes.unwrap_or(0) as i64,
                        now
                    ],
                )?;
                let id = self.conn.last_insert_rowid();
                self.conn.execute(
                    "INSERT INTO drive_identifiers (drive_id, kind, value, strength, first_seen_at, last_seen_at)
                     VALUES (?1, 'dir_path', ?2, 100, ?3, ?3)",
                    params![id, ident, now],
                )?;
                id
            }
        };
        if existing.is_some() {
            if let Some(label) = &src.label {
                self.conn.execute("UPDATE drives SET label = ?2 WHERE id = ?1", params![drive_id, label])?;
            }
        }

        let fs_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM filesystems WHERE drive_id = ?1 ORDER BY id LIMIT 1",
                [drive_id],
                |r| r.get(0),
            )
            .optional()?;
        let as_i64 = |v: Option<u64>| v.map(|x| x as i64);
        let fs_id = match fs_id {
            Some(id) => {
                self.conn.execute(
                    "UPDATE filesystems SET fs_type = ?2, size_bytes = ?3, used_bytes = ?4, free_bytes = ?5,
                            block_size = ?6, inodes_total = ?7, inodes_used = ?8, last_mount_point = ?9,
                            last_seen_at = ?10
                     WHERE id = ?1",
                    params![
                        id,
                        fs.fs_type,
                        as_i64(fs.size_bytes),
                        as_i64(fs.used_bytes),
                        as_i64(fs.free_bytes),
                        as_i64(fs.block_size),
                        as_i64(fs.inodes_total),
                        as_i64(fs.inodes_used),
                        src.path,
                        now
                    ],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO filesystems (drive_id, fs_type, size_bytes, used_bytes, free_bytes, block_size,
                                              inodes_total, inodes_used, last_mount_point, first_seen_at, last_seen_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                    params![
                        drive_id,
                        fs.fs_type,
                        as_i64(fs.size_bytes),
                        as_i64(fs.used_bytes),
                        as_i64(fs.free_bytes),
                        as_i64(fs.block_size),
                        as_i64(fs.inodes_total),
                        as_i64(fs.inodes_used),
                        src.path,
                        now
                    ],
                )?;
                self.conn.last_insert_rowid()
            }
        };
        Ok(SourceIds { drive_id, filesystem_id: fs_id })
    }

    // ---------------------------------------------------------------------------------
    // Scans
    // ---------------------------------------------------------------------------------

    pub fn create_scan(&self, new: &NewScan) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO scans (filesystem_id, status, method, started_at, host, host_os, host_tz,
                                app_version, mount_point, threads_final, options)
             VALUES (?1, 'running', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                new.filesystem_id,
                new.method,
                now_secs(),
                new.host,
                std::env::consts::OS,
                new.host_tz,
                env!("CARGO_PKG_VERSION"),
                new.mount_point,
                new.threads as i64,
                new.options_json
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The most recent unfinished scan of a filesystem, if any. A scan still marked
    /// `running` was cut short by a crash and is resumable too.
    pub fn resumable_scan(&self, filesystem_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM scans WHERE filesystem_id = ?1
                   AND status IN ('running', 'paused', 'interrupted')
                 ORDER BY id DESC LIMIT 1",
                [filesystem_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_scan_status(&self, scan_id: i64, status: &str) -> Result<()> {
        let finished = matches!(status, "complete" | "failed" | "aborted").then(now_secs);
        self.conn.execute(
            "UPDATE scans SET status = ?2, finished_at = COALESCE(?3, finished_at) WHERE id = ?1",
            params![scan_id, status, finished],
        )?;
        Ok(())
    }

    pub fn add_scan_active_ms(&self, scan_id: i64, ms: u64, threads: usize) -> Result<()> {
        self.conn.execute(
            "UPDATE scans SET active_ms = COALESCE(active_ms, 0) + ?2, threads_final = ?3 WHERE id = ?1",
            params![scan_id, ms as i64, threads as i64],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------
    // Listing & labels
    // ---------------------------------------------------------------------------------

    pub fn drives(&self) -> Result<Vec<DriveSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.id, d.label, d.fingerprint, d.bus, d.capacity_bytes, d.last_seen_at, d.last_scanned_at,
                    count(fs.id), COALESCE(sum(fs.file_count), 0), COALESCE(sum(fs.file_bytes), 0)
             FROM drives d LEFT JOIN filesystems fs ON fs.drive_id = d.id
             GROUP BY d.id ORDER BY d.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DriveSummary {
                id: r.get(0)?,
                label: r.get(1)?,
                fingerprint: r.get(2)?,
                bus: r.get(3)?,
                capacity_bytes: r.get::<_, i64>(4)? as u64,
                last_seen_at: r.get(5)?,
                last_scanned_at: r.get(6)?,
                filesystems: r.get::<_, i64>(7)? as u64,
                files: r.get::<_, i64>(8)? as u64,
                bytes: r.get::<_, i64>(9)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn filesystems(&self) -> Result<Vec<FsSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT fs.id, fs.drive_id,
                    (SELECT count(*) FROM filesystems x WHERE x.drive_id = fs.drive_id AND x.id <= fs.id),
                    fs.fs_type, COALESCE(fs.user_label, fs.volume_label), fs.fs_uuid, fs.size_bytes,
                    fs.used_bytes, fs.last_mount_point, fs.file_count, fs.dir_count, fs.file_bytes,
                    s.finished_at
             FROM filesystems fs LEFT JOIN scans s ON s.id = fs.current_scan_id
             ORDER BY fs.drive_id, fs.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FsSummary {
                id: r.get(0)?,
                drive_id: r.get(1)?,
                ordinal: r.get(2)?,
                fs_type: r.get(3)?,
                label: r.get(4)?,
                fs_uuid: r.get(5)?,
                size_bytes: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                used_bytes: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                mount_point: r.get(8)?,
                files: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                dirs: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                bytes: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                scanned_at: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn scans(&self, limit: usize) -> Result<Vec<ScanSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.filesystem_id, fs.drive_id, s.status, s.method, s.started_at, s.finished_at,
                    s.active_ms, s.threads_final, s.files, s.dirs, s.bytes, s.errors, s.host
             FROM scans s JOIN filesystems fs ON fs.id = s.filesystem_id
             ORDER BY s.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(ScanSummary {
                id: r.get(0)?,
                filesystem_id: r.get(1)?,
                drive_id: r.get(2)?,
                status: r.get(3)?,
                method: r.get(4)?,
                started_at: r.get(5)?,
                finished_at: r.get(6)?,
                active_ms: r.get(7)?,
                threads: r.get(8)?,
                files: r.get::<_, i64>(9)? as u64,
                dirs: r.get::<_, i64>(10)? as u64,
                bytes: r.get::<_, i64>(11)? as u64,
                errors: r.get::<_, i64>(12)? as u64,
                host: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Set or clear (`None`) a drive's notes. Returns false if no such drive.
    pub fn set_drive_notes(&self, drive_id: i64, notes: Option<&str>) -> Result<bool> {
        Ok(self.conn.execute("UPDATE drives SET notes = ?2 WHERE id = ?1", params![drive_id, notes])? == 1)
    }

    /// Append to the persistent event log (shown on the Log screen).
    pub fn log_event(&self, level: &str, drive_id: Option<i64>, kind: &str, message: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (ts, level, drive_id, kind, message) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now_secs(), level, drive_id, kind, message],
        )?;
        Ok(())
    }

    /// Most recent events, newest first.
    pub fn recent_events(&self, limit: usize) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare("SELECT ts, level, drive_id, message FROM events ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(EventRow { ts: r.get(0)?, level: r.get(1)?, drive_id: r.get(2)?, message: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Set or clear (`None`) a drive's user label. Returns false if no such drive.
    pub fn set_drive_label(&self, drive_id: i64, label: Option<&str>) -> Result<bool> {
        Ok(self.conn.execute("UPDATE drives SET label = ?2 WHERE id = ?1", params![drive_id, label])? == 1)
    }
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -262144;
         PRAGMA mmap_size = 1073741824;",
    )?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("catalog schema version {version} is newer than this build ({SCHEMA_VERSION}); upgrade drive-index");
    }
    if version == 0 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(include_str!("schema_v1.sql")).context("creating schema v1")?;
        {
            let mut ins = tx.prepare("INSERT INTO ext_categories (ext, category) VALUES (?1, ?2)")?;
            for (category, exts) in seed::EXT_CATEGORIES {
                for ext in *exts {
                    ins.execute(params![ext, category])?;
                }
            }
            let mut ins = tx.prepare("INSERT INTO collections (slug, name, query) VALUES (?1, ?2, ?3)")?;
            for (slug, name, query) in seed::COLLECTIONS {
                ins.execute(params![slug, name, query])?;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('catalog_id', ?1), ('created_at', ?2)",
            params![random_id(), now_secs().to_string()],
        )?;
        tx.execute_batch("PRAGMA user_version = 1;")?;
        tx.commit()?;
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 2 {
        // v2: session process ids (to tell crashed sessions from live ones) and the
        // last scan id when the session began (to count the session's own scans).
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "ALTER TABLE sessions ADD COLUMN pid INTEGER;
             ALTER TABLE sessions ADD COLUMN first_scan_id INTEGER;
             PRAGMA user_version = 2;",
        )?;
        tx.commit()?;
    }
    if version < 3 {
        // v3: the disk's own model, serial and WWN, which rule out other drives. See
        // Note [Disk facts] in di-model:identity. Filled in from what earlier versions
        // stored only where it certainly came from the disk: a Linux probe that read ATA
        // identify data (also through a USB bridge), a disk that is not behind a USB
        // bridge (Linux SD cards: mmc, which has udev data), or an ATA/NVMe SMART reading.
        // The rest is filled in when the drive is next seen.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "ALTER TABLE drives ADD COLUMN disk_model TEXT;
             ALTER TABLE drives ADD COLUMN disk_serial TEXT;
             ALTER TABLE drives ADD COLUMN disk_wwn TEXT;
             UPDATE drives SET disk_model = model, disk_serial = serial, disk_wwn = wwn
              WHERE bus IN ('sata', 'nvme', 'thunderbolt')
                 OR (json_valid(raw_info) AND json_extract(raw_info, '$.udev.ID_BUS') = 'ata')
                 OR (bus = 'sd' AND json_valid(raw_info) AND json_type(raw_info, '$.udev') IS NOT NULL);
             CREATE TEMP TABLE smart_ids AS
               SELECT drive_id, json_extract(raw_json, '$.model_name') AS model,
                      json_extract(raw_json, '$.serial_number') AS serial
                 FROM smart_snapshots s
                WHERE json_valid(raw_json) AND json_extract(raw_json, '$.device.protocol') IN ('ATA', 'NVMe')
                  AND taken_at = (SELECT max(taken_at) FROM smart_snapshots t WHERE t.drive_id = s.drive_id);
             UPDATE drives SET
                disk_model = COALESCE(disk_model, (SELECT model FROM smart_ids WHERE smart_ids.drive_id = drives.id)),
                disk_serial = COALESCE(disk_serial, (SELECT serial FROM smart_ids WHERE smart_ids.drive_id = drives.id));
             DROP TABLE smart_ids;
             PRAGMA user_version = 3;",
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// 128-bit random hex id.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    }
    if bytes == [0u8; 16] {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        bytes = (t ^ (std::process::id() as u128) << 64).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A directory scanned as a "virtual drive".
#[derive(Debug, Clone)]
pub struct DirSource {
    pub host: String,
    /// Canonical absolute path.
    pub path: String,
    pub label: Option<String>,
    pub fs: FsUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceIds {
    pub drive_id: i64,
    pub filesystem_id: i64,
}

#[derive(Debug, Clone)]
pub struct NewScan {
    pub filesystem_id: i64,
    pub method: String,
    pub host: String,
    pub host_tz: Option<String>,
    pub mount_point: String,
    pub threads: usize,
    pub options_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub ts: i64,
    pub level: String,
    pub drive_id: Option<i64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveSummary {
    pub id: i64,
    pub label: Option<String>,
    pub fingerprint: String,
    pub bus: Option<String>,
    pub capacity_bytes: u64,
    pub last_seen_at: i64,
    pub last_scanned_at: Option<i64>,
    pub filesystems: u64,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsSummary {
    pub id: i64,
    pub drive_id: i64,
    /// Position within the drive: the `n` in short id `D<drive>.<n>`.
    pub ordinal: i64,
    pub fs_type: String,
    pub label: Option<String>,
    pub fs_uuid: Option<String>,
    pub size_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub mount_point: Option<String>,
    pub files: Option<u64>,
    pub dirs: Option<u64>,
    pub bytes: Option<u64>,
    pub scanned_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanSummary {
    pub id: i64,
    pub filesystem_id: i64,
    pub drive_id: i64,
    pub status: String,
    pub method: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub active_ms: Option<i64>,
    pub threads: Option<i64>,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub host: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/index.db");
        let id = {
            let cat = Catalog::open(&path).unwrap();
            let n: i64 = cat.conn().query_row("SELECT count(*) FROM ext_categories", [], |r| r.get(0)).unwrap();
            assert!(n > 50);
            cat.catalog_id().unwrap()
        };
        let cat = Catalog::open(&path).unwrap();
        assert_eq!(cat.catalog_id().unwrap(), id, "reopen must not re-run migration");
        let ro = Catalog::open_readonly(&path).unwrap();
        assert_eq!(ro.catalog_id().unwrap(), id);
    }

    #[test]
    fn dir_source_upsert_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&tmp.path().join("index.db")).unwrap();
        let src = DirSource {
            host: "h".into(),
            path: "/data/photos".into(),
            label: None,
            fs: FsUsage { fs_type: "apfs".into(), size_bytes: Some(1000), ..Default::default() },
        };
        let a = cat.upsert_dir_source(&src).unwrap();
        let b = cat.upsert_dir_source(&DirSource { label: Some("Photos".into()), ..src.clone() }).unwrap();
        assert_eq!(a, b);
        let drives = cat.drives().unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].label.as_deref(), Some("Photos"));
        assert!(cat.set_drive_label(a.drive_id, Some("Renamed")).unwrap());
        assert!(!cat.set_drive_label(999, Some("x")).unwrap());
    }
}
