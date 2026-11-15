//! Environment and external-tool checks.
//!
//! Each check reports whether a component is *usable* (not merely present), what it
//! enables, and how to fix it on this host. Missing tools only degrade their own
//! feature; nothing here blocks scanning.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use di_platform::util::run;

const TIMEOUT: Duration = Duration::from_secs(10);
const TINY_MP4: &[u8] = include_bytes!("../assets/tiny.mp4");
const TINY_JPG: &[u8] = include_bytes!("../assets/tiny.jpg");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Works.
    Ok,
    /// Works with a limitation, or needs attention.
    Warn,
    /// Not installed / not available.
    Missing,
    /// Present but not working.
    Error,
    /// Informational only.
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub name: &'static str,
    pub status: Status,
    pub version: Option<String>,
    pub enables: &'static str,
    pub detail: Option<String>,
    /// How to fix it on this host, when not Ok.
    pub fix: Option<String>,
    /// Package to install (for the combined install command).
    #[serde(skip)]
    pub package: Option<&'static str>,
    /// Without this, drive-index cannot do its core job on this host.
    pub essential: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub host: String,
    pub os: String,
    pub installer: Option<String>,
    pub running_as_root: bool,
    pub checks: Vec<Check>,
    /// One command installing everything missing, if a package manager was found.
    pub install_command: Option<String>,
}

impl Report {
    pub fn essential_failures(&self) -> usize {
        self.checks.iter().filter(|c| c.essential && matches!(c.status, Status::Missing | Status::Error)).count()
    }
}

// -------------------------------------------------------------------------------------
// Package managers
// -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Installer {
    Brew,
    MacPorts,
    Apt,
    Dnf,
    Pacman,
}

impl Installer {
    fn detect() -> Option<Installer> {
        if cfg!(target_os = "macos") {
            if find_program("brew").is_some() {
                return Some(Installer::Brew);
            }
            if find_program("port").is_some() {
                return Some(Installer::MacPorts);
            }
            return None;
        }
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let field = |k: &str| {
            os_release
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .map(|v| v.trim_matches('"').to_ascii_lowercase())
                .unwrap_or_default()
        };
        let ids = format!("{} {}", field("ID"), field("ID_LIKE"));
        if ids.contains("debian") || ids.contains("ubuntu") {
            Some(Installer::Apt)
        } else if ids.contains("fedora") || ids.contains("rhel") {
            Some(Installer::Dnf)
        } else if ids.contains("arch") {
            Some(Installer::Pacman)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Installer::Brew => "Homebrew",
            Installer::MacPorts => "MacPorts",
            Installer::Apt => "apt",
            Installer::Dnf => "dnf",
            Installer::Pacman => "pacman",
        }
    }

    fn command(self, packages: &[&str]) -> String {
        let p = packages.join(" ");
        match self {
            Installer::Brew => format!("brew install {p}"),
            Installer::MacPorts => format!("sudo port install {p}"),
            Installer::Apt => format!("sudo apt install {p}"),
            Installer::Dnf => format!("sudo dnf install {p}"),
            Installer::Pacman => format!("sudo pacman -S {p}"),
        }
    }

    /// Package name for a tool.
    fn package(self, tool: &str) -> Option<&'static str> {
        use Installer::*;
        Some(match (tool, self) {
            ("smartctl", _) => "smartmontools",
            ("ffprobe", Dnf) => "ffmpeg-free",
            ("ffprobe", _) => "ffmpeg",
            ("exiftool", Brew) => "exiftool",
            ("exiftool", MacPorts) => "p5-image-exiftool",
            ("exiftool", Apt) => "libimage-exiftool-perl",
            ("exiftool", Dnf) => "perl-Image-ExifTool",
            ("exiftool", Pacman) => "perl-image-exiftool",
            ("udisks2", Apt | Dnf | Pacman) => "udisks2",
            ("ntfs-3g", Apt | Dnf | Pacman) => "ntfs-3g",
            _ => return None,
        })
    }
}

fn homepage(tool: &str) -> &'static str {
    match tool {
        "smartctl" => "https://www.smartmontools.org",
        "ffprobe" => "https://ffmpeg.org/download.html",
        "exiftool" => "https://exiftool.org",
        _ => "",
    }
}

// -------------------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------------------

/// Find a program on PATH, plus the usual locations a non-login PATH may miss.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/sbin", "/sbin", "/usr/bin", "/opt/local/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).lines().next().unwrap_or("").trim().to_string()
}

