//! Content-derived file data: quick hashes and media metadata.
//!
//! One [`HeadTailReader`] per file fetches the start and end (two reads), which serve
//! both the quick hash and most metadata parsers. Parsers: `nom-exif` for photo EXIF
//! and video track info, `lofty` for audio tags; optionally `ffprobe` (video streams)
//! and `exiftool` (photo formats the built-in parser cannot read).

mod exiftool;
mod reader;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

pub use exiftool::ExiftoolSession;
pub use reader::HeadTailReader;

const HEAD: usize = 256 * 1024;
const TAIL: usize = 64 * 1024;
const QUICK_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        }
    }

    /// From an `ext_categories` category.
    pub fn from_category(cat: &str) -> Option<MediaKind> {
        match cat {
            "photo" | "photo_raw" => Some(MediaKind::Image),
            "video" => Some(MediaKind::Video),
            "audio" => Some(MediaKind::Audio),
            _ => None,
        }
    }
}

/// `media.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    /// Some data missing (e.g. video without ffprobe: no stream list).
    Partial = 1,
    Unsupported = 2,
    Error = 3,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StreamInfo {
    pub idx: u32,
    pub kind: String,
    pub codec: Option<String>,
    pub profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bitrate_bps: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub bit_depth: Option<u32>,
    pub language: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaInfo {
    pub kind: MediaKind,
    pub status: Status,
    pub extractor: String,
    pub error: Option<String>,
    /// Camera-local wall-clock time, ISO 8601 (`2014-07-12T14:03:27`).
    pub captured_at: Option<String>,
    pub captured_offset_min: Option<i32>,
    pub captured_offset_inferred: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub gps_alt_m: Option<f64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub camera_serial: Option<String>,
    pub lens_model: Option<String>,
    pub orientation: Option<u32>,
    pub iso: Option<u32>,
    pub exposure_s: Option<f64>,
    pub f_number: Option<f64>,
    pub focal_mm: Option<f64>,
    pub container: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub streams: Vec<StreamInfo>,
    /// Other fields, kept as JSON (`media.tags`).
    pub tags: serde_json::Map<String, serde_json::Value>,
}

impl MediaInfo {
    fn new(kind: MediaKind, extractor: &str) -> MediaInfo {
        MediaInfo {
            kind,
            status: Status::Ok,
            extractor: extractor.to_string(),
            error: None,
            captured_at: None,
            captured_offset_min: None,
            captured_offset_inferred: false,
            width: None,
            height: None,
            duration_ms: None,
            gps_lat: None,
            gps_lon: None,
            gps_alt_m: None,
            camera_make: None,
            camera_model: None,
            camera_serial: None,
            lens_model: None,
            orientation: None,
            iso: None,
            exposure_s: None,
            f_number: None,
            focal_mm: None,
            container: None,
            bitrate_bps: None,
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            track_no: None,
            disc_no: None,
            year: None,
            genre: None,
            streams: Vec::new(),
            tags: serde_json::Map::new(),
        }
    }

    fn failed(kind: MediaKind, extractor: &str, status: Status, error: String) -> MediaInfo {
        let mut m = MediaInfo::new(kind, extractor);
        m.status = status;
        m.error = Some(error);
        m
    }
}

/// What the content phase stores for one file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extracted {
    /// BLAKE3(size ‖ first 64 KiB ‖ last 64 KiB), 16 bytes. Whole content for small files.
    pub quick_hash: Option<[u8; 16]>,
    pub media: Option<MediaInfo>,
    /// Bytes actually read from the file by us (not by external tools).
    pub bytes_read: u64,
}

/// Configured extractors. Cheap to create per worker thread.
pub struct Extractor {
    ffprobe: Option<PathBuf>,
    exiftool_path: Option<PathBuf>,
    /// Started on first use (it is a Perl process).
    exiftool: Option<ExiftoolSession>,
    max_extra_read: u64,
}

