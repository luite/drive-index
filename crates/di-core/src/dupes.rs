//! Duplicate groups (PLAN.md §7.5) at three confidence levels:
//! exact (full BLAKE3), likely (quick hash + size), probable (same name and size).

use anyhow::Result;
use rusqlite::{Connection, params_from_iter, types::Value};
use serde::Serialize;

use di_model::flags;

use crate::query::{self, Compiled};
use crate::search::FROM;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Confidence {
    Exact,
    Likely,
    Probable,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::Likely => "likely",
            Confidence::Probable => "probable",
        }
    }
    pub fn next(self) -> Confidence {
        match self {
            Confidence::Exact => Confidence::Likely,
            Confidence::Likely => Confidence::Probable,
            Confidence::Probable => Confidence::Exact,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DupeQuery {
    pub confidence: Confidence,
    pub min_size: u64,
    /// Restrict to files matching a search query (e.g. `in:photos-raw`).
    pub filter: Option<String>,
    /// Only groups whose copies are all on one drive (wasted space on that drive).
    pub same_drive_only: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupeGroup {
    /// Grouping key: hex hash, or the lowercased name for probable groups.
    pub key: String,
    pub size: u64,
    pub copies: u64,
    pub drives: Vec<i64>,
    pub wasted: u64,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DupeSummary {
    pub groups: u64,
    pub files: u64,
    pub wasted: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupeMember {
    pub file_id: i64,
    pub drive_id: i64,
    pub drive_label: Option<String>,
    pub path: String,
    pub mtime_ns: Option<i64>,
}

fn grouping(c: Confidence) -> (&'static str, &'static str) {
    // (group key expression, extra WHERE)
    match c {
        Confidence::Exact => ("hex(h.full)", "h.full IS NOT NULL"),
        Confidence::Likely => ("hex(h.quick)", "h.quick IS NOT NULL"),
        Confidence::Probable => ("lower(f.name)", "1"),
    }
}

fn base(q: &DupeQuery, conn: &Connection) -> Result<(String, Vec<Value>)> {
    let (key, extra) = grouping(q.confidence);
    let mut params: Vec<Value> = Vec::new();
    let filter = match q.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        Some(f) => {
            let Compiled { sql, params: p } = query::compile(conn, f)?;
            params = p;
            format!("AND ({sql})")
        }
        None => String::new(),
    };
    let n = params.len();
    params.push((q.min_size as i64).into());
    let having = if q.same_drive_only { "AND count(DISTINCT fs.drive_id) = 1" } else { "" };
    Ok((
        format!(
            "SELECT {key} AS k, f.size AS size, count(*) AS copies, group_concat(DISTINCT fs.drive_id) AS drives,
                    min(f.name) AS name, (count(*) - 1) * f.size AS wasted
             {FROM}
             WHERE f.kind = 0 AND (f.flags & {dup}) = 0 AND f.size >= ?{p} AND {extra} {filter}
             GROUP BY k, f.size HAVING count(*) > 1 {having}",
            dup = flags::HARDLINK_DUP,
            p = n + 1
        ),
        params,
    ))
}

pub fn groups(conn: &Connection, q: &DupeQuery) -> Result<(Vec<DupeGroup>, DupeSummary)> {
    let (sql, mut params) = base(q, conn)?;
    let summary = conn.query_row(
        &format!("SELECT count(*), COALESCE(sum(copies), 0), COALESCE(sum(wasted), 0) FROM ({sql})"),
        params_from_iter(params.iter()),
        |r| {
            Ok(DupeSummary {
                groups: r.get::<_, i64>(0)? as u64,
                files: r.get::<_, i64>(1)? as u64,
                wasted: r.get::<_, i64>(2)? as u64,
            })
        },
    )?;
    let n = params.len();
    params.push((q.limit as i64).into());
    let mut stmt = conn.prepare(&format!("{sql} ORDER BY wasted DESC, size DESC LIMIT ?{}", n + 1))?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |r| {
            let drives: String = r.get(3)?;
            Ok(DupeGroup {
                key: r.get(0)?,
                size: r.get::<_, i64>(1)? as u64,
                copies: r.get::<_, i64>(2)? as u64,
                drives: drives.split(',').filter_map(|d| d.parse().ok()).collect(),
                name: r.get(4)?,
                wasted: r.get::<_, i64>(5)? as u64,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((rows, summary))
}

/// The files of one group.
pub fn members(conn: &Connection, confidence: Confidence, key: &str, size: u64) -> Result<Vec<DupeMember>> {
    let (key_expr, extra) = grouping(confidence);
    let mut stmt = conn.prepare(&format!(
        "SELECT f.id, d.id, d.label, CASE di.path WHEN '/' THEN '/' || f.name ELSE di.path || '/' || f.name END, f.mtime_ns
         {FROM}
         WHERE f.size = ?1 AND {extra} AND {key_expr} = ?2 AND f.kind = 0 AND (f.flags & {dup}) = 0
         ORDER BY d.id, di.path, f.name",
        dup = flags::HARDLINK_DUP
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![size as i64, key], |r| {
            Ok(DupeMember { file_id: r.get(0)?, drive_id: r.get(1)?, drive_label: r.get(2)?, path: r.get(3)?, mtime_ns: r.get(4)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
