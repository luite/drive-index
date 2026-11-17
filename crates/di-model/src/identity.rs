//! Drive identifiers and match scoring, and the disk facts that rule a match out.

use crate::units::{format_count, format_size};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identifier {
    pub kind: String,
    pub value: String,
    pub strength: i64,
}

pub const WWN: i64 = 100;
pub const DISK_SERIAL: i64 = 90;
pub const PART_UUID: i64 = 60;
pub const PTABLE_UUID: i64 = 60;
pub const USB_SERIAL: i64 = 40;
pub const FS_UUID: i64 = 30;

/// Minimum score for a confident automatic match.
pub const MATCH_THRESHOLD: i64 = 60;

/// Identifier kinds of the enclosure or adapter a disk is in rather than of the disk:
/// every disk put into the same dock shares them. On their own they are never enough
/// to recognise a drive. See Note [Disk facts].
pub fn is_enclosure_kind(kind: &str) -> bool {
    kind == "usb_serial"
}

/// Score candidate drives from identifier hits `(drive_id, identifier)`: per drive, the
/// maximum strength per kind, summed over kinds, so several weak identifiers of one
/// kind (three filesystem UUIDs) do not add up to a strong match. Sorted best first.
pub fn score<'a>(hits: impl IntoIterator<Item = (i64, &'a Identifier)>) -> Vec<(i64, i64)> {
    use std::collections::HashMap;
    let mut per: HashMap<i64, HashMap<&str, i64>> = HashMap::new();
    for (drive, id) in hits {
        let e = per.entry(drive).or_default().entry(id.kind.as_str()).or_insert(0);
        *e = (*e).max(id.strength);
    }
    let mut scores: Vec<(i64, i64)> = per.into_iter().map(|(d, kinds)| (d, kinds.values().sum())).collect();
    scores.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scores
}

// Note [Disk facts]
// ~~~~~~~~~~~~~~~~~
// Identifiers are evidence *for* a match: a connected drive is scored against every
// known drive it shares identifiers with. Disk facts are evidence *against* one. They
// are properties of the physical disk that no enclosure, host, repartitioning or
// reformatting changes:
//
//   * capacity in bytes;
//   * model, which includes the brand ("WDC WD3200JS-00PDB0", "ST3160023AS");
//   * serial number;
//   * WWN.
//
// When both sides know a fact and the values differ, they are different disks, however
// many identifiers they share. The known drive is not a candidate at all: not matched,
// not offered as a choice, and the catalog refuses to record one as the other.
//
// Example: two disks swapped in one USB dock share the dock's USB serial
// (usb_serial 152d:0578:0000000047E5). Without disk facts the second disk scores 40
// against the first and the user is asked whether it is the first; with them, 149 GiB
// against 298 GiB settles it.
//
// Model, serial and WWN count only when the disk itself reported them: its ATA or NVMe
// identify data (directly, or passed on by a USB bridge) or SMART. A bridge that hides
// the disk reports its own product name and serial instead, the same for every disk put
// into that enclosure, and those would rule out the same disk seen through another
// enclosure. They are not disk facts (`IdentitySource` in di-platform). Capacity always
// counts. The one known way for it to differ for the same disk is an old USB bridge that
// cannot address more than 2 TiB; a disk read through one is recorded as a separate
// drive (its partitions past the limit are unreadable there anyway).
//
// The comparison is lenient, so formatting differences between udev, udisks, IOKit and
// smartctl never split one disk in two: case, spaces and punctuation are ignored, a
// model is compatible with a model that contains it ("WD3200JS-00PDB0" and
// "WDC WD3200JS-00PDB0", SCSI vendor prefixes), and WWNs are compared without their
// "0x" / "eui." / "naa." prefixes, an NAA extension being allowed on either side.

/// Facts about a physical disk that never change. See Note [Disk facts].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskFacts {
    /// 0 when unknown.
    pub capacity_bytes: u64,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub wwn: Option<String>,
}

/// A disk fact with different values on two drives, which makes them different disks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// `capacity`, `model`, `serial` or `WWN`.
    pub fact: &'static str,
    /// The value on the drive `difference` was called on.
    pub this: String,
    /// The value on the other drive.
    pub other: String,
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} vs {}", self.fact, self.this, self.other)
    }
}

impl DiskFacts {
    /// The first fact proving `self` and `other` are different disks, if any. Unknown
    /// facts prove nothing.
    pub fn difference(&self, other: &DiskFacts) -> Option<Difference> {
        let diff = |fact, this: &str, other: &str| Some(Difference { fact, this: this.into(), other: other.into() });
        let (a, b) = (self.capacity_bytes, other.capacity_bytes);
        if a != 0 && b != 0 && a != b {
            let (sa, sb) = (format_size(a), format_size(b));
            return if sa == sb {
                diff("capacity", &format!("{} bytes", format_count(a)), &format!("{} bytes", format_count(b)))
            } else {
                diff("capacity", &sa, &sb)
            };
        }
        if let Some((a, b)) = known_pair(&self.wwn, &other.wwn, wwn_key)
            && !a.1.starts_with(&b.1)
            && !b.1.starts_with(&a.1)
        {
            return diff("WWN", a.0, b.0);
        }
        if let Some((a, b)) = known_pair(&self.serial, &other.serial, text_key)
            && a.1 != b.1
        {
            return diff("serial", a.0, b.0);
        }
        if let Some((a, b)) = known_pair(&self.model, &other.model, text_key)
            && !a.1.contains(&b.1)
            && !b.1.contains(&a.1)
        {
            return diff("model", a.0, b.0);
        }
        None
    }
}