/// Write an embedded sample to a private temp file for a functional test.
fn temp_sample(ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let p = std::env::temp_dir().join(format!("drive-index-doctor-{}.{ext}", std::process::id()));
    std::fs::write(&p, bytes)?;
    Ok(p)
}

fn version_at_least(v: &str, major: u32, minor: u32) -> bool {
    let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().unwrap_or(0));
    let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (a, b) >= (major, minor)
}

struct Ctx {
    installer: Option<Installer>,
    root: bool,
}

impl Ctx {
    fn missing(&self, tool: &'static str) -> (Option<String>, Option<&'static str>) {
        match self.installer.and_then(|i| i.package(tool).map(|p| (i, p))) {
            Some((i, p)) => (Some(i.command(&[p])), Some(p)),
            None => (Some(format!("install {tool}: {}", homepage(tool))), None),
        }
    }
}

// -------------------------------------------------------------------------------------
// Checks
// -------------------------------------------------------------------------------------

fn check_smartctl(ctx: &Ctx) -> Check {
    let mut c = Check {
        id: "smartctl",
        name: "smartctl",
        status: Status::Missing,
        version: None,
        enables: "drive health (SMART), real disk serial behind USB bridges",
        detail: None,
        fix: None,
        package: None,
        essential: false,
    };
    let Some(path) = find_program("smartctl") else {
        (c.fix, c.package) = ctx.missing("smartctl");
        return c;
    };
    match run(path.to_str().unwrap_or("smartctl"), &["--version"], TIMEOUT) {
        Ok(out) => {
            let line = first_line(&out.stdout);
            let version = line.split_whitespace().nth(1).unwrap_or("").to_string();
            c.version = Some(version.clone());
            if !version_at_least(&version, 7, 0) {
                c.status = Status::Error;
                c.detail = Some("smartmontools 7.0 or newer is needed for JSON output".into());
                (c.fix, c.package) = ctx.missing("smartctl");
            } else if let Ok(cmd) = std::env::var("DRIVE_INDEX_SMARTCTL") {
                c.status = Status::Ok;
                c.detail = Some(format!("runs `{cmd}` (DRIVE_INDEX_SMARTCTL)"));
            } else if cfg!(target_os = "linux") && !ctx.root {
                match di_platform::smart::helper() {
                    Some(h) => {
                        c.status = Status::Ok;
                        c.detail = Some(format!("as a normal user through the setuid helper {}", h.display()));
                    }
                    None => {
                        c.status = Status::Warn;
                        c.detail = Some("reading SMART data needs root on Linux".into());
                        c.fix = Some(
                            "install the setuid helper drive-index-smart (README: \"SMART on Linux without root\"), or run drive-index with sudo"
                                .into(),
                        );
                    }
                }
            } else if cfg!(target_os = "macos") {
                c.status = Status::Ok;
                c.detail = Some("note: macOS cannot read SMART over USB without a third-party driver".into());
            } else {
                c.status = Status::Ok;
            }
        }
        Err(e) => {
            c.status = Status::Error;
            c.detail = Some(format!("{e:#}"));
        }
    }
    c
}

fn check_ffprobe(ctx: &Ctx) -> Check {
    let mut c = Check {
        id: "ffprobe",
        name: "ffprobe",
        status: Status::Missing,
        version: None,
        enables: "video stream info (codecs, frame rates, audio tracks)",
        detail: None,
        fix: None,
        package: None,
        essential: false,
    };
    let Some(path) = find_program("ffprobe") else {
        (c.fix, c.package) = ctx.missing("ffprobe");
        c.detail = Some("without it, videos get only duration, size and date".into());
        return c;
    };
    let prog = path.to_str().unwrap_or("ffprobe");
    match run(prog, &["-version"], TIMEOUT) {
        Ok(out) => c.version = first_line(&out.stdout).split_whitespace().nth(2).map(str::to_string),
        Err(e) => {
            c.status = Status::Error;
            c.detail = Some(format!("{e:#}"));
            return c;
        }
    }
    let result = temp_sample("mp4", TINY_MP4).map_err(anyhow::Error::from).and_then(|sample| {
        let r = run(prog, &["-v", "error", "-print_format", "json", "-show_streams", sample.to_str().unwrap_or("")], TIMEOUT);
        let _ = std::fs::remove_file(&sample);
        r
    });
    match result {
        Ok(out) => {
            let streams = serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .ok()
                .and_then(|v| v.get("streams").and_then(|s| s.as_array()).map(|a| a.len()))
                .unwrap_or(0);
            if streams > 0 {
                c.status = Status::Ok;
            } else {
                c.status = Status::Error;
                c.detail = Some("could not read the built-in test video".into());
            }
        }
        Err(e) => {
            c.status = Status::Error;
            c.detail = Some(format!("test video failed: {e:#}"));
        }
    }
    c
}