/// Find a tool on PATH or in the usual locations a non-login PATH misses.
pub fn locate_tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/opt/local/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

impl Extractor {
    /// `ffprobe`/`exiftool`: paths of usable tools, or None.
    pub fn new(ffprobe: Option<PathBuf>, exiftool: Option<PathBuf>) -> Extractor {
        Extractor { ffprobe, exiftool_path: exiftool, exiftool: None, max_extra_read: 16 * 1024 * 1024 }
    }

    pub fn has_ffprobe(&self) -> bool {
        self.ffprobe.is_some()
    }

    /// Read `path` once (start + end) for the quick hash and, if `kind` is given,
    /// media metadata. Never panics on malformed files.
    pub fn extract(&mut self, path: &Path, kind: Option<MediaKind>, want_hash: bool) -> std::io::Result<Extracted> {
        let mut reader = HeadTailReader::open(path, HEAD, TAIL, self.max_extra_read)?;
        let quick_hash = want_hash.then(|| quick_hash(&reader));
        let media = kind.map(|k| {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match k {
                MediaKind::Image => self.image(path, &mut reader),
                MediaKind::Video => self.video(path, &mut reader),
                MediaKind::Audio => audio(&mut reader),
            }));
            r.unwrap_or_else(|_| MediaInfo::failed(k, "drive-index", Status::Error, "parser panicked".into()))
        });
        Ok(Extracted { quick_hash, media, bytes_read: reader.bytes_read() })
    }

    fn image(&mut self, path: &Path, reader: &mut HeadTailReader) -> MediaInfo {
        reader.rewind_to_start();
        let parsed = nom_exif::MediaSource::seekable(&mut *reader).and_then(|ms| {
            let mut parser = nom_exif::MediaParser::new();
            parser.parse_exif(ms).map(nom_exif::Exif::from)
        });
        match parsed {
            Ok(exif) => {
                let mut m = MediaInfo::new(MediaKind::Image, "nom-exif");
                fill_from_exif(&mut m, &exif);
                if m.captured_at.is_none() && m.camera_model.is_none() {
                    // Parsed, but nothing useful: maybe exiftool knows more.
                    if let Some(better) = self.exiftool_image(path) {
                        return better;
                    }
                }
                m
            }
            Err(e) => self
                .exiftool_image(path)
                .unwrap_or_else(|| MediaInfo::failed(MediaKind::Image, "nom-exif", Status::Unsupported, e.to_string())),
        }
    }

    fn exiftool_image(&mut self, path: &Path) -> Option<MediaInfo> {
        if self.exiftool.is_none() {
            self.exiftool = ExiftoolSession::start(self.exiftool_path.as_ref()?).ok();
            if self.exiftool.is_none() {
                self.exiftool_path = None; // don't retry a broken tool for every file
            }
        }
        let session = self.exiftool.as_mut()?;
        let json = session.query(path).ok()?;
        let mut m = MediaInfo::new(MediaKind::Image, "exiftool");
        exiftool::fill(&mut m, &json);
        (m.captured_at.is_some() || m.camera_model.is_some() || m.width.is_some()).then_some(m)
    }

    fn video(&mut self, path: &Path, reader: &mut HeadTailReader) -> MediaInfo {
        reader.rewind_to_start();
        let track = nom_exif::MediaSource::seekable(&mut *reader).and_then(|ms| {
            let mut parser = nom_exif::MediaParser::new();
            parser.parse_track(ms)
        });
        let mut m = MediaInfo::new(MediaKind::Video, "nom-exif");
        let mut parsed = false;
        if let Ok(t) = &track {
            parsed = true;
            use nom_exif::TrackInfoTag as T;
            m.camera_make = t.get(T::Make).and_then(text);
            m.camera_model = t.get(T::Model).and_then(text);
            m.duration_ms = t.get(T::DurationMs).and_then(num).map(|v| v as u64);
            m.width = t.get(T::Width).and_then(num).map(|v| v as u32);
            m.height = t.get(T::Height).and_then(num).map(|v| v as u32);
            if let Some(dt) = t.get(T::CreateDate) {
                set_capture(&mut m, dt, true);
            }
            if let Some(g) = t.gps_info() {
                m.gps_lat = g.latitude_decimal();
                m.gps_lon = g.longitude_decimal();
                m.gps_alt_m = g.altitude_meters();
            }
            if m.gps_lat.is_none() {
                if let Some((lat, lon, alt)) = t.get(T::GpsIso6709).and_then(text).as_deref().and_then(parse_iso6709) {
                    (m.gps_lat, m.gps_lon, m.gps_alt_m) = (Some(lat), Some(lon), alt);
                }
            }
            if let Some(a) = t.get(T::Author).and_then(text) {
                m.tags.insert("author".into(), a.into());
            }
        }
        match &self.ffprobe {
            Some(ff) => match ffprobe(ff, path) {
                Ok(probe) => {
                    m.extractor = if parsed { "nom-exif+ffprobe".into() } else { "ffprobe".into() };
                    merge_ffprobe(&mut m, &probe);
                }
                Err(e) if !parsed => return MediaInfo::failed(MediaKind::Video, "ffprobe", Status::Error, e),
                Err(e) => {
                    m.status = Status::Partial;
                    m.error = Some(format!("ffprobe: {e}"));
                }
            },
            None if parsed => {
                // Container-level data only; refilled once ffprobe is installed.
                m.status = Status::Partial;
            }
            None => {
                let e = track.err().map(|e| e.to_string()).unwrap_or_default();
                return MediaInfo::failed(MediaKind::Video, "nom-exif", Status::Unsupported, e);
            }
        }
        m
    }
}