/// Both values with their comparison keys, when both are known (a value whose key is
/// empty is unknown).
#[allow(clippy::type_complexity)]
fn known_pair<'a>(
    a: &'a Option<String>,
    b: &'a Option<String>,
    key: fn(&str) -> String,
) -> Option<((&'a str, String), (&'a str, String))> {
    let keyed = |v: &'a Option<String>| v.as_deref().map(|s| (s, key(s))).filter(|(_, k)| !k.is_empty());
    Some((keyed(a)?, keyed(b)?))
}

/// Letters and digits only, upper case.
fn text_key(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect()
}

/// Hex digits only, lower case, without a `0x` / `eui.` / `naa.` / `wwn-` prefix.
fn wwn_key(s: &str) -> String {
    let t = s.trim().to_ascii_lowercase();
    let t = ["0x", "eui.", "naa.", "wwn-"].iter().find_map(|p| t.strip_prefix(p)).unwrap_or(&t);
    t.chars().filter(char::is_ascii_hexdigit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_takes_max_per_kind() {
        let id = |k: &str, v: &str, s| Identifier { kind: k.into(), value: v.into(), strength: s };
        let (fs1, fs2, usb) = (id("fs_uuid", "A", FS_UUID), id("fs_uuid", "B", FS_UUID), id("usb_serial", "x", USB_SERIAL));
        let s = score([(1, &fs1), (1, &fs2), (1, &usb), (2, &fs1)]);
        assert_eq!(s, vec![(1, FS_UUID + USB_SERIAL), (2, FS_UUID)]);
    }

    fn facts(capacity: u64, model: Option<&str>, serial: Option<&str>, wwn: Option<&str>) -> DiskFacts {
        DiskFacts {
            capacity_bytes: capacity,
            model: model.map(Into::into),
            serial: serial.map(Into::into),
            wwn: wwn.map(Into::into),
        }
    }

    #[test]
    fn different_disks_in_one_dock() {
        let seagate = facts(160_041_885_696, Some("ST3160023AS"), Some("3JS2WMZV"), None);
        let wd = facts(320_072_933_376, Some("WDC WD3200JS-00PDB0"), Some("WD-WCAPD1049006"), None);
        let d = seagate.difference(&wd).expect("different disks");
        assert_eq!(d.fact, "capacity");
        assert_eq!(d.to_string(), "capacity 149 GiB vs 298 GiB");
        // Same capacity: the serial, then the model, still tell them apart.
        let twin = facts(160_041_885_696, Some("ST3160023AS"), Some("5JS2XXXX"), None);
        assert_eq!(seagate.difference(&twin).unwrap().fact, "serial");
        let other_model = facts(160_041_885_696, Some("WDC WD1600JS"), None, None);
        assert_eq!(seagate.difference(&other_model).unwrap().fact, "model");
    }

    #[test]
    fn close_capacities_are_shown_in_bytes() {
        let d = facts(2_000_398_934_016, None, None, None).difference(&facts(2_000_398_933_504, None, None, None));
        assert_eq!(d.unwrap().to_string(), "capacity 2,000,398,934,016 bytes vs 2,000,398,933,504 bytes");
    }

    #[test]
    fn one_disk_reported_differently_is_one_disk() {
        let udisks = facts(500_107_862_016, Some("WDC WD5000AAKX-00ERMA0"), Some("WD-WCC2EJF12345"), Some("0x50014ee2b5b3c7a1"));
        // smartctl, udev's underscored ID_MODEL, a SCSI-style vendor split, other case.
        let variants = [
            facts(500_107_862_016, Some("WDC WD5000AAKX-00ERMA0"), Some("WD-WCC2EJF12345"), Some("0x50014ee2b5b3c7a1")),
            facts(0, Some("WDC_WD5000AAKX-00ERMA0"), Some("  WD-WCC2EJF12345 "), None),
            facts(500_107_862_016, Some("WD5000AAKX-00ERMA0"), Some("wd-wcc2ejf12345"), Some("naa.50014EE2B5B3C7A1")),
        ];
        for v in &variants {
            assert_eq!(udisks.difference(v), None, "{v:?}");
            assert_eq!(v.difference(&udisks), None, "{v:?}");
        }
    }

    #[test]
    fn unknown_facts_prove_nothing() {
        let known = facts(1_000, Some("M"), Some("S"), Some("0x5"));
        assert_eq!(known.difference(&DiskFacts::default()), None);
        // Values without letters or digits count as unknown.
        assert_eq!(known.difference(&facts(1_000, Some(" "), Some("--"), Some("0x"))), None);
    }

    #[test]
    fn wwn_with_extension() {
        let a = facts(0, None, None, Some("0x6000c29d3f0e5a1b"));
        let b = facts(0, None, None, Some("0x6000c29d3f0e5a1b0000000000000001"));
        assert_eq!(a.difference(&b), None);
        let c = facts(0, None, None, Some("0x5000c500a1b2c3d4"));
        assert_eq!(a.difference(&c).unwrap().fact, "WWN");
    }
}
