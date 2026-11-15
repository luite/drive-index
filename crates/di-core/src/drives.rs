//! Physical drives in the catalog: identity matching (PLAN.md §3.3) and records for
//! drives, identifiers, partitions and filesystems.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use di_model::FsUsage;
use di_model::identity::{Identifier, MATCH_THRESHOLD, score};

use crate::catalog::{Catalog, now_secs};

/// Platform-neutral description of a drive, built from a device probe.
#[derive(Debug, Clone, Default)]
pub struct DriveRecord {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub wwn: Option<String>,
    pub firmware: Option<String>,
    pub bus: String,
    pub transport_driver: Option<String>,
    pub usb_vendor_id: Option<u16>,
    pub usb_product_id: Option<u16>,
    pub usb_serial: Option<String>,
    pub usb_manufacturer: Option<String>,
    pub usb_product: Option<String>,
    pub usb_speed_mbps: Option<u32>,
    pub capacity_bytes: u64,
    pub logical_block_size: Option<u32>,
    pub physical_block_size: Option<u32>,
    pub medium: String,
    pub rotation_rpm: Option<u32>,
    pub removable: bool,
    pub ptable_type: Option<String>,
    pub ptable_uuid: Option<String>,
    pub raw_info: Option<String>,
    pub identifiers: Vec<Identifier>,
    pub partitions: Vec<PartitionRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct PartitionRecord {
    pub number: u32,
    pub scheme: String,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub type_id: Option<String>,
    pub type_name: Option<String>,
    pub part_uuid: Option<String>,
    pub part_label: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FsRecord {
    pub partition_number: Option<u32>,
    pub container_uuid: Option<String>,
    pub fs_type: String,
    pub fs_uuid: Option<String>,
    pub volume_label: Option<String>,
    pub size_bytes: Option<u64>,
    pub encrypted: Option<bool>,
    pub locked: Option<bool>,
    pub apfs_role: Option<String>,
    pub unscannable_reason: Option<String>,
    pub mount_point: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveMatch {
    /// Recognised. `weak` when only low-strength identifiers matched (plus capacity).
    Known { drive_id: i64, score: i64, weak: bool },
    New,
    /// Several candidates or an unconvincing one: the user must decide.
    Ambiguous(Vec<(i64, i64)>),
}

fn i64opt<T: Into<i64>>(v: Option<T>) -> Option<i64> {
    v.map(Into::into)
}

impl Catalog {
    pub fn match_drive(&self, rec: &DriveRecord) -> Result<DriveMatch> {
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT drive_id FROM drive_identifiers WHERE kind = ?1 AND value = ?2")?;
        let mut hits = Vec::new();
        for id in &rec.identifiers {
            let drives: Vec<i64> =
                stmt.query_map(params![id.kind, id.value], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            hits.extend(drives.into_iter().map(|d| (d, id)));
        }
        let scores = score(hits);
        let Some(&(top, top_score)) = scores.first() else { return Ok(DriveMatch::New) };
        let runner_up = scores.get(1).map(|s| s.1).unwrap_or(0);
        if top_score >= MATCH_THRESHOLD && runner_up < MATCH_THRESHOLD / 2 {
            return Ok(DriveMatch::Known { drive_id: top, score: top_score, weak: false });
        }
        if scores.len() == 1 {
            let capacity: Option<i64> = self
                .conn()
                .query_row("SELECT capacity_bytes FROM drives WHERE id = ?1", [top], |r| r.get(0))
                .optional()?;
            if capacity == Some(rec.capacity_bytes as i64) {
                return Ok(DriveMatch::Known { drive_id: top, score: top_score, weak: true });
            }
        }
        Ok(DriveMatch::Ambiguous(scores))
    }

    /// Insert or update a drive and its identifiers and partitions. `label` sets the
    /// user label (an existing label is kept when `None`).
    pub fn save_drive(&mut self, rec: &DriveRecord, existing: Option<i64>, label: Option<&str>) -> Result<i64> {
        let now = now_secs();
        let fingerprint = rec
            .identifiers
            .iter()
            .max_by_key(|i| i.strength)
            .map(|i| format!("{}:{}", i.kind, i.value))
            .unwrap_or_else(|| format!("unidentified:{}", crate::catalog::random_id()));
        let tx = self.conn_mut().transaction()?;
        let drive_id = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE drives SET fingerprint = ?2, vendor = ?3, model = ?4, serial = COALESCE(?5, serial),
                        wwn = COALESCE(?6, wwn), firmware = COALESCE(?7, firmware), bus = ?8,
                        transport_driver = ?9, usb_vendor_id = ?10, usb_product_id = ?11, usb_serial = ?12,
                        usb_manufacturer = ?13, usb_product = ?14, usb_speed_mbps = ?15, capacity_bytes = ?16,
                        logical_block_size = ?17, physical_block_size = ?18, medium = ?19, rotation_rpm = ?20,
                        removable = ?21, ptable_type = ?22, ptable_uuid = ?23, raw_info = ?24,
                        last_seen_at = ?25, connect_count = connect_count + 1, label = COALESCE(?26, label)
                     WHERE id = ?1",
                    params![
                        id,
                        fingerprint,
                        rec.vendor,
                        rec.model,
                        rec.serial,
                        rec.wwn,
                        rec.firmware,
                        rec.bus,
                        rec.transport_driver,
                        i64opt(rec.usb_vendor_id),
                        i64opt(rec.usb_product_id),
                        rec.usb_serial,
                        rec.usb_manufacturer,
                        rec.usb_product,
                        i64opt(rec.usb_speed_mbps),
                        rec.capacity_bytes as i64,
                        i64opt(rec.logical_block_size),
                        i64opt(rec.physical_block_size),
                        rec.medium,
                        i64opt(rec.rotation_rpm),
                        rec.removable,
                        rec.ptable_type,
                        rec.ptable_uuid,
                        rec.raw_info,
                        now,
                        label
                    ],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO drives (fingerprint, label, vendor, model, serial, wwn, firmware, bus,
                        transport_driver, usb_vendor_id, usb_product_id, usb_serial, usb_manufacturer, usb_product,
                        usb_speed_mbps, capacity_bytes, logical_block_size, physical_block_size, medium,
                        rotation_rpm, removable, ptable_type, ptable_uuid, raw_info,
                        first_seen_at, last_seen_at, connect_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                             ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?25, 1)",
                    params![
                        fingerprint,
                        label,
                        rec.vendor,
                        rec.model,
                        rec.serial,
                        rec.wwn,
                        rec.firmware,
                        rec.bus,
                        rec.transport_driver,
                        i64opt(rec.usb_vendor_id),
                        i64opt(rec.usb_product_id),
                        rec.usb_serial,
                        rec.usb_manufacturer,
                        rec.usb_product,
                        i64opt(rec.usb_speed_mbps),
                        rec.capacity_bytes as i64,
                        i64opt(rec.logical_block_size),
                        i64opt(rec.physical_block_size),
                        rec.medium,
                        i64opt(rec.rotation_rpm),
                        rec.removable,
                        rec.ptable_type,
                        rec.ptable_uuid,
                        rec.raw_info,
                        now
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };

        {
            let mut ins = tx.prepare(
                "INSERT INTO drive_identifiers (drive_id, kind, value, strength, first_seen_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT (kind, value, drive_id) DO UPDATE SET last_seen_at = excluded.last_seen_at",
            )?;
            for id in &rec.identifiers {
                ins.execute(params![drive_id, id.kind, id.value, id.strength, now])?;
            }
            let mut up = tx.prepare(
                "INSERT INTO partitions (drive_id, number, scheme, start_bytes, size_bytes, type_id, type_name,
                                         part_uuid, part_label)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (drive_id, number) DO UPDATE SET scheme = excluded.scheme,
                    start_bytes = excluded.start_bytes, size_bytes = excluded.size_bytes, type_id = excluded.type_id,
                    type_name = excluded.type_name, part_uuid = excluded.part_uuid, part_label = excluded.part_label",
            )?;
            for p in &rec.partitions {
                up.execute(params![
                    drive_id,
                    p.number,
                    p.scheme,
                    p.start_bytes as i64,
                    p.size_bytes as i64,
                    p.type_id,
                    p.type_name,
                    p.part_uuid,
                    p.part_label
                ])?;
            }
        }
        // Partitions that no longer exist (repartitioned drive).
        let keep: Vec<String> = rec.partitions.iter().map(|p| p.number.to_string()).collect();
        tx.execute(
            &format!(
                "DELETE FROM partitions WHERE drive_id = ?1 AND number NOT IN ({})",
                if keep.is_empty() { "-1".to_string() } else { keep.join(",") }
            ),
            [drive_id],
        )?;
        tx.commit()?;
        Ok(drive_id)
    }

    /// Insert or update one filesystem of a drive, matched by UUID, else by partition
    /// and type. Returns its id.
    pub fn save_filesystem(&self, drive_id: i64, fs: &FsRecord) -> Result<i64> {
        let now = now_secs();
        let partition_id: Option<i64> = match fs.partition_number {
            Some(n) => self
                .conn()
                .query_row(
                    "SELECT id FROM partitions WHERE drive_id = ?1 AND number = ?2",
                    params![drive_id, n],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        let existing: Option<i64> = match &fs.fs_uuid {
            Some(uuid) => self
                .conn()
                .query_row(
                    "SELECT id FROM filesystems WHERE drive_id = ?1 AND fs_uuid = ?2",
                    params![drive_id, uuid],
                    |r| r.get(0),
                )
                .optional()?,
            None => self
                .conn()
                .query_row(
                    "SELECT id FROM filesystems WHERE drive_id = ?1 AND fs_uuid IS NULL
                       AND partition_id IS ?2 AND fs_type = ?3",
                    params![drive_id, partition_id, fs.fs_type],
                    |r| r.get(0),
                )
                .optional()?,
        };
        let scannable = fs.unscannable_reason.is_none();
        match existing {
            Some(id) => {
                self.conn().execute(
                    "UPDATE filesystems SET partition_id = ?2, container_uuid = ?3, fs_type = ?4, volume_label = ?5,
                        size_bytes = COALESCE(?6, size_bytes), encrypted = ?7, locked = ?8, apfs_role = ?9,
                        scannable = ?10, unscannable_reason = ?11,
                        last_mount_point = COALESCE(?12, last_mount_point), last_seen_at = ?13
                     WHERE id = ?1",
                    params![
                        id,
                        partition_id,
                        fs.container_uuid,
                        fs.fs_type,
                        fs.volume_label,
                        fs.size_bytes.map(|v| v as i64),
                        fs.encrypted,
                        fs.locked,
                        fs.apfs_role,
                        scannable,
                        fs.unscannable_reason,
                        fs.mount_point,
                        now
                    ],
                )?;
                Ok(id)
            }
            None => {
                self.conn().execute(
                    "INSERT INTO filesystems (drive_id, partition_id, container_uuid, fs_type, fs_uuid, volume_label,
                        size_bytes, encrypted, locked, apfs_role, scannable, unscannable_reason, last_mount_point,
                        first_seen_at, last_seen_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                    params![
                        drive_id,
                        partition_id,
                        fs.container_uuid,
                        fs.fs_type,
                        fs.fs_uuid,
                        fs.volume_label,
                        fs.size_bytes.map(|v| v as i64),
                        fs.encrypted,
                        fs.locked,
                        fs.apfs_role,
                        scannable,
                        fs.unscannable_reason,
                        fs.mount_point,
                        now
                    ],
                )?;
                Ok(self.conn().last_insert_rowid())
            }
        }
    }

    /// Record statfs numbers for a mounted filesystem.
    pub fn update_fs_usage(&self, filesystem_id: i64, usage: &FsUsage, mount_point: &str) -> Result<()> {
        let v = |x: Option<u64>| x.map(|n| n as i64);
        self.conn().execute(
            "UPDATE filesystems SET size_bytes = COALESCE(?2, size_bytes), used_bytes = ?3, free_bytes = ?4,
                block_size = ?5, inodes_total = ?6, inodes_used = ?7, last_mount_point = ?8
             WHERE id = ?1",
            params![
                filesystem_id,
                v(usage.size_bytes),
                v(usage.used_bytes),
                v(usage.free_bytes),
                v(usage.block_size),
                v(usage.inodes_total),
                v(usage.inodes_used),
                mount_point
            ],
        )?;
        Ok(())
    }

    /// Unix seconds of the last completed scan of a filesystem.
    pub fn last_scanned(&self, filesystem_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT s.finished_at FROM filesystems fs JOIN scans s ON s.id = fs.current_scan_id WHERE fs.id = ?1",
                [filesystem_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn drive_label(&self, drive_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT label FROM drives WHERE id = ?1", [drive_id], |r| r.get(0))
            .optional()?
            .flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use di_model::identity::*;

    fn id(kind: &str, value: &str, strength: i64) -> Identifier {
        Identifier { kind: kind.into(), value: value.into(), strength }
    }

    fn record(ids: Vec<Identifier>, capacity: u64) -> DriveRecord {
        DriveRecord {
            bus: "usb".into(),
            medium: "hdd".into(),
            capacity_bytes: capacity,
            identifiers: ids,
            partitions: vec![PartitionRecord { number: 1, scheme: "gpt".into(), size_bytes: capacity, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn identity_matching() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open(&tmp.path().join("i.db")).unwrap();

        let a = record(vec![id("disk_serial", "WD|123", DISK_SERIAL), id("fs_uuid", "AAAA", FS_UUID)], 2_000);
        assert_eq!(cat.match_drive(&a).unwrap(), DriveMatch::New);
        let d1 = cat.save_drive(&a, None, Some("Photos")).unwrap();

        // Same disk in another enclosure: serial matches, USB serial is new.
        let a2 = record(vec![id("disk_serial", "WD|123", DISK_SERIAL), id("usb_serial", "174c:55aa:X", USB_SERIAL)], 2_000);
        assert!(matches!(cat.match_drive(&a2).unwrap(), DriveMatch::Known { drive_id, weak: false, .. } if drive_id == d1));
        cat.save_drive(&a2, Some(d1), None).unwrap();
        assert_eq!(cat.drive_label(d1).unwrap().as_deref(), Some("Photos"), "label kept");

        // A FAT card known only by its filesystem serial: weak, accepted when capacity matches.
        let card = record(vec![id("fs_uuid", "1234-ABCD", FS_UUID)], 64_000);
        let d2 = cat.save_drive(&card, None, None).unwrap();
        assert!(matches!(cat.match_drive(&card).unwrap(), DriveMatch::Known { drive_id, weak: true, .. } if drive_id == d2));
        let other_card = record(vec![id("fs_uuid", "1234-ABCD", FS_UUID)], 32_000);
        assert!(matches!(cat.match_drive(&other_card).unwrap(), DriveMatch::Ambiguous(_)));

        // A clone: partition GUID of drive 1 but the serial of nothing known / of drive 2.
        let clone = record(vec![id("fs_uuid", "AAAA", FS_UUID), id("fs_uuid", "1234-ABCD", FS_UUID)], 2_000);
        assert!(matches!(cat.match_drive(&clone).unwrap(), DriveMatch::Ambiguous(c) if c.len() == 2));
    }

    #[test]
    fn filesystems_matched_by_uuid_or_partition() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open(&tmp.path().join("i.db")).unwrap();
        let d = cat.save_drive(&record(vec![id("wwn", "0x5", WWN)], 10), None, None).unwrap();
        let with_uuid = FsRecord { partition_number: Some(1), fs_type: "apfs".into(), fs_uuid: Some("U1".into()), ..Default::default() };
        let a = cat.save_filesystem(d, &with_uuid).unwrap();
        assert_eq!(cat.save_filesystem(d, &with_uuid).unwrap(), a);
        let no_uuid = FsRecord { partition_number: Some(1), fs_type: "ntfs".into(), ..Default::default() };
        let b = cat.save_filesystem(d, &no_uuid).unwrap();
        assert_ne!(a, b);
        assert_eq!(cat.save_filesystem(d, &no_uuid).unwrap(), b);
    }
}