/// BLAKE3 of the whole file, streamed in 1 MiB chunks. Returns None if `stop()`
/// becomes true midway. `on_bytes` is told about progress.
pub fn full_hash(path: &Path, stop: &dyn Fn() -> bool, on_bytes: &dyn Fn(u64)) -> std::io::Result<Option<[u8; 32]>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    #[cfg(target_os = "macos")]
    unsafe {
        use std::os::fd::AsRawFd;
        libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1);
    }
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if stop() {
            return Ok(None);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        on_bytes(n as u64);
    }
    Ok(Some(*h.finalize().as_bytes()))
}

/// The quick hash from the reader's prefetched buffers.
pub fn quick_hash(r: &HeadTailReader) -> [u8; 16] {
    let size = r.size();
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    let head = r.head();
    if size as usize <= 2 * QUICK_CHUNK && head.len() as u64 == size {
        h.update(head);
    } else {
        h.update(&head[..head.len().min(QUICK_CHUNK)]);
        let tail = r.tail();
        h.update(&tail[tail.len().saturating_sub(QUICK_CHUNK)..]);
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

// -------------------------------------------------------------------------------------
// EXIF helpers
// -------------------------------------------------------------------------------------

fn text(v: &nom_exif::EntryValue) -> Option<String> {
    v.as_str().map(|s| s.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()).filter(|s| !s.is_empty())
}

fn num(v: &nom_exif::EntryValue) -> Option<f64> {
    use nom_exif::EntryValue as E;
    Some(match v {
        E::U8(x) => *x as f64,
        E::U16(x) => *x as f64,
        E::U32(x) => *x as f64,
        E::U64(x) => *x as f64,
        E::I8(x) => *x as f64,
        E::I16(x) => *x as f64,
        E::I32(x) => *x as f64,
        E::I64(x) => *x as f64,
        E::F32(x) => *x as f64,
        E::F64(x) => *x,
        E::URational(r) => r.to_f64()?,
        E::IRational(r) => r.to_f64()?,
        E::URationalArray(a) => a.first()?.to_f64()?,
        E::U16Array(a) => *a.first()? as f64,
        E::U32Array(a) => *a.first()? as f64,
        E::Text(s) => s.trim().parse().ok()?,
        _ => return None,
    })
    .filter(|x: &f64| x.is_finite())
}

/// Capture time: aware values keep their offset; naive ones are camera-local.
/// `utc_naive`: the container stores UTC (QuickTime), so convert with the host zone.
fn set_capture(m: &mut MediaInfo, v: &nom_exif::EntryValue, utc_naive: bool) {
    use nom_exif::EntryValue as E;
    match v {
        E::DateTime(dt) => {
            let off = dt.offset().local_minus_utc() / 60;
            if utc_naive && off == 0 {
                // Stored as UTC: show in the host's zone, offset inferred.
                let ts = jiff::Timestamp::from_second(dt.timestamp()).ok();
                if let Some(z) = ts.map(|t| t.to_zoned(jiff::tz::TimeZone::system())) {
                    m.captured_at = Some(z.strftime("%Y-%m-%dT%H:%M:%S").to_string());
                    m.captured_offset_min = Some(z.offset().seconds() / 60);
                    m.captured_offset_inferred = true;
                    return;
                }
            }
            m.captured_at = Some(dt.format("%Y-%m-%dT%H:%M:%S").to_string());
            m.captured_offset_min = Some(off);
        }
        E::NaiveDateTime(n) => m.captured_at = Some(n.format("%Y-%m-%dT%H:%M:%S").to_string()),
        E::Text(s) => m.captured_at = normalize_exif_datetime(s),
        _ => {}
    }
}

/// ISO 6709 point (`+59.3293+018.0686/`, `+59.3293+018.0686+012.5/`) → (lat, lon, alt).
pub fn parse_iso6709(s: &str) -> Option<(f64, f64, Option<f64>)> {
    let s = s.trim().trim_end_matches('/');
    let mut parts = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if (c == '+' || c == '-') && i > start {
            parts.push(&s[start..i]);
            start = i;
        }
    }
    parts.push(&s[start..]);
    let lat: f64 = parts.first()?.parse().ok()?;
    let lon: f64 = parts.get(1)?.parse().ok()?;
    let alt = parts.get(2).and_then(|a| a.parse().ok());
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon, alt))
}

