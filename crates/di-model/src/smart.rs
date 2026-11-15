//! SMART health as read by smartctl, in a form shared by the
//! platform layer (which reads it), the catalog (which stores snapshots) and the UI.

use crate::units::{format_count, format_size};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SmartReport {
    /// The smartctl `-d` type that worked (`auto` when none was needed).
    pub device_type: String,
    /// ATA, NVMe, SCSI.
    pub protocol: Option<String>,
    /// The disk itself, which may differ from what a USB bridge reports.
    pub model: Option<String>,
    pub serial: Option<String>,
    pub wwn: Option<String>,
    pub firmware: Option<String>,
    /// 0 for solid state.
    pub rotation_rpm: Option<u32>,
    pub passed: Option<bool>,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<i64>,
    pub power_cycles: Option<i64>,
    /// ATA attribute 5.
    pub reallocated_sectors: Option<i64>,
    /// ATA attribute 197.
    pub pending_sectors: Option<i64>,
    /// ATA attribute 198.
    pub offline_uncorrectable: Option<i64>,
    /// ATA attribute 199 (cabling/bridge trouble rather than the disk).
    pub udma_crc_errors: Option<i64>,
    pub nvme_percentage_used: Option<i64>,
    pub nvme_available_spare: Option<i64>,
    pub nvme_media_errors: Option<i64>,
    pub nvme_unsafe_shutdowns: Option<i64>,
    pub data_written_bytes: Option<i64>,
    /// smartctl's JSON, verbatim.
    pub raw_json: String,
}

impl SmartReport {
    /// One line: `PASSED · 31 °C · 12,431 h · realloc 0 · pending 0`.
    pub fn summary(&self) -> String {
        let mut parts = vec![match self.passed {
            Some(true) => "PASSED".to_string(),
            Some(false) => "FAILING".to_string(),
            None => "health unknown".to_string(),
        }];
        if let Some(t) = self.temperature_c {
            parts.push(format!("{t} °C"));
        }
        if let Some(h) = self.power_on_hours {
            parts.push(format!("{} h", format_count(h.max(0) as u64)));
        }
        let mut num = |label: &str, v: Option<i64>| {
            if let Some(v) = v {
                parts.push(format!("{label} {}", format_count(v.max(0) as u64)));
            }
        };
        num("realloc", self.reallocated_sectors);
        num("pending", self.pending_sectors);
        num("uncorrectable", self.offline_uncorrectable);
        if let Some(u) = self.nvme_percentage_used {
            parts.push(format!("{u}% used"));
        }
        if let Some(s) = self.nvme_available_spare {
            parts.push(format!("spare {s}%"));
        }
        if let Some(e) = self.nvme_media_errors {
            parts.push(format!("media errors {}", format_count(e.max(0) as u64)));
        }
        if let Some(w) = self.data_written_bytes {
            parts.push(format!("{} written", format_size(w.max(0) as u64)));
        }
        parts.join(" · ")
    }

    /// Things worth a warning, worst first.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.passed == Some(false) {
            w.push("the drive reports that SMART health checks FAILED: copy its data elsewhere".to_string());
        }
        let mut nonzero = |v: Option<i64>, what: &str| {
            if let Some(n) = v.filter(|n| *n > 0) {
                w.push(format!("{} {what}", format_count(n as u64)));
            }
        };
        nonzero(self.reallocated_sectors, "reallocated sectors");
        nonzero(self.pending_sectors, "sectors pending reallocation");
        nonzero(self.offline_uncorrectable, "uncorrectable sectors");
        nonzero(self.nvme_media_errors, "NVMe media errors");
        if let Some(n) = self.udma_crc_errors.filter(|n| *n > 0) {
            w.push(format!("{} interface CRC errors (often the cable or USB bridge)", format_count(n as u64)));
        }
        if let Some(u) = self.nvme_percentage_used.filter(|u| *u >= 90) {
            w.push(format!("{u}% of rated endurance used"));
        }
        if let Some(s) = self.nvme_available_spare.filter(|s| *s < 20) {
            w.push(format!("only {s}% spare blocks left"));
        }
        if let Some(t) = self.temperature_c.filter(|t| *t >= 60) {
            w.push(format!("running hot ({t} °C)"));
        }
        w
    }
}

/// Why SMART could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartUnavailable {
    NotInstalled,
    NeedsRoot(String),
    /// The connection does not pass SMART commands through (most USB on macOS).
    Unsupported(String),
    Failed(String),
}

impl SmartUnavailable {
    pub fn reason(&self) -> String {
        match self {
            SmartUnavailable::NotInstalled => "smartctl is not installed (package smartmontools)".into(),
            SmartUnavailable::NeedsRoot(m) => format!("needs root: {m}"),
            SmartUnavailable::Unsupported(m) => m.clone(),
            SmartUnavailable::Failed(m) => m.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_and_warnings() {
        let r = SmartReport {
            passed: Some(true),
            temperature_c: Some(31),
            power_on_hours: Some(12431),
            reallocated_sectors: Some(0),
            pending_sectors: Some(8),
            udma_crc_errors: Some(3),
            ..Default::default()
        };
        assert_eq!(r.summary(), "PASSED · 31 °C · 12,431 h · realloc 0 · pending 8");
        assert_eq!(r.warnings().len(), 2);
        assert!(r.warnings()[0].contains("8 sectors pending"));
    }
}
