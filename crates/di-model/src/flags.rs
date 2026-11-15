//! Bits of `files.flags` / `dirs.flags` (PLAN.md §5.3). The numbering is part of the
//! on-disk format: never renumber, only append.

pub const HIDDEN: u32 = 1 << 0;
pub const SYSTEM: u32 = 1 << 1;
pub const READONLY_ATTR: u32 = 1 << 2;
pub const ARCHIVE: u32 = 1 << 3;
pub const COMPRESSED: u32 = 1 << 4;
pub const ENCRYPTED: u32 = 1 << 5;
pub const SPARSE: u32 = 1 << 6;
pub const IMMUTABLE: u32 = 1 << 7;
pub const APPEND_ONLY: u32 = 1 << 8;
pub const NODUMP: u32 = 1 << 9;
pub const DATALESS: u32 = 1 << 10;
pub const REPARSE_POINT: u32 = 1 << 11;
pub const OFFLINE: u32 = 1 << 12;
pub const HAS_RSRC_FORK: u32 = 1 << 13;
pub const HARDLINK_DUP: u32 = 1 << 14;
pub const IN_TRASH: u32 = 1 << 15;
/// Directory recorded but its contents were deliberately not listed
/// (skip list, mount point, other device).
pub const SKIPPED: u32 = 1 << 16;
/// Directory could not be listed (permission or I/O error).
pub const LIST_ERROR: u32 = 1 << 17;

/// Names usable in the search language (`hidden:yes`, `trash:no`, ...).
pub const NAMED: &[(&str, u32)] = &[
    ("hidden", HIDDEN),
    ("system", SYSTEM),
    ("readonly", READONLY_ATTR),
    ("archive", ARCHIVE),
    ("compressed", COMPRESSED),
    ("enc", ENCRYPTED),
    ("encrypted", ENCRYPTED),
    ("sparse", SPARSE),
    ("immutable", IMMUTABLE),
    ("append", APPEND_ONLY),
    ("nodump", NODUMP),
    ("dataless", DATALESS),
    ("reparse", REPARSE_POINT),
    ("offline", OFFLINE),
    ("rsrc", HAS_RSRC_FORK),
    ("hardlink", HARDLINK_DUP),
    ("trash", IN_TRASH),
];

/// Human-readable list of set flags, for detail views.
pub fn describe(flags: u32) -> Vec<&'static str> {
    const ALL: &[(u32, &str)] = &[
        (HIDDEN, "hidden"),
        (SYSTEM, "system"),
        (READONLY_ATTR, "readonly"),
        (ARCHIVE, "archive"),
        (COMPRESSED, "compressed"),
        (ENCRYPTED, "encrypted"),
        (SPARSE, "sparse"),
        (IMMUTABLE, "immutable"),
        (APPEND_ONLY, "append-only"),
        (NODUMP, "nodump"),
        (DATALESS, "dataless"),
        (REPARSE_POINT, "reparse-point"),
        (OFFLINE, "offline"),
        (HAS_RSRC_FORK, "resource-fork"),
        (HARDLINK_DUP, "hardlink-dup"),
        (IN_TRASH, "in-trash"),
        (SKIPPED, "skipped"),
        (LIST_ERROR, "list-error"),
    ];
    ALL.iter().filter(|(bit, _)| flags & bit != 0).map(|(_, n)| *n).collect()
}