/// `2014:07:12 14:03:27` → `2014-07-12T14:03:27`.
pub fn normalize_exif_datetime(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() < 19 || s.starts_with("0000") {
        return None;
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(19);
    for (i, &c) in b[..19].iter().enumerate() {
        out.push(match (i, c) {
            (4 | 7, b':' | b'-') => '-',
            (10, b' ' | b'T') => 'T',
            (_, c) => c as char,
        });
    }
    out.chars().enumerate().all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit()).then_some(out)
}

fn fill_from_exif(m: &mut MediaInfo, exif: &nom_exif::Exif) {
    use nom_exif::ExifTag as T;
    m.camera_make = exif.get(T::Make).and_then(text);
    m.camera_model = exif.get(T::Model).and_then(text);
    m.camera_serial = exif.get(T::CameraSerialNumber).and_then(text);
    m.lens_model = exif.get(T::LensModel).and_then(text);
    m.orientation = exif.get(T::Orientation).and_then(num).map(|v| v as u32);
    m.iso = exif.get(T::ISOSpeedRatings).and_then(num).map(|v| v as u32);
    m.exposure_s = exif.get(T::ExposureTime).and_then(num);
    m.f_number = exif.get(T::FNumber).and_then(num);
    m.focal_mm = exif.get(T::FocalLength).and_then(num);
    m.width = exif.get(T::ExifImageWidth).or_else(|| exif.get(T::ImageWidth)).and_then(num).map(|v| v as u32);
    m.height = exif.get(T::ExifImageHeight).and_then(num).map(|v| v as u32);
    if let Some(dt) = exif.get(T::DateTimeOriginal).or_else(|| exif.get(T::CreateDate)) {
        set_capture(m, dt, false);
    }
    if let Some(g) = exif.gps_info() {
        m.gps_lat = g.latitude_decimal();
        m.gps_lon = g.longitude_decimal();
        m.gps_alt_m = g.altitude_meters();
    }
}

