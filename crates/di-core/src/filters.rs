//! Reference for the search language: one entry per filter, used for help screens,
//! completion and "did you mean" suggestions.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FilterDoc {
    pub key: &'static str,
    pub group: &'static str,
    pub syntax: &'static str,
    pub help: &'static str,
    pub examples: &'static [&'static str],
    /// Fixed values for completion (dynamic ones come from the catalog).
    pub values: &'static [&'static str],
}

const YES_NO: &[&str] = &["yes", "no"];

pub const FILTERS: &[FilterDoc] = &[
    FilterDoc { key: "", group: "Names", syntax: "word  \"two words\"  glob*", help: "file name contains the text (case-insensitive); * and ? make it a glob", examples: &["iceland", "IMG_44*.CR2", "\"holiday 2014\""], values: &[] },
    FilterDoc { key: "name", group: "Names", syntax: "name:TEXT", help: "same as a bare word", examples: &["name:report"], values: &[] },
    FilterDoc { key: "path", group: "Names", syntax: "path:TEXT  path:/A/B/*", help: "folder path contains the text, or matches a glob", examples: &["path:DCIM", "path:/Photos/2014/*"], values: &[] },
    FilterDoc { key: "ext", group: "Type", syntax: "ext:A,B,…", help: "file extension(s)", examples: &["ext:cr2,crw", "ext:mkv"], values: &[] },
    FilterDoc { key: "type", group: "Type", syntax: "type:CATEGORY", help: "extension category", examples: &["type:photo_raw", "type:video,audio"], values: &[] },
    FilterDoc { key: "kind", group: "Type", syntax: "kind:file|symlink|…", help: "entry kind", examples: &["kind:symlink"], values: &["file", "symlink", "fifo", "socket", "char", "block", "other"] },
    FilterDoc { key: "in", group: "Type", syntax: "in:COLLECTION", help: "restrict to a saved collection", examples: &["in:photos-raw"], values: &[] },
    FilterDoc { key: "size", group: "Size & dates", syntax: "size:>10M  size:1G..4G  size:0", help: "size; K/M/G/T are binary, KB/MB/GB decimal", examples: &["size:>100M", "size:1G..4G"], values: &[] },
    FilterDoc { key: "date", group: "Size & dates", syntax: "date:2014  2014-07  2014-06..2014-09  >2020-01-01  <30d", help: "modification time (local); relative ages like 30d, 6m, 1y", examples: &["date:2014-07", "date:<30d"], values: &[] },
    FilterDoc { key: "mtime", group: "Size & dates", syntax: "mtime:…", help: "same as date:", examples: &["mtime:>2020"], values: &[] },
    FilterDoc { key: "btime", group: "Size & dates", syntax: "btime:…", help: "creation (birth) time", examples: &["btime:2019"], values: &[] },
    FilterDoc { key: "ctime", group: "Size & dates", syntax: "ctime:…", help: "status-change time", examples: &["ctime:<7d"], values: &[] },
    FilterDoc { key: "drive", group: "Where", syntax: "drive:D3  drive:\"label*\"  drive:SERIAL", help: "drive by id, label glob or any identifier", examples: &["drive:D3", "drive:\"Photos*\""], values: &[] },
    FilterDoc { key: "fs", group: "Where", syntax: "fs:LABEL|UUID", help: "filesystem by label glob or UUID", examples: &["fs:EOS_DIGITAL"], values: &[] },
    FilterDoc { key: "fstype", group: "Where", syntax: "fstype:apfs,ntfs,…", help: "filesystem type", examples: &["fstype:ntfs"], values: &["apfs", "hfsplus", "exfat", "vfat", "ntfs", "ext4", "btrfs", "xfs"] },
    FilterDoc { key: "taken", group: "Photos & video", syntax: "taken:2014-07  taken:2014-06..2014-09", help: "capture date from EXIF/QuickTime (camera's local time)", examples: &["taken:2014", "taken:<1y"], values: &[] },
    FilterDoc { key: "when", group: "Photos & video", syntax: "when:2014", help: "capture date if known, otherwise modification date", examples: &["when:2014-07"], values: &[] },
    FilterDoc { key: "camera", group: "Photos & video", syntax: "camera:TEXT  camera:\"EOS 5D*\"", help: "camera make/model", examples: &["camera:canon", "camera:\"iPhone 1*\""], values: &[] },
    FilterDoc { key: "lens", group: "Photos & video", syntax: "lens:TEXT", help: "lens model", examples: &["lens:\"24-105\""], values: &[] },
    FilterDoc { key: "iso", group: "Photos & video", syntax: "iso:>=3200", help: "ISO speed", examples: &["iso:>=3200"], values: &[] },
    FilterDoc { key: "aperture", group: "Photos & video", syntax: "aperture:<=2.8", help: "f-number", examples: &["aperture:<=2.8"], values: &[] },
    FilterDoc { key: "focal", group: "Photos & video", syntax: "focal:24..70", help: "focal length in mm", examples: &["focal:>200"], values: &[] },
    FilterDoc { key: "gps", group: "Photos & video", syntax: "gps:yes|no", help: "has a location", examples: &["gps:yes"], values: YES_NO },
    FilterDoc { key: "res", group: "Photos & video", syntax: "res:1080p  res:4k  res:>=3840x2160", help: "resolution (short side for 1080p/4k)", examples: &["res:4k", "res:<720p"], values: &["480p", "720p", "1080p", "1440p", "4k", "8k"] },
    FilterDoc { key: "width", group: "Photos & video", syntax: "width:>=6000", help: "pixel width", examples: &["width:>=6000"], values: &[] },
    FilterDoc { key: "height", group: "Photos & video", syntax: "height:>=4000", help: "pixel height", examples: &["height:>=4000"], values: &[] },
    FilterDoc { key: "duration", group: "Photos & video", syntax: "duration:>10m  <30s  1h..2h", help: "video/audio length", examples: &["duration:>1h"], values: &[] },
    FilterDoc { key: "codec", group: "Photos & video", syntax: "codec:hevc,h264,…", help: "any stream with this codec", examples: &["codec:hevc", "codec:prores"], values: &["h264", "hevc", "av1", "vp9", "prores", "mpeg4", "aac", "mp3", "flac", "opus", "ac3", "eac3", "dts"] },
    FilterDoc { key: "artist", group: "Music", syntax: "artist:TEXT", help: "artist or album artist", examples: &["artist:\"miles davis\""], values: &[] },
    FilterDoc { key: "album", group: "Music", syntax: "album:TEXT", help: "album", examples: &["album:\"kind of*\""], values: &[] },
    FilterDoc { key: "title", group: "Music", syntax: "title:TEXT", help: "track/video title", examples: &["title:intro"], values: &[] },
    FilterDoc { key: "genre", group: "Music", syntax: "genre:TEXT", help: "genre", examples: &["genre:jazz"], values: &[] },
    FilterDoc { key: "year", group: "Music", syntax: "year:1999  year:<2000", help: "year from audio tags", examples: &["year:1960..1969"], values: &[] },
    FilterDoc { key: "media", group: "Content", syntax: "media:none|ok|partial|unsupported|error|image|video|audio", help: "metadata state", examples: &["media:none type:photo", "media:unsupported"], values: &["none", "any", "ok", "partial", "unsupported", "error", "image", "video", "audio"] },
    FilterDoc { key: "dup", group: "Content", syntax: "dup:yes|no|exact|likely|probable", help: "has a copy: exact (full hash), likely (quick hash), probable (same name and size)", examples: &["dup:likely size:>1G"], values: &["yes", "no", "exact", "likely", "probable"] },
    FilterDoc { key: "copies", group: "Content", syntax: "copies:1  copies:>=3", help: "number of drives holding the same content", examples: &["in:photos-raw copies:1"], values: &[] },
    FilterDoc { key: "hidden", group: "Flags", syntax: "hidden:yes|no", help: "hidden files", examples: &["-hidden:yes"], values: YES_NO },
    FilterDoc { key: "trash", group: "Flags", syntax: "trash:yes|no", help: "inside a trash folder", examples: &["trash:yes"], values: YES_NO },
    FilterDoc { key: "compressed", group: "Flags", syntax: "compressed:yes|no", help: "compressed by the filesystem", examples: &["compressed:yes"], values: YES_NO },
    FilterDoc { key: "enc", group: "Flags", syntax: "enc:yes|no", help: "encrypted files (fscrypt, EFS)", examples: &["enc:yes"], values: YES_NO },
    FilterDoc { key: "sparse", group: "Flags", syntax: "sparse:yes|no", help: "sparse files (holes)", examples: &["sparse:yes"], values: YES_NO },
    FilterDoc { key: "dataless", group: "Flags", syntax: "dataless:yes|no", help: "cloud placeholders whose data is not on the drive", examples: &["dataless:yes"], values: YES_NO },
    FilterDoc { key: "offline", group: "Flags", syntax: "offline:yes|no", help: "offline attribute (NTFS/exFAT)", examples: &["offline:yes"], values: YES_NO },
    FilterDoc { key: "system", group: "Flags", syntax: "system:yes|no", help: "system attribute (NTFS/FAT)", examples: &["-system:yes"], values: YES_NO },
    FilterDoc { key: "readonly", group: "Flags", syntax: "readonly:yes|no", help: "read-only attribute", examples: &["readonly:yes"], values: YES_NO },
    FilterDoc { key: "archive", group: "Flags", syntax: "archive:yes|no", help: "archive attribute (NTFS/FAT)", examples: &["archive:no"], values: YES_NO },
    FilterDoc { key: "immutable", group: "Flags", syntax: "immutable:yes|no", help: "immutable flag", examples: &["immutable:yes"], values: YES_NO },
    FilterDoc { key: "append", group: "Flags", syntax: "append:yes|no", help: "append-only flag", examples: &["append:yes"], values: YES_NO },
    FilterDoc { key: "nodump", group: "Flags", syntax: "nodump:yes|no", help: "no-dump flag", examples: &["nodump:yes"], values: YES_NO },
    FilterDoc { key: "reparse", group: "Flags", syntax: "reparse:yes|no", help: "NTFS reparse points", examples: &["reparse:yes"], values: YES_NO },
    FilterDoc { key: "rsrc", group: "Flags", syntax: "rsrc:yes|no", help: "has a macOS resource fork", examples: &["rsrc:yes"], values: YES_NO },
    FilterDoc { key: "hardlink", group: "Flags", syntax: "hardlink:yes|no", help: "extra names of a hard-linked file (counted once)", examples: &["hardlink:yes"], values: YES_NO },
];

