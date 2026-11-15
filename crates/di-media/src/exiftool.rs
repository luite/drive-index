//! A persistent `exiftool -stay_open` session: one process for many files.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::{MediaInfo, normalize_exif_datetime};

pub struct ExiftoolSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

const TAGS: &[&str] = &[
    "-DateTimeOriginal",
    "-CreateDate",
    "-OffsetTimeOriginal",
    "-Make",
    "-Model",
    "-SerialNumber",
    "-LensModel",
    "-ISO",
    "-ExposureTime",
    "-FNumber",
    "-FocalLength",
    "-Orientation",
    "-ImageWidth",
    "-ImageHeight",
    "-GPSLatitude",
    "-GPSLongitude",
    "-GPSAltitude",
];

impl ExiftoolSession {
    pub fn start(prog: &Path) -> std::io::Result<ExiftoolSession> {
        let mut child = Command::new(prog)
            .args(["-stay_open", "True", "-@", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(ExiftoolSession { child, stdin, stdout })
    }

    /// Selected tags of one file as a JSON object (numeric values with `-n`).
    pub fn query(&mut self, path: &Path) -> std::io::Result<serde_json::Value> {
        let mut cmd = String::from("-json\n-n\n-fast2\n");
        for t in TAGS {
            cmd.push_str(t);
            cmd.push('\n');
        }
        cmd.push_str(&path.to_string_lossy());
        cmd.push_str("\n-execute\n");
        self.stdin.write_all(cmd.as_bytes())?;
        self.stdin.flush()?;
        let mut out = String::new();
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                return Err(std::io::Error::other("exiftool exited"));
            }
            if line.trim_end() == "{ready}" {
                break;
            }
            out.push_str(&line);
        }
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or(serde_json::Value::Null);
        Ok(v.get(0).cloned().unwrap_or(serde_json::Value::Null))
    }
}

impl Drop for ExiftoolSession {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"-stay_open\nFalse\n");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

pub(crate) fn fill(m: &mut MediaInfo, j: &serde_json::Value) {
    let s = |k: &str| j.get(k).and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_f64().map(|n| n.to_string())));
    let n = |k: &str| j.get(k).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|x| x.parse().ok())));
    m.camera_make = s("Make");
    m.camera_model = s("Model");
    m.camera_serial = s("SerialNumber");
    m.lens_model = s("LensModel");
    m.iso = n("ISO").map(|v| v as u32);
    m.exposure_s = n("ExposureTime");
    m.f_number = n("FNumber");
    m.focal_mm = n("FocalLength");
    m.orientation = n("Orientation").map(|v| v as u32);
    m.width = n("ImageWidth").map(|v| v as u32);
    m.height = n("ImageHeight").map(|v| v as u32);
    m.gps_lat = n("GPSLatitude");
    m.gps_lon = n("GPSLongitude");
    m.gps_alt_m = n("GPSAltitude");
    m.captured_at = s("DateTimeOriginal").or_else(|| s("CreateDate")).and_then(|d| normalize_exif_datetime(&d));
    m.captured_offset_min = s("OffsetTimeOriginal").and_then(|o| parse_offset(&o));
}

/// `+01:00` → 60.
fn parse_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = s[1..].split_once(':')?;
    Some(sign * (h.parse::<i32>().ok()? * 60 + m.parse::<i32>().ok()?))
}
