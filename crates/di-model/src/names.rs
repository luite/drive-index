//! File-name handling: normalization, extensions and special directory names.

use unicode_normalization::{UnicodeNormalization, is_nfc_quick, IsNormalized};

/// A file name as stored in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredName {
    /// NFC-normalized UTF-8 (lossy if the on-disk bytes are not UTF-8).
    pub name: String,
    /// The exact on-disk bytes, only when they differ from `name`.
    pub raw: Option<Vec<u8>>,
}

/// Normalize raw on-disk name bytes for storage: lossy UTF-8, then NFC.
/// `raw` is kept only when the stored form differs from the original bytes.
pub fn normalize(raw: &[u8]) -> StoredName {
    // Fast path for the common case: ASCII is valid UTF-8 and already NFC.
    if raw.is_ascii() {
        // SAFETY: ASCII bytes are valid UTF-8.
        let name = unsafe { std::str::from_utf8_unchecked(raw) }.to_owned();
        return StoredName { name, raw: None };
    }
    match std::str::from_utf8(raw) {
        Ok(s) => {
            if matches!(is_nfc_quick(s.chars()), IsNormalized::Yes) {
                StoredName { name: s.to_owned(), raw: None }
            } else {
                let nfc: String = s.nfc().collect();
                let raw = (nfc.as_bytes() != raw).then(|| raw.to_vec());
                StoredName { name: nfc, raw }
            }
        }
        Err(_) => {
            let lossy = String::from_utf8_lossy(raw);
            let nfc: String = lossy.nfc().collect();
            StoredName { name: nfc, raw: Some(raw.to_vec()) }
        }
    }
}

/// Lowercase extension without the dot. `None` for dotfiles without a further dot
/// (`.bashrc`), names ending in a dot, and implausible "extensions" (too long or
/// containing spaces), which are usually just dots inside a name.
pub fn extension(name: &str) -> Option<String> {
    // AppleDouble companions (`._IMG_0001.CR2`) hold macOS metadata, not file content.
    if name.starts_with("._") {
        return None;
    }
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.chars().count() > 10 || ext.contains(char::is_whitespace) {
        return None;
    }
    Some(ext.to_lowercase())
}

/// True for directory names that hold deleted files (indexed, but flagged `in_trash`).
pub fn is_trash_dir(name: &str) -> bool {
    name == ".Trashes"
        || name == ".Trash"
        || name.starts_with(".Trash-")
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name.eq_ignore_ascii_case("RECYCLER")
}

/// Default directories whose contents are not indexed.
pub const DEFAULT_SKIP_DIRS: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    "System Volume Information",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_passthrough_and_conversion() {
        let n = normalize("IMG_0001.CR2".as_bytes());
        assert_eq!(n.name, "IMG_0001.CR2");
        assert_eq!(n.raw, None);

        // "é" as e + combining acute (NFD, as HFS+ stores it)
        let nfd = "caf\u{0065}\u{0301}.jpg";
        let n = normalize(nfd.as_bytes());
        assert_eq!(n.name, "caf\u{00e9}.jpg");
        assert_eq!(n.raw.as_deref(), Some(nfd.as_bytes()));
    }

    #[test]
    fn non_utf8_kept_raw() {
        let raw = b"bad\xffname";
        let n = normalize(raw);
        assert_eq!(n.name, "bad\u{fffd}name");
        assert_eq!(n.raw.as_deref(), Some(&raw[..]));
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("IMG_4411.CR2").as_deref(), Some("cr2"));
        assert_eq!(extension("archive.tar.gz").as_deref(), Some("gz"));
        assert_eq!(extension(".bashrc"), None);
        assert_eq!(extension("README"), None);
        assert_eq!(extension("trailing."), None);
        assert_eq!(extension("v1.0 final draft"), None);
        assert_eq!(extension(".config.json").as_deref(), Some("json"));
        assert_eq!(extension("._IMG_0001.CR2"), None);
    }

    #[test]
    fn trash_names() {
        assert!(is_trash_dir(".Trashes"));
        assert!(is_trash_dir(".Trash-1000"));
        assert!(is_trash_dir("$Recycle.Bin"));
        assert!(!is_trash_dir("Trash"));
    }
}