// -------------------------------------------------------------------------------------
// Audio (lofty)
// -------------------------------------------------------------------------------------

fn audio(reader: &mut HeadTailReader) -> MediaInfo {
    use lofty::prelude::*;
    reader.rewind_to_start();
    let mut opts = lofty::config::ParseOptions::new();
    let opts = opts.read_cover_art(false);
    let probe = match lofty::probe::Probe::new(&mut *reader).options(opts).guess_file_type() {
        Ok(p) => p,
        Err(e) => return MediaInfo::failed(MediaKind::Audio, "lofty", Status::Error, e.to_string()),
    };
    let container = probe.file_type().map(|t| format!("{t:?}").to_lowercase());
    let tagged = match probe.read() {
        Ok(t) => t,
        Err(e) => return MediaInfo::failed(MediaKind::Audio, "lofty", Status::Unsupported, e.to_string()),
    };
    let mut m = MediaInfo::new(MediaKind::Audio, "lofty");
    m.container = container;
    let p = tagged.properties();
    m.duration_ms = Some(p.duration().as_millis() as u64).filter(|&d| d > 0);
    m.bitrate_bps = p.audio_bitrate().or(p.overall_bitrate()).map(|k| k as u64 * 1000);
    let codec = match m.container.as_deref() {
        Some("mpeg") => Some("mp3"),
        Some("flac") => Some("flac"),
        Some("opus") => Some("opus"),
        Some("vorbis") => Some("vorbis"),
        Some("speex") => Some("speex"),
        Some("wav") | Some("aiff") => Some("pcm"),
        Some("ape") => Some("ape"),
        Some("wavpack") => Some("wavpack"),
        _ => None, // MP4 audio can be AAC or ALAC: unknown here
    };
    m.streams.push(StreamInfo {
        idx: 0,
        kind: "audio".into(),
        codec: codec.map(str::to_string),
        sample_rate: p.sample_rate(),
        channels: p.channels().map(u32::from),
        bit_depth: p.bit_depth().map(u32::from),
        bitrate_bps: m.bitrate_bps,
        duration_ms: m.duration_ms,
        ..StreamInfo::default()
    });
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let s = |v: Option<std::borrow::Cow<str>>| v.map(|c| c.trim().to_string()).filter(|x| !x.is_empty());
        m.title = s(tag.title());
        m.artist = s(tag.artist());
        m.album = s(tag.album());
        m.genre = s(tag.genre());
        m.track_no = tag.track();
        m.disc_no = tag.disk();
        m.album_artist = tag.get_string(ItemKey::AlbumArtist).map(str::to_string);
        m.year = tag
            .date()
            .map(|d| d.year as i32)
            .or_else(|| tag.get_string(ItemKey::Year).and_then(|y| y.get(..4)?.parse().ok()))
            .or_else(|| tag.get_string(ItemKey::RecordingDate).and_then(|y| y.get(..4)?.parse().ok()));
        if tag.picture_count() > 0 {
            m.tags.insert("cover_art".into(), true.into());
        }
    }
    m
}

// -------------------------------------------------------------------------------------
// ffprobe
// -------------------------------------------------------------------------------------

fn ffprobe(prog: &Path, path: &Path) -> Result<serde_json::Value, String> {
    let out = run_with_timeout(
        std::process::Command::new(prog)
            .args(["-v", "error", "-probesize", "10M", "-print_format", "json", "-show_format", "-show_streams"])
            .arg(path),
        Duration::from_secs(60),
    )?;
    serde_json::from_slice(&out).map_err(|e| e.to_string())
}

fn run_with_timeout(cmd: &mut std::process::Command, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().unwrap_or_default();
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        let _ = e.read_to_string(&mut err);
                    }
                    return Err(err.lines().next().unwrap_or("failed").to_string());
                }
                return Ok(out);
            }
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
        }
    }
}

