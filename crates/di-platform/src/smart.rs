//! Reading SMART with `smartctl -j`.
//!
//! USB bridges need the right `-d` type; smartctl's own database usually picks it
//! (`auto`), and on Linux we fall back through the common bridge types. macOS
//! cannot pass SMART through USB bridges without a third-party driver.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use di_model::smart::{SmartReport, SmartUnavailable};

use crate::{Bus, DriveProbe};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Bridge types tried after `auto` on Linux, most common first.
const USB_TYPES: &[&str] = &["sat", "sntjmicron", "sntasmedia", "sntrealtek", "usbjmicron", "usbcypress", "usbprolific", "usbsunplus"];

/// The smartctl command: `DRIVE_INDEX_SMARTCTL` (e.g. `sudo -n smartctl`); on Linux
/// as a normal user the setuid helper `drive-index-smart` if installed; else smartctl
/// from PATH or the usual sbin directories.
pub fn command() -> Option<Vec<String>> {
    if let Ok(c) = std::env::var("DRIVE_INDEX_SMARTCTL") {
        let parts: Vec<String> = c.split_whitespace().map(str::to_string).collect();
        if !parts.is_empty() {
            return Some(parts);
        }
    }
    let smartctl = locate("smartctl")?;
    if cfg!(target_os = "linux") && !is_root() {
        if let Some(h) = helper() {
            return Some(vec![h.display().to_string()]);
        }
    }
    Some(vec![smartctl.display().to_string()])
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The setuid helper, if installed usably: `drive-index-smart` next to this program,
/// on PATH, or in /usr/local/bin, /usr/local/libexec or /usr/libexec; owned by root
/// with the setuid bit, and executable by this user.
pub fn helper() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    const NAME: &str = "drive-index-smart";
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(NAME)));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let fixed = ["/usr/local/bin", "/usr/local/libexec", "/usr/libexec", "/usr/bin"];
    beside
        .into_iter()
        .chain(std::env::split_paths(&path).map(|d| d.join(NAME)))
        .chain(fixed.iter().map(|d| Path::new(d).join(NAME)))
        .find(|p| {
            let setuid_root = std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.uid() == 0 && m.mode() & 0o4000 != 0);
            setuid_root && executable_by_me(p)
        })
}

