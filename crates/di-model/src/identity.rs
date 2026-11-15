//! Drive identifiers and match scoring (PLAN.md §3.3).

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
}