fn merge_ffprobe(m: &mut MediaInfo, j: &serde_json::Value) {
    let s = |v: &serde_json::Value| v.as_str().map(str::to_string).filter(|x| !x.is_empty());
    let n = |v: &serde_json::Value| v.as_str().and_then(|x| x.parse::<f64>().ok()).or_else(|| v.as_f64());
    if let Some(fmt) = j.get("format") {
        m.container = fmt.get("format_name").and_then(s).map(|f| f.split(',').next().unwrap_or("").to_string());
        if m.duration_ms.is_none() {
            m.duration_ms = fmt.get("duration").and_then(n).map(|d| (d * 1000.0) as u64);
        }
        m.bitrate_bps = fmt.get("bit_rate").and_then(n).map(|b| b as u64);
        if let Some(tags) = fmt.get("tags").and_then(|t| t.as_object()) {
            let tag = |k: &str| tags.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).and_then(|(_, v)| s(v));
            if m.captured_at.is_none() {
                if let Some(ct) = tag("com.apple.quicktime.creationdate").or_else(|| tag("creation_time")) {
                    if let Ok(z) = ct.parse::<jiff::Timestamp>() {
                        let local = z.to_zoned(jiff::tz::TimeZone::system());
                        m.captured_at = Some(local.strftime("%Y-%m-%dT%H:%M:%S").to_string());
                        m.captured_offset_min = Some(local.offset().seconds() / 60);
                        m.captured_offset_inferred = true;
                    }
                }
            }
            for k in ["title", "artist", "album", "genre"] {
                if let Some(v) = tag(k) {
                    match k {
                        "title" => m.title = m.title.take().or(Some(v)),
                        "artist" => m.artist = m.artist.take().or(Some(v)),
                        "album" => m.album = m.album.take().or(Some(v)),
                        _ => m.genre = m.genre.take().or(Some(v)),
                    }
                }
            }
            if m.gps_lat.is_none() {
                let loc = tag("com.apple.quicktime.location.ISO6709").or_else(|| tag("location"));
                if let Some((lat, lon, alt)) = loc.as_deref().and_then(parse_iso6709) {
                    (m.gps_lat, m.gps_lon, m.gps_alt_m) = (Some(lat), Some(lon), alt);
                }
            }
            if let Some(enc) = tag("encoder") {
                m.tags.insert("encoder".into(), enc.into());
            }
        }
    }
    m.streams.clear();
    for st in j.get("streams").and_then(|v| v.as_array()).into_iter().flatten() {
        let fps = st.get("avg_frame_rate").and_then(|v| v.as_str()).and_then(|r| {
            let (a, b) = r.split_once('/')?;
            let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
            (b > 0.0 && a > 0.0).then(|| a / b)
        });
        let info = StreamInfo {
            idx: st.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            kind: st.get("codec_type").and_then(s).unwrap_or_else(|| "data".into()),
            codec: st.get("codec_name").and_then(s),
            profile: st.get("profile").and_then(s),
            width: st.get("width").and_then(|v| v.as_u64()).map(|v| v as u32),
            height: st.get("height").and_then(|v| v.as_u64()).map(|v| v as u32),
            fps,
            bitrate_bps: st.get("bit_rate").and_then(n).map(|b| b as u64),
            sample_rate: st.get("sample_rate").and_then(n).map(|v| v as u32),
            channels: st.get("channels").and_then(|v| v.as_u64()).map(|v| v as u32),
            bit_depth: st.get("bits_per_raw_sample").and_then(n).map(|v| v as u32),
            language: st.get("tags").and_then(|t| t.get("language")).and_then(s),
            duration_ms: st.get("duration").and_then(n).map(|d| (d * 1000.0) as u64),
        };
        if info.kind == "video" && m.width.is_none() {
            m.width = info.width;
            m.height = info.height;
        }
        m.streams.push(info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn iso6709() {
        assert_eq!(parse_iso6709("+59.3293+018.0686/"), Some((59.3293, 18.0686, None)));
        assert_eq!(parse_iso6709("-33.8568+151.2153+012.500/"), Some((-33.8568, 151.2153, Some(12.5))));
        assert_eq!(parse_iso6709("nonsense"), None);
    }

    #[test]
    fn video_with_ffprobe_has_streams() {
        let Some(ff) = ["/opt/homebrew/bin/ffprobe", "/usr/bin/ffprobe", "/usr/local/bin/ffprobe"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        else {
            return; // ffprobe not installed here
        };
        let mut x = Extractor::new(Some(ff), None);
        let m = x.extract(&fixture("video.mp4"), Some(MediaKind::Video), false).unwrap().media.unwrap();
        assert_eq!(m.status, Status::Ok, "{m:?}");
        let codecs: Vec<_> = m.streams.iter().map(|s| (s.kind.as_str(), s.codec.as_deref())).collect();
        assert_eq!(codecs, vec![("video", Some("h264")), ("audio", Some("aac"))]);
        assert_eq!(m.streams[0].fps, Some(10.0));
        assert_eq!(m.container.as_deref(), Some("mov"));
        assert!((m.gps_lat.unwrap() - 59.3293).abs() < 1e-3, "MP4 location via ffprobe: {m:?}");
        let mkv = x.extract(&fixture("video.mkv"), Some(MediaKind::Video), false).unwrap().media.unwrap();
        assert_eq!(mkv.container.as_deref(), Some("matroska"));
        assert_eq!(mkv.streams[0].codec.as_deref(), Some("h264"));
    }

    #[test]
    fn datetime_normalization() {
        assert_eq!(normalize_exif_datetime("2014:07:12 14:03:27").as_deref(), Some("2014-07-12T14:03:27"));
        assert_eq!(normalize_exif_datetime("0000:00:00 00:00:00"), None);
        assert_eq!(normalize_exif_datetime("garbage"), None);
    }

    #[test]
    fn jpeg_exif() {
        let mut x = Extractor::new(None, None);
        let e = x.extract(&fixture("photo.jpg"), Some(MediaKind::Image), true).unwrap();
        let m = e.media.unwrap();
        assert_eq!(m.status, Status::Ok, "{m:?}");
        assert_eq!(m.camera_make.as_deref(), Some("Canon"));
        assert_eq!(m.camera_model.as_deref(), Some("Canon EOS 5D Mark III"));
        assert_eq!(m.lens_model.as_deref(), Some("EF24-105mm f/4L IS USM"));
        assert_eq!(m.captured_at.as_deref(), Some("2014-07-12T14:03:27"));
        assert_eq!(m.captured_offset_min, Some(0));
        assert_eq!(m.iso, Some(3200));
        assert_eq!(m.f_number, Some(5.6));
        assert_eq!(m.focal_mm, Some(50.0));
        assert!((m.exposure_s.unwrap() - 0.004).abs() < 1e-9);
        assert!((m.gps_lat.unwrap() - 64.1466).abs() < 1e-3);
        assert!((m.gps_lon.unwrap() + 21.9426).abs() < 1e-3, "west is negative");
        assert!(e.quick_hash.is_some());
    }

    #[test]
    fn heic_and_tiff() {
        let mut x = Extractor::new(None, None);
        let heic = x.extract(&fixture("photo.heic"), Some(MediaKind::Image), false).unwrap().media.unwrap();
        assert_eq!(heic.camera_model.as_deref(), Some("iPhone 12"), "{heic:?}");
        assert_eq!(heic.captured_at.as_deref(), Some("2021-03-04T09:10:11"));
        assert_eq!(heic.captured_offset_min, Some(60));
        let tif = x.extract(&fixture("photo.tif"), Some(MediaKind::Image), false).unwrap().media.unwrap();
        assert_eq!(tif.camera_model.as_deref(), Some("NIKON D700"), "{tif:?}");
        assert_eq!(tif.captured_at.as_deref(), Some("2010-01-02T03:04:05"));
    }

    #[test]
    fn video_without_ffprobe_is_partial() {
        let mut x = Extractor::new(None, None);
        let m = x.extract(&fixture("video.mp4"), Some(MediaKind::Video), false).unwrap().media.unwrap();
        assert_eq!(m.status, Status::Partial, "{m:?}");
        assert_eq!((m.width, m.height), (Some(320), Some(240)));
        assert!(m.duration_ms.unwrap() >= 1900);
        assert!(m.captured_at.is_some());
        // nom-exif reads the QuickTime location of .mov files (MP4 GPS needs ffprobe).
        let mov = x.extract(&fixture("video.mov"), Some(MediaKind::Video), false).unwrap().media.unwrap();
        assert!((mov.gps_lat.unwrap() - 59.3293).abs() < 1e-3, "{mov:?}");
        let mkv = x.extract(&fixture("video.mkv"), Some(MediaKind::Video), false).unwrap().media.unwrap();
        assert!(matches!(mkv.status, Status::Partial | Status::Ok), "{mkv:?}");
    }

    #[test]
    fn audio_tags() {
        let mut x = Extractor::new(None, None);
        let mp3 = x.extract(&fixture("audio.mp3"), Some(MediaKind::Audio), false).unwrap().media.unwrap();
        assert_eq!(mp3.status, Status::Ok, "{mp3:?}");
        assert_eq!(mp3.title.as_deref(), Some("Fixture Song"));
        assert_eq!(mp3.artist.as_deref(), Some("The Testers"));
        assert_eq!(mp3.album.as_deref(), Some("Sample Album"));
        assert_eq!(mp3.album_artist.as_deref(), Some("Various"));
        assert_eq!(mp3.track_no, Some(3));
        assert_eq!(mp3.year, Some(1999));
        assert_eq!(mp3.genre.as_deref(), Some("Jazz"));
        assert!(mp3.duration_ms.unwrap() > 500);
        let flac = x.extract(&fixture("audio.flac"), Some(MediaKind::Audio), false).unwrap().media.unwrap();
        assert_eq!(flac.title.as_deref(), Some("Flac Song"));
        assert_eq!(flac.year, Some(2005));
        assert_eq!(flac.streams[0].sample_rate, Some(44100));
        let m4a = x.extract(&fixture("audio.m4a"), Some(MediaKind::Audio), false).unwrap().media.unwrap();
        assert_eq!(m4a.artist.as_deref(), Some("Apple Fan"), "{m4a:?}");
    }

    #[test]
    fn garbage_is_not_a_panic() {
        let tmp = std::env::temp_dir().join(format!("di-media-garbage-{}.jpg", std::process::id()));
        std::fs::write(&tmp, b"\xff\xd8\xff\xe1 this is not really a jpeg").unwrap();
        let mut x = Extractor::new(None, None);
        let m = x.extract(&tmp, Some(MediaKind::Image), true).unwrap().media.unwrap();
        assert!(matches!(m.status, Status::Unsupported | Status::Error), "{m:?}");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn quick_hash_identity() {
        let dir = std::env::temp_dir().join(format!("di-media-qh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("a"), &big).unwrap();
        std::fs::write(dir.join("b"), &big).unwrap();
        let mut changed_middle = big.clone();
        changed_middle[200_000] ^= 1;
        std::fs::write(dir.join("c"), &changed_middle).unwrap();
        let mut changed_end = big.clone();
        *changed_end.last_mut().unwrap() ^= 1;
        std::fs::write(dir.join("d"), &changed_end).unwrap();
        let mut x = Extractor::new(None, None);
        let h = |n: &str, x: &mut Extractor| x.extract(&dir.join(n), None, true).unwrap().quick_hash.unwrap();
        let (a, b, c, d) = (h("a", &mut x), h("b", &mut x), h("c", &mut x), h("d", &mut x));
        assert_eq!(a, b);
        assert_eq!(a, c, "the quick hash only samples start and end");
        assert_ne!(a, d);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