fn executable_by_me(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

fn locate(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let extra = ["/usr/sbin", "/sbin", "/usr/local/sbin", "/usr/local/bin", "/opt/homebrew/sbin", "/opt/homebrew/bin", "/opt/local/sbin"];
    std::env::split_paths(&path)
        .chain(extra.iter().map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Why one smartctl attempt did not produce a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Permission(String),
    /// Wrong or unknown device type: try the next one.
    NoData(String),
}

fn messages(v: &Value) -> Vec<String> {
    v.pointer("/smartctl/messages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.get("string").and_then(Value::as_str)).map(str::to_string).collect())
        .unwrap_or_default()
}

fn int(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_i64)
}

fn text(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Parse `smartctl -j` output. `device_type` is the `-d` value used.
pub fn parse(json: &str, device_type: &str) -> Result<SmartReport, Attempt> {
    let v: Value = serde_json::from_str(json).map_err(|e| Attempt::NoData(format!("unreadable smartctl output: {e}")))?;
    let exit = int(&v, "/smartctl/exit_status").unwrap_or(0);
    let msgs = messages(&v);
    let joined = msgs.join("; ");
    let has_data = v.get("smart_status").is_some()
        || v.get("ata_smart_attributes").is_some()
        || v.get("nvme_smart_health_information_log").is_some();
    // Bits 0 and 1: command line error, or the device could not be opened/identified.
    if exit & 3 != 0 || !has_data {
        let lower = joined.to_ascii_lowercase();
        if lower.contains("permission denied") || lower.contains("operation not permitted") {
            return Err(Attempt::Permission(joined));
        }
        return Err(Attempt::NoData(if joined.is_empty() { format!("smartctl exit status {exit}, no SMART data") } else { joined }));
    }

    let attr = |id: i64| -> Option<i64> {
        v.pointer("/ata_smart_attributes/table")?
            .as_array()?
            .iter()
            .find(|a| a.get("id").and_then(Value::as_i64) == Some(id))
            .and_then(|a| a.pointer("/raw/value").and_then(Value::as_i64))
    };
    let nvme = |k: &str| int(&v, &format!("/nvme_smart_health_information_log/{k}"));
    let wwn = match (int(&v, "/wwn/naa"), int(&v, "/wwn/oui"), int(&v, "/wwn/id")) {
        (Some(naa), Some(oui), Some(id)) => Some(format!("0x{naa:x}{oui:06x}{id:09x}")),
        _ => None,
    };
    let block = int(&v, "/logical_block_size").unwrap_or(512);
    let data_written = nvme("data_units_written").map(|u| u * 512_000).or_else(|| attr(241).map(|lbas| lbas * block));
    Ok(SmartReport {
        device_type: device_type.to_string(),
        protocol: text(&v, "/device/protocol"),
        model: text(&v, "/model_name"),
        serial: text(&v, "/serial_number"),
        wwn,
        firmware: text(&v, "/firmware_version"),
        rotation_rpm: int(&v, "/rotation_rate").and_then(|r| u32::try_from(r).ok()),
        passed: v.pointer("/smart_status/passed").and_then(Value::as_bool),
        temperature_c: int(&v, "/temperature/current").or_else(|| nvme("temperature")),
        power_on_hours: int(&v, "/power_on_time/hours").or_else(|| nvme("power_on_hours")),
        power_cycles: int(&v, "/power_cycle_count").or_else(|| nvme("power_cycles")),
        reallocated_sectors: attr(5),
        pending_sectors: attr(197),
        offline_uncorrectable: attr(198),
        udma_crc_errors: attr(199),
        nvme_percentage_used: nvme("percentage_used"),
        nvme_available_spare: nvme("available_spare"),
        nvme_media_errors: nvme("media_errors"),
        nvme_unsafe_shutdowns: nvme("unsafe_shutdowns"),
        data_written_bytes: data_written,
        raw_json: json.to_string(),
    })
}

/// Device types to try for this drive.
fn types_for(probe: &DriveProbe, cached: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = cached.map(|c| vec![c.to_string()]).unwrap_or_default();
    let mut add = |t: &str| {
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    };
    add("auto");
    if probe.bus == Bus::Usb && cfg!(target_os = "linux") {
        for t in USB_TYPES {
            add(t);
        }
    }
    out
}

/// Read SMART for a probed drive. `cached` is the device type that worked last time.
pub fn read(probe: &DriveProbe, cached: Option<&str>) -> Result<SmartReport, SmartUnavailable> {
    match probe.bus {
        Bus::Image => return Err(SmartUnavailable::Unsupported("disk images have no SMART".into())),
        Bus::Sd => return Err(SmartUnavailable::Unsupported("SD cards have no SMART".into())),
        _ => {}
    }
    let cmd = command().ok_or(SmartUnavailable::NotInstalled)?;
    let mut last = String::new();
    for t in types_for(probe, cached) {
        let mut args: Vec<&str> = cmd[1..].iter().map(String::as_str).collect();
        args.extend(["-j", "-i", "-H", "-A"]);
        if t != "auto" {
            args.extend(["-d", t.as_str()]);
        }
        args.push(&probe.dev_path);
        let out = match crate::util::run_status(&cmd[0], &args, TIMEOUT) {
            Ok((_, stdout)) => stdout,
            Err(e) => {
                last = format!("{e:#}");
                continue;
            }
        };
        match parse(&String::from_utf8_lossy(&out), &t) {
            Ok(r) => return Ok(r),
            Err(Attempt::Permission(m)) => return Err(SmartUnavailable::NeedsRoot(m)),
            Err(Attempt::NoData(m)) => last = m,
        }
    }
    if probe.bus == Bus::Usb && cfg!(target_os = "macos") {
        return Err(SmartUnavailable::Unsupported("SMART is not available over USB on macOS".into()));
    }
    if probe.bus == Bus::Usb {
        return Err(SmartUnavailable::Unsupported(format!("the USB bridge does not pass SMART through ({last})")));
    }
    Err(SmartUnavailable::Failed(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
    }

    #[test]
    fn nvme() {
        let r = parse(&fixture("smart-nvme.json"), "auto").unwrap();
        assert_eq!(r.protocol.as_deref(), Some("NVMe"));
        assert_eq!(r.serial.as_deref(), Some("TESTSERIAL0001"));
        assert_eq!(r.passed, Some(true));
        assert_eq!(r.temperature_c, Some(30));
        assert_eq!(r.power_on_hours, Some(1837));
        assert_eq!(r.power_cycles, Some(406));
        assert_eq!(r.nvme_percentage_used, Some(3));
        assert_eq!(r.nvme_available_spare, Some(100));
        assert_eq!(r.nvme_unsafe_shutdowns, Some(22));
        assert_eq!(r.data_written_bytes, Some(93_402_697 * 512_000));
        assert!(r.warnings().is_empty());
    }

    #[test]
    fn ata_behind_a_bridge() {
        let r = parse(&fixture("smart-ata.json"), "sat").unwrap();
        assert_eq!(r.device_type, "sat");
        assert_eq!(r.serial.as_deref(), Some("WD-TEST1234"));
        assert!(r.model.as_deref().unwrap().starts_with("WDC WD20SPZX"));
        assert!(r.wwn.as_deref().unwrap().starts_with("0x5"), "{:?}", r.wwn);
        assert_eq!(r.rotation_rpm, Some(5400));
        assert_eq!((r.reallocated_sectors, r.pending_sectors, r.offline_uncorrectable, r.udma_crc_errors), (Some(0), Some(8), Some(0), Some(3)));
        assert_eq!(r.power_on_hours, Some(12431));
        assert_eq!(r.warnings().len(), 2, "{:?}", r.warnings());
    }

    #[test]
    fn failures() {
        let denied = r#"{"smartctl":{"exit_status":2,"messages":[{"string":"Smartctl open device: /dev/sdb failed: Permission denied","severity":"error"}]}}"#;
        assert!(matches!(parse(denied, "auto"), Err(Attempt::Permission(_))));
        let bridge = r#"{"smartctl":{"exit_status":1,"messages":[{"string":"/dev/sdb: Unknown USB bridge [0x152d:0x0578 (0x214)]","severity":"error"}]}}"#;
        assert!(matches!(parse(bridge, "auto"), Err(Attempt::NoData(m)) if m.contains("Unknown USB bridge")));
        assert!(matches!(parse("not json", "auto"), Err(Attempt::NoData(_))));
    }
}
