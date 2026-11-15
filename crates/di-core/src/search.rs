//! Running compiled queries against the catalog.

use rusqlite::{Connection, params_from_iter};
use serde::Serialize;

use di_model::FileKind;

use crate::query::{self, Compiled, QueryError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Drive, then directory path, then name.
    Path,
    Name,
    Size,
    Mtime,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s {
            "path" => SortKey::Path,
            "name" => SortKey::Name,
            "size" => SortKey::Size,
            "mtime" | "date" => SortKey::Mtime,
            _ => return None,
        })
    }

    fn order_by(self, desc: bool) -> String {
        let dir = if desc { "DESC" } else { "ASC" };
        match self {
            SortKey::Path => format!("d.id {dir}, di.path {dir}, f.name {dir}"),
            SortKey::Name => format!("f.name {dir}, f.id"),
            SortKey::Size => format!("f.size {dir}, f.id"),
            SortKey::Mtime => format!("f.mtime_ns {dir}, f.id"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub sort: SortKey,
    pub descending: bool,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub file_id: i64,
    pub drive_id: i64,
    pub drive_label: Option<String>,
    pub filesystem_id: i64,
    pub fs_label: Option<String>,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub alloc_size: Option<u64>,
    pub mtime_ns: Option<i64>,
    #[serde(serialize_with = "ser_kind")]
    pub kind: FileKind,
    pub flags: u32,
    pub link_target: Option<String>,
}

fn ser_kind<S: serde::Serializer>(k: &FileKind, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(k.as_str())
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

const FROM: &str = "FROM files f
    JOIN dirs di        ON di.id = f.dir_id
    JOIN filesystems fs ON fs.id = f.filesystem_id
    JOIN drives d       ON d.id = fs.drive_id";

pub fn search(conn: &Connection, req: &SearchRequest) -> Result<Vec<Hit>, SearchError> {
    let Compiled { sql: cond, mut params } = query::compile(conn, req.query)?;
    let sql = format!(
        "SELECT f.id, d.id, d.label, fs.id, COALESCE(fs.user_label, fs.volume_label),
                CASE di.path WHEN '/' THEN '/' || f.name ELSE di.path || '/' || f.name END,
                f.name, f.size, f.alloc_size, f.mtime_ns, f.kind, f.flags, f.link_target
         {FROM}
         WHERE {cond}
         ORDER BY {}
         LIMIT ?{} OFFSET ?{}",
        req.sort.order_by(req.descending),
        params.len() + 1,
        params.len() + 2
    );
    params.push((req.limit as i64).into());
    params.push((req.offset as i64).into());
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok(Hit {
            file_id: r.get(0)?,
            drive_id: r.get(1)?,
            drive_label: r.get(2)?,
            filesystem_id: r.get(3)?,
            fs_label: r.get(4)?,
            path: r.get(5)?,
            name: r.get(6)?,
            size: r.get::<_, i64>(7)? as u64,
            alloc_size: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            mtime_ns: r.get(9)?,
            kind: FileKind::from_code(r.get(10)?),
            flags: r.get::<_, i64>(11)? as u32,
            link_target: r.get(12)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DriveTotal {
    pub drive_id: i64,
    pub label: Option<String>,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub files: u64,
    /// Sum of sizes, counting hardlinked content once.
    pub bytes: u64,
    pub per_drive: Vec<DriveTotal>,
}

pub fn summarize(conn: &Connection, query_str: &str) -> Result<Summary, SearchError> {
    let Compiled { sql: cond, params } = query::compile(conn, query_str)?;
    let sql = format!(
        "SELECT d.id, d.label, count(*),
                COALESCE(sum(CASE WHEN (f.flags & {}) = 0 THEN f.size ELSE 0 END), 0)
         {FROM}
         WHERE {cond}
         GROUP BY d.id ORDER BY count(*) DESC",
        di_model::flags::HARDLINK_DUP
    );
    let mut stmt = conn.prepare(&sql)?;
    let per_drive: Vec<DriveTotal> = stmt
        .query_map(params_from_iter(params.iter()), |r| {
            Ok(DriveTotal {
                drive_id: r.get(0)?,
                label: r.get(1)?,
                files: r.get::<_, i64>(2)? as u64,
                bytes: r.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Summary {
        files: per_drive.iter().map(|d| d.files).sum(),
        bytes: per_drive.iter().map(|d| d.bytes).sum(),
        per_drive,
    })
}