fn check_exiftool(ctx: &Ctx) -> Check {
    let mut c = Check {
        id: "exiftool",
        name: "exiftool",
        status: Status::Missing,
        version: None,
        enables: "rare photo formats (CRW, MRW, X3F, …); fallback when built-in parsers fail",
        detail: None,
        fix: None,
        package: None,
        essential: false,
    };
    let Some(path) = find_program("exiftool") else {
        (c.fix, c.package) = ctx.missing("exiftool");
        return c;
    };
    let prog = path.to_str().unwrap_or("exiftool");
    match run(prog, &["-ver"], TIMEOUT) {
        Ok(out) => c.version = Some(first_line(&out.stdout)),
        Err(e) => {
            c.status = Status::Error;
            c.detail = Some(format!("{e:#}"));
            return c;
        }
    }
    let result = temp_sample("jpg", TINY_JPG).map_err(anyhow::Error::from).and_then(|sample| {
        let r = run(prog, &["-json", "-Make", sample.to_str().unwrap_or("")], TIMEOUT);
        let _ = std::fs::remove_file(&sample);
        r
    });
    match result {
        Ok(out) if String::from_utf8_lossy(&out.stdout).contains("DriveIndex") => c.status = Status::Ok,
        Ok(_) => {
            c.status = Status::Error;
            c.detail = Some("could not read EXIF from the built-in test photo".into());
        }
        Err(e) => {
            c.status = Status::Error;
            c.detail = Some(format!("test photo failed: {e:#}"));
        }
    }
    c
}

fn check_privileges(ctx: &Ctx) -> Check {
    Check {
        id: "privileges",
        name: "privileges",
        status: Status::Info,
        version: None,
        enables: "raw partition-table copies",
        detail: Some(if ctx.root { "running as root".into() } else { "running as a normal user".into() }),
        fix: None,
        package: None,
        essential: false,
    }
}

#[cfg(target_os = "macos")]
fn platform_checks(_ctx: &Ctx) -> Vec<Check> {
    let mut out = Vec::new();

    let mut du = Check {
        id: "diskutil",
        name: "diskutil",
        status: Status::Ok,
        version: None,
        enables: "drive details, mounting and ejecting",
        detail: None,
        fix: None,
        package: None,
        essential: true,
    };
    if let Err(e) = run("/usr/sbin/diskutil", &["info", "-plist", "/"], TIMEOUT) {
        du.status = Status::Error;
        du.detail = Some(format!("{e:#}"));
    }
    out.push(du);

    // Full Disk Access: only processes with FDA can open the TCC database.
    let tcc = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Application Support/com.apple.TCC/TCC.db");
    let fda = std::fs::File::open(&tcc).is_ok();
    let terminal = std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "your terminal app".into());
    let mut access = Check {
        id: "volume_access",
        name: "Removable Volumes",
        status: Status::Ok,
        version: None,
        enables: "reading files on external drives",
        detail: None,
        fix: None,
        package: None,
        essential: true,
    };
    if fda {
        access.detail = Some("Full Disk Access granted".into());
    } else {
        match external_volume_access() {
            Some((vol, true)) => access.detail = Some(format!("readable ({})", vol.display())),
            Some((vol, false)) => {
                access.status = Status::Error;
                access.detail = Some(format!("macOS denied access to {}", vol.display()));
                access.fix = Some(format!(
                    "System Settings › Privacy & Security › Files & Folders: allow {terminal} \"Removable Volumes\" (or grant Full Disk Access)"
                ));
            }
            None => {
                access.status = Status::Warn;
                access.detail = Some("not checked: no external volume is mounted".into());
                access.fix = Some(format!(
                    "if scans fail with \"Operation not permitted\", allow {terminal} under Privacy & Security › Files & Folders › Removable Volumes"
                ));
            }
        }
    }
    out.push(access);
    out
}

