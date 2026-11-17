//! SMART snapshots: one row per reading, plus whether SMART works
//! for the drive at all (`drives.smart_*`).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use di_model::smart::{SmartReport, SmartUnavailable};

use crate::catalog::{Catalog, now_secs};

#[derive(Debug, Clone, PartialEq)]
pub struct SmartView {
    pub taken_at: i64,
    pub report: SmartReport,
}

/// Whether SMART could be read the last time it was tried.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartAccess {
    pub available: Option<bool>,
    pub reason: Option<String>,
    pub device_type: Option<String>,
}

impl Catalog {
    /// Record a SMART reading (or why there is none) for a drive.
    pub fn save_smart(&self, drive_id: i64, r: &Result<SmartReport, SmartUnavailable>) -> Result<()> {
        let conn = self.conn();
        match r {
            Ok(s) => {
                conn.execute(
                    "UPDATE drives SET smart_available = 1, smart_unavailable_reason = NULL, smart_device_type = ?2 WHERE id = ?1",
                    params![drive_id, s.device_type],
                )?;
                conn.execute(
                    "INSERT INTO smart_snapshots (drive_id, taken_at, passed, temperature_c, power_on_hours, power_cycles,
                        reallocated_sectors, pending_sectors, offline_uncorrectable, udma_crc_errors,
                        nvme_percentage_used, nvme_available_spare, nvme_media_errors, nvme_unsafe_shutdowns,
                        data_written_bytes, raw_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    params![
                        drive_id,
                        now_secs(),
                        s.passed,
                        s.temperature_c,
                        s.power_on_hours,
                        s.power_cycles,
                        s.reallocated_sectors,
                        s.pending_sectors,
                        s.offline_uncorrectable,
                        s.udma_crc_errors,
                        s.nvme_percentage_used,
                        s.nvme_available_spare,
                        s.nvme_media_errors,
                        s.nvme_unsafe_shutdowns,
                        s.data_written_bytes,
                        s.raw_json
                    ],
                )?;
            }
            Err(e) => {
                // Not installed / needs root say nothing about the drive: keep an earlier "available".
                let drive_level = matches!(e, SmartUnavailable::Unsupported(_) | SmartUnavailable::Failed(_));
                conn.execute(
                    "UPDATE drives SET smart_available = CASE WHEN ?3 THEN 0 ELSE smart_available END,
                        smart_unavailable_reason = ?2 WHERE id = ?1",
                    params![drive_id, e.reason(), drive_level],
                )?;
            }
        }
        Ok(())
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<SmartView> {
    let raw_json = r.get::<_, Option<String>>(14)?.unwrap_or_default();
    // See Note [SMART details from smartctl's JSON] in di-model.
    let (health_source, failing_attributes) = di_model::smart::details_of_raw(&raw_json);
    Ok(SmartView {
        taken_at: r.get(0)?,
        report: SmartReport {
            passed: r.get(1)?,
            temperature_c: r.get(2)?,
            power_on_hours: r.get(3)?,
            power_cycles: r.get(4)?,
            reallocated_sectors: r.get(5)?,
            pending_sectors: r.get(6)?,
            offline_uncorrectable: r.get(7)?,
            udma_crc_errors: r.get(8)?,
            nvme_percentage_used: r.get(9)?,
            nvme_available_spare: r.get(10)?,
            nvme_media_errors: r.get(11)?,
            nvme_unsafe_shutdowns: r.get(12)?,
            data_written_bytes: r.get(13)?,
            raw_json,
            health_source,
            failing_attributes,
            ..Default::default()
        },
    })
}

const COLUMNS: &str = "taken_at, passed, temperature_c, power_on_hours, power_cycles, reallocated_sectors, pending_sectors,
     offline_uncorrectable, udma_crc_errors, nvme_percentage_used, nvme_available_spare, nvme_media_errors,
     nvme_unsafe_shutdowns, data_written_bytes, raw_json";

/// Readings for a drive, newest first.
pub fn history(conn: &Connection, drive_id: i64, limit: usize) -> Result<Vec<SmartView>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {COLUMNS} FROM smart_snapshots WHERE drive_id = ?1 ORDER BY taken_at DESC, id DESC LIMIT ?2"))?;
    Ok(stmt.query_map(params![drive_id, limit as i64], row)?.collect::<rusqlite::Result<_>>()?)
}

pub fn latest(conn: &Connection, drive_id: i64) -> Result<Option<SmartView>> {
    Ok(history(conn, drive_id, 1)?.into_iter().next())
}

pub fn access(conn: &Connection, drive_id: i64) -> Result<SmartAccess> {
    Ok(conn
        .query_row(
            "SELECT smart_available, smart_unavailable_reason, smart_device_type FROM drives WHERE id = ?1",
            [drive_id],
            |r| Ok(SmartAccess { available: r.get(0)?, reason: r.get(1)?, device_type: r.get(2)? }),
        )
        .optional()?
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirSource, FsUsage};

    #[test]
    fn snapshots_and_access() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();
        let src = DirSource { host: "h".into(), path: "/x".into(), label: None, fs: FsUsage { fs_type: "apfs".into(), ..Default::default() } };
        let id = cat.upsert_dir_source(&src).unwrap().drive_id;
        assert_eq!(latest(cat.conn(), id).unwrap(), None);

        let r = SmartReport { device_type: "sat".into(), passed: Some(true), pending_sectors: Some(8), raw_json: "{}".into(), ..Default::default() };
        cat.save_smart(id, &Ok(r)).unwrap();
        let a = access(cat.conn(), id).unwrap();
        assert_eq!((a.available, a.device_type.as_deref(), a.reason), (Some(true), Some("sat"), None));
        let l = latest(cat.conn(), id).unwrap().unwrap();
        assert_eq!((l.report.passed, l.report.pending_sectors), (Some(true), Some(8)));

        // Missing permissions say nothing about the drive: it stays "available".
        cat.save_smart(id, &Err(SmartUnavailable::NeedsRoot("Permission denied".into()))).unwrap();
        let a = access(cat.conn(), id).unwrap();
        assert_eq!(a.available, Some(true));
        assert!(a.reason.unwrap().contains("needs root"));
        cat.save_smart(id, &Err(SmartUnavailable::Unsupported("SMART is not available over USB on macOS".into()))).unwrap();
        assert_eq!(access(cat.conn(), id).unwrap().available, Some(false));
        assert_eq!(history(cat.conn(), id, 10).unwrap().len(), 1);
    }
}
