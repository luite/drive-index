//! Byte-size parsing and formatting.
//!
//! Parsing follows the search language (PLAN.md §6): `K`/`M`/`G`/`T`/`P` (and `KiB`…)
//! are binary, `KB`/`MB`/… are decimal, a bare number is bytes.

pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        return Err(format!("invalid size '{input}'"));
    }
    let value: f64 = num.parse().map_err(|_| format!("invalid size '{input}'"))?;
    let unit = unit.trim();
    let mult: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024f64.powi(2),
        "g" | "gib" => 1024f64.powi(3),
        "t" | "tib" => 1024f64.powi(4),
        "p" | "pib" => 1024f64.powi(5),
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        _ => return Err(format!("unknown size unit '{unit}' in '{input}'")),
    };
    let bytes = value * mult;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("size out of range '{input}'"));
    }
    Ok(bytes.round() as u64)
}

/// Compact binary formatting: `512 B`, `24.1 KiB`, `2.0 TiB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Thousands separators for counts: `1,204,331`.
pub fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("10M"), Ok(10 * 1024 * 1024));
        assert_eq!(parse_size("10MB"), Ok(10_000_000));
        assert_eq!(parse_size("1.5k"), Ok(1536));
        assert_eq!(parse_size("2GiB"), Ok(2 * 1024 * 1024 * 1024));
        assert!(parse_size("ten").is_err());
        assert!(parse_size("10Q").is_err());
    }

    #[test]
    fn format() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(25_270_000), "24.1 MiB");
        assert_eq!(format_count(1_204_331), "1,204,331");
        assert_eq!(format_count(12), "12");
    }
}