/// First mounted external volume under /Volumes and whether we can list it.
#[cfg(target_os = "macos")]
fn external_volume_access() -> Option<(PathBuf, bool)> {
    use std::os::unix::fs::MetadataExt;
    let root_dev = std::fs::metadata("/").ok()?.dev();
    let volumes_dev = std::fs::metadata("/Volumes").ok()?.dev();
    for e in std::fs::read_dir("/Volumes").ok()?.flatten() {
        let p = e.path();
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.file_type().is_symlink() || md.dev() == root_dev || md.dev() == volumes_dev {
            continue;
        }
        let readable = match std::fs::read_dir(&p) {
            Ok(_) => true,
            Err(e) => e.raw_os_error() != Some(libc::EPERM) && e.raw_os_error() != Some(libc::EACCES),
        };
        return Some((p, readable));
    }
    None
}

#[cfg(target_os = "linux")]
fn platform_checks(ctx: &Ctx) -> Vec<Check> {
    use di_platform::AuthAnswer;
    let mut out = Vec::new();

    let mut ud = Check {
        id: "udisks2",
        name: "udisks2",
        status: Status::Ok,
        version: None,
        enables: "drive detection, mounting and power-off without root",
        detail: None,
        fix: None,
        package: None,
        essential: true,
    };
    match di_platform::udisks_version() {
        Ok(v) => ud.version = Some(v),
        Err(e) => {
            ud.status = Status::Missing;
            ud.detail = Some(format!("{e:#}"));
            let (fix, pkg) = ctx.missing("udisks2");
            ud.fix = fix.map(|f| format!("{f} && sudo systemctl enable --now udisks2"));
            ud.package = pkg;
        }
    }
    let have_udisks = ud.status == Status::Ok;
    out.push(ud);

    if have_udisks {
        let mut pk = Check {
            id: "polkit",
            name: "mount permission",
            status: Status::Ok,
            version: None,
            enables: "mounting drives read-only as a normal user",
            detail: None,
            fix: None,
            package: None,
            essential: true,
        };
        match di_platform::polkit_check("org.freedesktop.udisks2.filesystem-mount") {
            Ok(AuthAnswer::Yes) => pk.detail = Some("allowed".into()),
            Ok(AuthAnswer::NeedsAuthentication) => {
                pk.status = Status::Warn;
                pk.detail = Some("polkit asks for a password (typical over SSH)".into());
                pk.fix = Some("run in a local desktop session, as root, or install contrib/polkit/50-drive-index.rules (allows mounting for the plugdev group)".into());
            }
            Ok(AuthAnswer::No) => {
                pk.status = Status::Error;
                pk.detail = Some("polkit denies mounting for this user".into());
                pk.fix = Some("install contrib/polkit/50-drive-index.rules, or run as root".into());
            }
            Err(e) if ctx.root => pk.detail = Some(format!("root (polkit not consulted): {e:#}")),
            Err(e) => {
                pk.status = Status::Warn;
                pk.detail = Some(format!("could not ask polkit: {e:#}"));
            }
        }
        out.push(pk);
    }

    // Filesystem drivers.
    let loaded: std::collections::HashSet<String> = std::fs::read_to_string("/proc/filesystems")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().last().map(str::to_string))
        .collect();
    let module_available = |m: &str| {
        loaded.contains(m)
            || run("/usr/sbin/modprobe", &["-n", "-q", m], TIMEOUT).is_ok()
            || run("/sbin/modprobe", &["-n", "-q", m], TIMEOUT).is_ok()
    };
    for (fs, label) in [("vfat", "FAT"), ("exfat", "exFAT"), ("hfsplus", "HFS+")] {
        let ok = module_available(fs);
        out.push(Check {
            id: "fs_driver",
            name: label_static(label),
            status: if ok { Status::Ok } else { Status::Missing },
            version: None,
            enables: "scanning drives with this filesystem",
            detail: Some(if ok { "kernel driver available".into() } else { format!("no {fs} kernel module") }),
            fix: (!ok).then(|| "install your distribution's extra kernel modules package".into()),
            package: None,
            essential: false,
        });
    }
    let ntfs3 = module_available("ntfs3");
    let ntfs3g = find_program("ntfs-3g").is_some();
    let (fix, pkg) = if ntfs3 || ntfs3g { (None, None) } else { ctx.missing("ntfs-3g") };
    out.push(Check {
        id: "fs_driver",
        name: "NTFS",
        status: if ntfs3 || ntfs3g { Status::Ok } else { Status::Missing },
        version: None,
        enables: "scanning Windows drives",
        detail: Some(match (ntfs3, ntfs3g) {
            (true, _) => "ntfs3 kernel driver available".into(),
            (false, true) => "ntfs-3g (FUSE, slower metadata) available".into(),
            _ => "no NTFS driver".into(),
        }),
        fix,
        package: pkg,
        essential: false,
    });
    let apfs = loaded.contains("apfs") || module_available("apfs") || find_program("apfs-fuse").is_some();
    out.push(Check {
        id: "fs_driver",
        name: "APFS",
        status: if apfs { Status::Ok } else { Status::Info },
        version: None,
        enables: "scanning Mac drives on this host",
        detail: Some(if apfs { "APFS driver available".into() } else { "no APFS driver: scan Mac drives on the Mac".into() }),
        fix: (!apfs).then(|| "optional: install apfs-fuse or the linux-apfs-rw module".into()),
        package: None,
        essential: false,
    });
    out
}

