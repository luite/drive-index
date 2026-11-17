//! SMART health as read by smartctl, in a form shared by the
//! platform layer (which reads it), the catalog (which stores snapshots) and the UI.

use crate::units::{format_count, format_size};

// Note [SMART details from smartctl's JSON]
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Two things that matter for reading a SMART report are only in smartctl's raw
// output, not in the columns we store per snapshot:
//
//  * where PASSED comes from. Normally it is the drive's own verdict (SMART RETURN
//    STATUS). Many USB bridges don't pass that through; smartctl then checks the
//    pre-failure attributes against their thresholds itself and says so in a message
//    ("This result is based on an Attribute check"). That check ignores old-age
//    attributes, so a drive can be PASSED while one of them is past its limit.
//  * attributes at or below their threshold (`when_failed` "now" or "past").
//
// Both are derived from `raw_json` by `details`, both when a report is read from
// smartctl and when a stored snapshot is loaded, so old snapshots get them too.

/// Where the PASSED/FAILED verdict comes from. See Note [SMART details from smartctl's JSON].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HealthSource {
    /// The drive's own assessment.
    #[default]
    Drive,
    /// smartctl's check of attribute thresholds, because the drive's own verdict did
    /// not get through (common behind USB bridges).
    Attributes,
}

impl HealthSource {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthSource::Drive => "drive",
            HealthSource::Attributes => "attributes",
        }
    }
}

/// An ATA attribute whose normalized value is (or was) at or below its threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailingAttribute {
    pub id: i64,
    pub name: String,
    /// Below its threshold now, rather than only at its worst in the past.
    pub now: bool,
    /// A pre-failure attribute (predicts failure) rather than an old-age one (wear).
    pub prefail: bool,
}

/// Health source and failing attributes from smartctl's JSON.
/// See Note [SMART details from smartctl's JSON].
pub fn details(json: &serde_json::Value) -> (HealthSource, Vec<FailingAttribute>) {
    use serde_json::Value;
    let by_attributes = json
        .pointer("/smartctl/messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("string").and_then(Value::as_str))
        .any(|s| s.contains("based on an Attribute check"));
    let failing = json
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let now = match a.get("when_failed").and_then(Value::as_str)? {
                "now" => true,
                "past" => false,
                _ => return None,
            };
            Some(FailingAttribute {
                id: a.get("id").and_then(Value::as_i64)?,
                name: a.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
                now,
                prefail: a.pointer("/flags/prefailure").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    (if by_attributes { HealthSource::Attributes } else { HealthSource::Drive }, failing)
}

/// `details` of a stored report's raw JSON (nothing if it is not valid JSON).
pub fn details_of_raw(raw_json: &str) -> (HealthSource, Vec<FailingAttribute>) {
    serde_json::from_str(raw_json).map(|v| details(&v)).unwrap_or_default()
}

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
    /// Where `passed` comes from.
    pub health_source: HealthSource,
    /// Attributes at or below their threshold, now or in the past.
    pub failing_attributes: Vec<FailingAttribute>,
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
        let source = match self.health_source {
            HealthSource::Drive => "",
            HealthSource::Attributes => " (attribute check only)",
        };
        let mut parts = vec![match self.passed {
            Some(true) => format!("PASSED{source}"),
            Some(false) => format!("FAILING{source}"),
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
        // Failing now before failed in the past; pre-failure before old-age.
        let mut failing: Vec<&FailingAttribute> = self.failing_attributes.iter().collect();
        failing.sort_by_key(|a| (!a.now, !a.prefail, a.id));
        for a in failing {
            w.push(match (a.now, a.prefail) {
                (true, true) => format!("{} is below its failure threshold: the drive predicts its own failure", a.name),
                (true, false) => format!("{} is below its threshold: worn past the drive's rated life", a.name),
                (false, _) => format!("{} was below its threshold in the past", a.name),
            });
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

    /// An old disk behind a USB bridge: smartctl inferred PASSED from the attributes,
    /// and an old-age attribute is below its threshold.
    #[test]
    fn verdict_from_attributes_and_failing_attributes() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
              "smartctl": {"messages": [{"string": "Warning: This result is based on an Attribute check.", "severity": "warning"}]},
              "smart_status": {"passed": true},
              "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "when_failed": "", "flags": {"prefailure": true}},
                {"id": 12, "name": "Power_Cycle_Count", "when_failed": "now", "flags": {"prefailure": false}},
                {"id": 199, "name": "UDMA_CRC_Error_Count", "when_failed": "past", "flags": {"prefailure": false}},
                {"id": 3, "name": "Spin_Up_Time", "when_failed": "now", "flags": {"prefailure": true}}
              ]}
            }"#,
        )
        .unwrap();
        let (source, failing) = details(&json);
        assert_eq!(source, HealthSource::Attributes);
        assert_eq!(failing.iter().map(|a| (a.id, a.now, a.prefail)).collect::<Vec<_>>(), vec![
            (12, true, false),
            (199, false, false),
            (3, true, true)
        ]);
        let r = SmartReport { passed: Some(true), health_source: source, failing_attributes: failing, ..Default::default() };
        assert_eq!(r.summary(), "PASSED (attribute check only)");
        let w = r.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].starts_with("Spin_Up_Time is below its failure threshold"), "{w:?}");
        assert!(w[1].starts_with("Power_Cycle_Count is below its threshold: worn"), "{w:?}");
        assert!(w[2].starts_with("UDMA_CRC_Error_Count was below"), "{w:?}");

        // A drive that answers for itself; and stored reports without JSON.
        let own: serde_json::Value = serde_json::from_str(r#"{"smart_status": {"passed": true}}"#).unwrap();
        assert_eq!(details(&own), (HealthSource::Drive, vec![]));
        assert_eq!(details_of_raw(""), (HealthSource::Drive, vec![]));
    }
}
