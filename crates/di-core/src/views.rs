//! Read-only queries for the interactive screens: drive details, directory browsing
//! (ncdu-style, from the stored aggregates) and full file details.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use di_model::FileKind;

#[derive(Debug, Clone, Serialize)]
pub struct DriveDetail {
    pub id: i64,
    pub label: Option<String>,
    pub notes: Option<String>,
    pub fingerprint: String,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub bus: Option<String>,
    pub usb_vendor_id: Option<i64>,
    pub usb_product_id: Option<i64>,
    pub usb_serial: Option<String>,
    pub usb_speed_mbps: Option<i64>,
    pub capacity_bytes: u64,
    pub logical_block_size: Option<i64>,
    pub medium: Option<String>,
    pub ptable_type: Option<String>,
    pub ptable_uuid: Option<String>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub last_scanned_at: Option<i64>,
    pub connect_count: i64,
    pub partitions: Vec<PartitionRow>,
    pub filesystems: Vec<FsRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartitionRow {
    pub number: i64,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub type_name: Option<String>,
    pub part_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsRow {
    pub id: i64,
    pub ordinal: i64,
    pub partition_number: Option<i64>,
    pub fs_type: String,
    pub label: Option<String>,
    pub fs_uuid: Option<String>,
    pub size_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub files: Option<u64>,
    pub bytes: Option<u64>,
    pub scanned_at: Option<i64>,
    pub unscannable_reason: Option<String>,
    pub mount_point: Option<String>,
    pub root_dir: Option<i64>,
}

fn u(v: Option<i64>) -> Option<u64> {
    v.map(|x| x.max(0) as u64)
}

pub fn drive_detail(conn: &Connection, drive_id: i64) -> Result<Option<DriveDetail>> {
    let Some(mut d) = conn
        .query_row(
            "SELECT id, label, notes, fingerprint, vendor, model, serial, firmware, bus, usb_vendor_id, usb_product_id,
                    usb_serial, usb_speed_mbps, capacity_bytes, logical_block_size, medium, ptable_type, ptable_uuid,
                    first_seen_at, last_seen_at, last_scanned_at, connect_count
             FROM drives WHERE id = ?1",
            [drive_id],
            |r| {
                Ok(DriveDetail {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    notes: r.get(2)?,
                    fingerprint: r.get(3)?,
                    vendor: r.get(4)?,
                    model: r.get(5)?,
                    serial: r.get(6)?,
                    firmware: r.get(7)?,
                    bus: r.get(8)?,
                    usb_vendor_id: r.get(9)?,
                    usb_product_id: r.get(10)?,
                    usb_serial: r.get(11)?,
                    usb_speed_mbps: r.get(12)?,
                    capacity_bytes: r.get::<_, i64>(13)?.max(0) as u64,
                    logical_block_size: r.get(14)?,
                    medium: r.get(15)?,
                    ptable_type: r.get(16)?,
                    ptable_uuid: r.get(17)?,
                    first_seen_at: r.get(18)?,
                    last_seen_at: r.get(19)?,
                    last_scanned_at: r.get(20)?,
                    connect_count: r.get(21)?,
                    partitions: Vec::new(),
                    filesystems: Vec::new(),
                })
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT number, start_bytes, size_bytes, type_name, part_uuid FROM partitions WHERE drive_id = ?1 ORDER BY number",
    )?;
    d.partitions = stmt
        .query_map([drive_id], |r| {
            Ok(PartitionRow {
                number: r.get(0)?,
                start_bytes: r.get::<_, i64>(1)?.max(0) as u64,
                size_bytes: r.get::<_, i64>(2)?.max(0) as u64,
                type_name: r.get(3)?,
                part_uuid: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare(
        "SELECT fs.id, (SELECT count(*) FROM filesystems x WHERE x.drive_id = fs.drive_id AND x.id <= fs.id),
                p.number, fs.fs_type, COALESCE(fs.user_label, fs.volume_label), fs.fs_uuid, fs.size_bytes, fs.used_bytes,
                fs.file_count, fs.file_bytes, s.finished_at, fs.unscannable_reason, fs.last_mount_point,
                (SELECT id FROM dirs WHERE filesystem_id = fs.id AND parent_id IS NULL)
         FROM filesystems fs
         LEFT JOIN partitions p ON p.id = fs.partition_id
         LEFT JOIN scans s ON s.id = fs.current_scan_id
         WHERE fs.drive_id = ?1 ORDER BY fs.id",
    )?;
    d.filesystems = stmt
        .query_map([drive_id], |r| {
            Ok(FsRow {
                id: r.get(0)?,
                ordinal: r.get(1)?,
                partition_number: r.get(2)?,
                fs_type: r.get(3)?,
                label: r.get(4)?,
                fs_uuid: r.get(5)?,
                size_bytes: u(r.get(6)?),
                used_bytes: u(r.get(7)?),
                files: u(r.get(8)?),
                bytes: u(r.get(9)?),
                scanned_at: r.get(10)?,
                unscannable_reason: r.get(11)?,
                mount_point: r.get(12)?,
                root_dir: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(d))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirInfo {
    pub id: i64,
    pub filesystem_id: i64,
    pub parent_id: Option<i64>,
    pub path: String,
    pub tree_files: u64,
    pub tree_bytes: u64,
}

pub fn dir_info(conn: &Connection, dir_id: i64) -> Result<Option<DirInfo>> {
    Ok(conn
        .query_row(
            "SELECT id, filesystem_id, parent_id, path, tree_files, tree_bytes FROM dirs WHERE id = ?1",
            [dir_id],
            |r| {
                Ok(DirInfo {
                    id: r.get(0)?,
                    filesystem_id: r.get(1)?,
                    parent_id: r.get(2)?,
                    path: r.get(3)?,
                    tree_files: u(r.get(4)?).unwrap_or(0),
                    tree_bytes: u(r.get(5)?).unwrap_or(0),
                })
            },
        )
        .optional()?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowseEntry {
    /// Directory id for directories, file id for files.
    pub id: i64,
    pub is_dir: bool,
    pub name: String,
    pub bytes: u64,
    /// Files below (directories) or 1.
    pub files: u64,
    pub mtime_ns: Option<i64>,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseSort {
    Size,
    Count,
    Name,
    Mtime,
}

/// Children of a directory: subdirectories (with recursive totals) and files,
/// sorted together. Files are capped at `file_limit` (largest first).
pub fn list_dir(conn: &Connection, dir_id: i64, sort: BrowseSort, file_limit: usize) -> Result<Vec<BrowseEntry>> {
    let mut out: Vec<BrowseEntry> = Vec::new();
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, COALESCE(tree_bytes, 0), COALESCE(tree_files, 0), mtime_ns, flags FROM dirs WHERE parent_id = ?1",
    )?;
    let dirs = stmt.query_map([dir_id], |r| {
        Ok(BrowseEntry {
            id: r.get(0)?,
            is_dir: true,
            name: r.get(1)?,
            bytes: r.get::<_, i64>(2)?.max(0) as u64,
            files: r.get::<_, i64>(3)?.max(0) as u64,
            mtime_ns: r.get(4)?,
            flags: r.get::<_, i64>(5)? as u32,
        })
    })?;
    for d in dirs {
        out.push(d?);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, size, mtime_ns, flags FROM files WHERE dir_id = ?1 ORDER BY size DESC LIMIT ?2",
    )?;
    let files = stmt.query_map(params![dir_id, file_limit as i64], |r| {
        Ok(BrowseEntry {
            id: r.get(0)?,
            is_dir: false,
            name: r.get(1)?,
            bytes: r.get::<_, i64>(2)?.max(0) as u64,
            files: 1,
            mtime_ns: r.get(3)?,
            flags: r.get::<_, i64>(4)? as u32,
        })
    })?;
    for f in files {
        out.push(f?);
    }
    match sort {
        BrowseSort::Size => out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name))),
        BrowseSort::Count => out.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| b.bytes.cmp(&a.bytes))),
        BrowseSort::Name => out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))),
        BrowseSort::Mtime => out.sort_by_key(|e| std::cmp::Reverse(e.mtime_ns)),
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct FileDetail {
    pub id: i64,
    pub drive_id: i64,
    pub drive_label: Option<String>,
    pub filesystem_id: i64,
    pub fs_label: Option<String>,
    pub fs_type: String,
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub alloc_size: Option<u64>,
    pub mtime_ns: Option<i64>,
    pub ctime_ns: Option<i64>,
    pub btime_ns: Option<i64>,
    pub mode: Option<i64>,
    pub uid: Option<i64>,
    pub gid: Option<i64>,
    pub inode: Option<i64>,
    pub nlink: Option<i64>,
    pub flags: Vec<&'static str>,
    pub link_target: Option<String>,
    pub scanned_at: Option<i64>,
    pub mount_point: Option<String>,
    pub quick_hash: Option<String>,
    pub media: Option<MediaDetail>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MediaDetail {
    pub kind: String,
    pub status: i64,
    pub extractor: String,
    pub captured_at: Option<String>,
    pub captured_offset_min: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub gps: Option<(f64, f64)>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub exposure_s: Option<f64>,
    pub f_number: Option<f64>,
    pub focal_mm: Option<f64>,
    pub container: Option<String>,
    pub bitrate_bps: Option<i64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<i64>,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub tags: Option<String>,
    /// "video h264 1920x1080 25 fps", "audio aac 2 ch 48000 Hz eng", …
    pub streams: Vec<String>,
}

impl MediaDetail {
    /// Labelled lines for detail views.
    pub fn lines(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        let status = match self.status {
            0 => "",
            1 => " (partial)",
            2 => " (unsupported format)",
            _ => " (error)",
        };
        out.push(("Media", format!("{} via {}{status}", self.kind, self.extractor)));
        if let Some(c) = &self.captured_at {
            let off = self
                .captured_offset_min
                .map(|m| format!(" UTC{}{:02}:{:02}", if m < 0 { '-' } else { '+' }, m.abs() / 60, m.abs() % 60))
                .unwrap_or_default();
            out.push(("Taken", format!("{}{off}", c.replace('T', " "))));
        }
        if let Some(c) = &self.camera {
            out.push(("Camera", c.clone()));
        }
        if let Some(l) = &self.lens {
            out.push(("Lens", l.clone()));
        }
        let mut exp = Vec::new();
        if let Some(e) = self.exposure_s {
            exp.push(if e > 0.0 && e < 1.0 { format!("1/{:.0} s", 1.0 / e) } else { format!("{e} s") });
        }
        if let Some(f) = self.f_number {
            exp.push(format!("f/{f}"));
        }
        if let Some(i) = self.iso {
            exp.push(format!("ISO {i}"));
        }
        if let Some(fl) = self.focal_mm {
            exp.push(format!("{fl} mm"));
        }
        if !exp.is_empty() {
            out.push(("Exposure", exp.join("  ")));
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            out.push(("Pixels", format!("{w} × {h}")));
        }
        if let Some(d) = self.duration_ms {
            let s = d / 1000;
            out.push(("Duration", format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)));
        }
        if let Some((lat, lon)) = self.gps {
            out.push(("Location", format!("{lat:.5}, {lon:.5}")));
        }
        let audio = [
            self.artist.clone(),
            self.title.clone().map(|t| format!("“{t}”")),
            self.album.clone().map(|a| format!("on {a}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !audio.is_empty() {
            out.push(("Track", audio.join(" ")));
        }
        let more = [
            self.album_artist.clone().map(|a| format!("album artist {a}")),
            self.track_no.map(|t| format!("track {t}")),
            self.year.map(|y| y.to_string()),
            self.genre.clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !more.is_empty() {
            out.push(("", more.join(" · ")));
        }
        if let Some(c) = &self.container {
            let br = self.bitrate_bps.map(|b| format!(", {} kb/s", b / 1000)).unwrap_or_default();
            out.push(("Container", format!("{c}{br}")));
        }
        for s in &self.streams {
            out.push(("Stream", s.clone()));
        }
        out
    }
}

pub fn media_detail(conn: &Connection, file_id: i64) -> Result<Option<MediaDetail>> {
    let Some(mut m) = conn
        .query_row(
            "SELECT kind, status, extractor, captured_at, captured_offset_min, width, height, duration_ms, gps_lat, gps_lon,
                    TRIM(COALESCE(camera_make || ' ', '') || COALESCE(camera_model, '')), lens_model, iso, exposure_s,
                    f_number, focal_mm, container, bitrate_bps, title, artist, album, album_artist, track_no, year,
                    genre, tags
             FROM media WHERE file_id = ?1",
            [file_id],
            |r| {
                let lat: Option<f64> = r.get(8)?;
                let lon: Option<f64> = r.get(9)?;
                Ok(MediaDetail {
                    kind: r.get(0)?,
                    status: r.get(1)?,
                    extractor: r.get(2)?,
                    captured_at: r.get(3)?,
                    captured_offset_min: r.get(4)?,
                    width: r.get(5)?,
                    height: r.get(6)?,
                    duration_ms: r.get(7)?,
                    gps: lat.zip(lon),
                    camera: r.get::<_, Option<String>>(10)?.filter(|s| !s.is_empty()),
                    lens: r.get(11)?,
                    iso: r.get(12)?,
                    exposure_s: r.get(13)?,
                    f_number: r.get(14)?,
                    focal_mm: r.get(15)?,
                    container: r.get(16)?,
                    bitrate_bps: r.get(17)?,
                    title: r.get(18)?,
                    artist: r.get(19)?,
                    album: r.get(20)?,
                    album_artist: r.get(21)?,
                    track_no: r.get(22)?,
                    year: r.get(23)?,
                    genre: r.get(24)?,
                    tags: r.get(25)?,
                    streams: Vec::new(),
                })
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT type, codec, profile, width, height, fps, bitrate_bps, sample_rate, channels, language
         FROM media_streams WHERE file_id = ?1 ORDER BY idx",
    )?;
    m.streams = stmt
        .query_map([file_id], |r| {
            let mut parts: Vec<String> = vec![r.get::<_, String>(0)?];
            parts.extend(r.get::<_, Option<String>>(1)?);
            parts.extend(r.get::<_, Option<String>>(2)?.map(|p| format!("({p})")));
            if let (Some(w), Some(h)) = (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?) {
                parts.push(format!("{w}×{h}"));
            }
            parts.extend(r.get::<_, Option<f64>>(5)?.map(|f| format!("{f:.3} fps").replace(".000", "")));
            parts.extend(r.get::<_, Option<i64>>(6)?.map(|b| format!("{} kb/s", b / 1000)));
            parts.extend(r.get::<_, Option<i64>>(7)?.map(|s| format!("{s} Hz")));
            parts.extend(r.get::<_, Option<i64>>(8)?.map(|c| format!("{c} ch")));
            parts.extend(r.get::<_, Option<String>>(9)?);
            Ok(parts.join(" "))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(m))
}

pub fn file_detail(conn: &Connection, file_id: i64) -> Result<Option<FileDetail>> {
    Ok(conn
        .query_row(
            "SELECT f.id, d.id, d.label, fs.id, COALESCE(fs.user_label, fs.volume_label), fs.fs_type,
                    CASE di.path WHEN '/' THEN '/' || f.name ELSE di.path || '/' || f.name END,
                    f.kind, f.size, f.alloc_size, f.mtime_ns, f.ctime_ns, f.btime_ns, f.mode, f.uid, f.gid, f.inode,
                    f.nlink, f.flags, f.link_target, s.finished_at, fs.last_mount_point, h.quick
             FROM files f JOIN dirs di ON di.id = f.dir_id JOIN filesystems fs ON fs.id = f.filesystem_id
             JOIN drives d ON d.id = fs.drive_id LEFT JOIN scans s ON s.id = fs.current_scan_id
             LEFT JOIN file_hashes h ON h.file_id = f.id
             WHERE f.id = ?1",
            [file_id],
            |r| {
                Ok(FileDetail {
                    id: r.get(0)?,
                    drive_id: r.get(1)?,
                    drive_label: r.get(2)?,
                    filesystem_id: r.get(3)?,
                    fs_label: r.get(4)?,
                    fs_type: r.get(5)?,
                    path: r.get(6)?,
                    kind: FileKind::from_code(r.get(7)?).as_str().to_string(),
                    size: r.get::<_, i64>(8)?.max(0) as u64,
                    alloc_size: u(r.get(9)?),
                    mtime_ns: r.get(10)?,
                    ctime_ns: r.get(11)?,
                    btime_ns: r.get(12)?,
                    mode: r.get(13)?,
                    uid: r.get(14)?,
                    gid: r.get(15)?,
                    inode: r.get(16)?,
                    nlink: r.get(17)?,
                    flags: di_model::flags::describe(r.get::<_, i64>(18)? as u32),
                    link_target: r.get(19)?,
                    scanned_at: r.get(20)?,
                    mount_point: r.get(21)?,
                    quick_hash: r.get::<_, Option<Vec<u8>>>(22)?.map(|h| h.iter().map(|b| format!("{b:02x}")).collect()),
                    media: None,
                })
            },
        )
        .optional()?
        .map(|mut d: FileDetail| {
            d.media = media_detail(conn, file_id).ok().flatten();
            d
        }))
}

/// A scanned filesystem, for the browse picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FsChoice {
    pub fs_id: i64,
    pub drive_id: i64,
    /// The `n` in `D<drive>.<n>`.
    pub ordinal: i64,
    pub fs_type: String,
    pub label: Option<String>,
    pub root_dir: i64,
}

/// Filesystems that have been scanned, for the browse picker.
pub fn browsable_filesystems(conn: &Connection) -> Result<Vec<FsChoice>> {
    let mut stmt = conn.prepare(
        "SELECT fs.id, fs.drive_id, (SELECT count(*) FROM filesystems x WHERE x.drive_id = fs.drive_id AND x.id <= fs.id),
                fs.fs_type, COALESCE(fs.user_label, fs.volume_label, d.label), r.id
         FROM filesystems fs JOIN drives d ON d.id = fs.drive_id
         JOIN dirs r ON r.filesystem_id = fs.id AND r.parent_id IS NULL
         ORDER BY fs.drive_id, fs.id",
    )?;
    Ok(stmt
        .query_map([], |r| {
            Ok(FsChoice {
                fs_id: r.get(0)?,
                drive_id: r.get(1)?,
                ordinal: r.get(2)?,
                fs_type: r.get(3)?,
                label: r.get(4)?,
                root_dir: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