#[cfg(target_os = "linux")]
fn label_static(s: &str) -> &'static str {
    match s {
        "FAT" => "FAT",
        "exFAT" => "exFAT",
        _ => "HFS+",
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_checks(_ctx: &Ctx) -> Vec<Check> {
    Vec::new()
}

/// Run every check (in parallel) and assemble the report.
pub fn run_checks(host: &str) -> Report {
    let installer = Installer::detect();
    let root = unsafe { libc::geteuid() } == 0;
    let ctx = Ctx { installer, root };
    let (platform, tools) = std::thread::scope(|s| {
        let platform = s.spawn(|| platform_checks(&ctx));
        let smart = s.spawn(|| check_smartctl(&ctx));
        let ff = s.spawn(|| check_ffprobe(&ctx));
        let exif = s.spawn(|| check_exiftool(&ctx));
        let tools: Vec<Check> = [smart, ff, exif].into_iter().filter_map(|h| h.join().ok()).collect();
        (platform.join().unwrap_or_default(), tools)
    });
    let mut checks = platform;
    checks.extend(tools);
    checks.push(check_privileges(&ctx));

    let mut packages: Vec<&str> = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Missing | Status::Error))
        .filter_map(|c| c.package)
        .collect();
    packages.dedup();
    let install_command = match (installer, packages.is_empty()) {
        (Some(i), false) => Some(i.command(&packages)),
        _ => None,
    };
    Report {
        host: host.to_string(),
        os: os_description(),
        installer: installer.map(|i| i.name().to_string()),
        running_as_root: root,
        checks,
        install_command,
    }
}

fn os_description() -> String {
    if cfg!(target_os = "macos") {
        let v = run("/usr/bin/sw_vers", &["-productVersion"], TIMEOUT).map(|o| first_line(&o.stdout)).unwrap_or_default();
        format!("macOS {v} {}", std::env::consts::ARCH)
    } else {
        let pretty = std::fs::read_to_string("/etc/os-release")
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
            .unwrap_or_else(|| "Linux".into());
        format!("{pretty} {}", std::env::consts::ARCH)
    }
}

/// Is `path` writable (for the catalog directory check)?
pub fn writable_dir(path: &Path) -> bool {
    let probe = path.join(format!(".drive-index-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_at_least("7.5", 7, 0));
        assert!(version_at_least("7.0", 7, 0));
        assert!(!version_at_least("6.6", 7, 0));
        assert!(version_at_least("n7.1-2", 7, 0));
    }

    #[test]
    fn package_names() {
        assert_eq!(Installer::Apt.package("exiftool"), Some("libimage-exiftool-perl"));
        assert_eq!(Installer::Dnf.package("ffprobe"), Some("ffmpeg-free"));
        assert_eq!(Installer::Brew.command(&["ffmpeg", "exiftool"]), "brew install ffmpeg exiftool");
    }

    /// With PATH and the extra dirs unable to find a tool, the check reports Missing
    /// with a fix; this exercises the code path without depending on the host.
    #[test]
    fn missing_tool_has_fix() {
        let ctx = Ctx { installer: Some(Installer::Brew), root: false };
        let (fix, pkg) = ctx.missing("exiftool");
        assert_eq!(fix.as_deref(), Some("brew install exiftool"));
        assert_eq!(pkg, Some("exiftool"));
        let ctx = Ctx { installer: None, root: false };
        assert!(ctx.missing("smartctl").0.unwrap().contains("smartmontools.org"));
    }

    #[test]
    fn report_runs() {
        let r = run_checks("test-host");
        assert!(r.checks.iter().any(|c| c.id == "smartctl"));
        assert!(r.checks.iter().any(|c| c.id == "privileges"));
    }
}
