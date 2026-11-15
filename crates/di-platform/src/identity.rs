//! Drive identifiers (PLAN.md §3.3): every identifier a probe reveals, with the
//! strength used to recognise the drive again on a later connect or another host.

use crate::DriveProbe;

pub use di_model::identity::*;

/// Serial numbers that cheap bridges and card readers report for every unit.
fn is_junk_serial(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 4 {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    const KNOWN: &[&str] = &[
        "0123456789abcdef",
        "0123456789",
        "123456789012",
        "000000000000",
        "none",
        "unknown",
        "serial",
        "default string",
        "to be filled by o.e.m.",
    ];
    if KNOWN.contains(&lower.as_str()) {
        return true;
    }
    // All one repeated character (000000…, FFFFFFFF…).
    let first = lower.chars().next().unwrap();
    lower.chars().all(|c| c == first)
}

/// All identifiers of a probed drive.
pub fn identifiers(d: &DriveProbe) -> Vec<Identifier> {
    let mut out = Vec::new();
    let mut push = |kind: &str, value: String, strength: i64| {
        let value = value.trim().to_string();
        if !value.is_empty() && !out.iter().any(|i: &Identifier| i.kind == kind && i.value == value) {
            out.push(Identifier { kind: kind.to_string(), value, strength });
        }
    };

    if let Some(w) = d.wwn.as_deref().filter(|w| !is_junk_serial(w)) {
        push("wwn", w.to_ascii_lowercase(), WWN);
    }
    if let Some(s) = d.serial.as_deref().filter(|s| !is_junk_serial(s)) {
        let model = d.model.as_deref().unwrap_or("").trim();
        push("disk_serial", format!("{model}|{s}"), DISK_SERIAL);
    }
    if let Some(u) = d.ptable_uuid.as_deref() {
        push("ptable_uuid", u.to_ascii_uppercase(), PTABLE_UUID);
    }
    for p in &d.partitions {
        if let Some(u) = &p.part_uuid {
            push("part_uuid", u.to_ascii_uppercase(), PART_UUID);
        }
    }
    if let Some(usb) = &d.usb {
        if let Some(s) = usb.serial.as_deref().filter(|s| !is_junk_serial(s)) {
            let vid = usb.vendor_id.map(|v| format!("{v:04x}")).unwrap_or_default();
            let pid = usb.product_id.map(|v| format!("{v:04x}")).unwrap_or_default();
            push("usb_serial", format!("{vid}:{pid}:{s}"), USB_SERIAL);
        }
    }
    for v in &d.volumes {
        // EFI system partitions are formatted with fixed serials, so many drives share
        // their UUID (e.g. macOS's 0E239BC6-…); they identify nothing.
        let efi = v.partition_number.is_some_and(|n| {
            d.partitions.iter().any(|p| {
                p.number == n
                    && p.type_name.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("EFI") || t == "EFI System")
            })
        });
        if let Some(u) = v.fs_uuid.as_ref().filter(|_| !efi) {
            push("fs_uuid", u.to_ascii_uppercase(), FS_UUID);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_serials() {
        assert!(is_junk_serial("000000000000"));
        assert!(is_junk_serial("0123456789ABCDEF"));
        assert!(is_junk_serial("FFFFFFFF"));
        assert!(is_junk_serial("ab"));
        assert!(!is_junk_serial("WD-WX12A34B5678"));
    }
}