/// Tips shown with the reference.
pub const TIPS: &[&str] = &[
    "Terms are AND-ed; commas inside a value mean OR (ext:cr2,crw).",
    "Prefix a term with - to exclude it (-trash:yes, -path:Backup).",
    "Quote values with spaces (camera:\"EOS 5D*\"); * and ? are wildcards.",
];

/// Example queries for the cheat sheet.
pub const EXAMPLES: &[(&str, &str)] = &[
    ("ext:cr2,crw date:2014", "Canon RAW files modified in 2014"),
    ("in:photos-raw copies:1", "RAW photos that exist on only one drive"),
    ("type:video size:>1G -trash:yes", "big videos, not in a trash folder"),
    ("camera:canon iso:>=3200 taken:2019", "high-ISO Canon shots from 2019"),
    ("dup:likely size:>100M", "large files with a copy elsewhere"),
    ("res:4k codec:hevc", "4K HEVC videos"),
];

/// Doc for a key (flag keys not listed above get a generic entry).
pub fn doc(key: &str) -> Option<&'static FilterDoc> {
    FILTERS.iter().find(|f| f.key == key && !f.key.is_empty())
}

/// Keys starting with `prefix`, for completion.
pub fn complete_key(prefix: &str) -> Vec<&'static str> {
    let p = prefix.to_ascii_lowercase();
    FILTERS.iter().map(|f| f.key).filter(|k| !k.is_empty() && k.starts_with(&p)).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest known key to a misspelled one (`camra` → `camera`).
pub fn closest_key(key: &str) -> Option<&'static str> {
    let k = key.to_ascii_lowercase();
    FILTERS
        .iter()
        .map(|f| f.key)
        .filter(|c| !c.is_empty())
        .map(|c| (edit_distance(&k, c), c))
        .filter(|(d, c)| *d <= 2.max(c.len() / 4) || c.starts_with(&k))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        assert_eq!(closest_key("camra"), Some("camera"));
        assert_eq!(closest_key("sise"), Some("size"));
        assert_eq!(closest_key("zzzzzz"), None);
        assert_eq!(complete_key("c"), vec!["ctime", "camera", "codec", "copies", "compressed"]);
        assert!(doc("camera").is_some());
    }
}
