//! Seed data: extension categories and default collections (user-editable afterwards).

pub const EXT_CATEGORIES: &[(&str, &[&str])] = &[
    (
        "photo_raw",
        &[
            "crw", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "orf", "raf", "rw2", "pef",
            "srw", "x3f", "3fr", "iiq", "erf", "mos", "kdc", "dcr", "rwl", "mrw",
        ],
    ),
    ("photo", &["jpg", "jpeg", "heic", "heif", "png", "tif", "tiff", "gif", "bmp", "webp", "avif", "jxl"]),
    (
        "video",
        &[
            "mov", "mp4", "m4v", "avi", "mts", "m2ts", "mkv", "mxf", "wmv", "mpg", "mpeg", "3gp", "webm",
            "flv", "vob",
        ],
    ),
    ("audio", &["mp3", "flac", "m4a", "aac", "wav", "aif", "aiff", "ogg", "opus", "wma", "ape", "alac"]),
    (
        "document",
        &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "md",
            "pages", "numbers", "key", "epub",
        ],
    ),
    ("archive", &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "cab"]),
    (
        "disk_image",
        &["dmg", "iso", "img", "vmdk", "vdi", "vhd", "vhdx", "qcow2", "sparseimage", "sparsebundle"],
    ),
    ("code", &["rs", "py", "js", "ts", "c", "h", "cpp", "go", "java", "rb", "sh"]),
];

/// (slug, name, query)
pub const COLLECTIONS: &[(&str, &str, &str)] = &[
    ("photos-raw", "Photos (RAW)", "type:photo_raw"),
    ("photos", "Photos (JPEG/HEIC/…)", "type:photo"),
    ("video", "Video", "type:video"),
    ("audio", "Audio", "type:audio"),
    ("documents", "Documents", "type:document"),
    ("archives", "Archives", "type:archive"),
    ("disk-images", "Disk images", "type:disk_image"),
];
