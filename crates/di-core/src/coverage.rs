//! Collection coverage (PLAN.md §5.5, §7.6): which drives hold how much of a
//! collection, and what exists in only one place.
//!
//! An *item* is one piece of content, identified by its quick hash (which covers
//! the size), else by lowercase name + size. Full hashes are not used for identity:
//! they exist only where duplicates were verified, and mixing the two would count
//! one item twice. (`hex(NULL)` is '', not NULL, hence the explicit CASE.)

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use serde::Serialize;

use di_model::flags;

use crate::query::{self, Compiled};
use crate::search::FROM;

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub slug: String,
    pub name: String,
    pub query: String,
}

pub fn collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare("SELECT slug, name, query FROM collections ORDER BY id")?;
    Ok(stmt
        .query_map([], |r| Ok(Collection { slug: r.get(0)?, name: r.get(1)?, query: r.get(2)? }))?
        .collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveCoverage {
    pub drive_id: i64,
    pub label: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// Distinct items on this drive.
    pub items: u64,
    /// Share of all items in the collection.
    pub pct: f64,
    /// Items that exist on no other drive.
    pub only_items: u64,
    pub only_bytes: u64,
    pub last_seen_at: i64,
    /// Latest SMART verdict, if any.
    pub smart_passed: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub collection: Collection,
    pub items: u64,
    pub bytes: u64,
    /// Items identified by a content hash (the rest by name + size).
    pub hashed_items: u64,
    pub drives: Vec<DriveCoverage>,
    /// Items with exactly 1, exactly 2, and 3+ copies (distinct drives).
    pub redundancy: [u64; 3],
    pub only_bytes: u64,
    /// (year, items): capture year when known, else modification year.
    pub years: Vec<(i32, u64)>,
}

pub fn coverage(conn: &Connection, slug_or_name: &str) -> Result<Coverage> {
    let collection = conn
        .query_row(
            "SELECT slug, name, query FROM collections WHERE slug = ?1 OR lower(name) = lower(?1)",
            [slug_or_name],
            |r| Ok(Collection { slug: r.get(0)?, name: r.get(1)?, query: r.get(2)? }),
        )
        .optional()?;
    let Some(collection) = collection else { bail!("unknown collection '{slug_or_name}'") };
    let Compiled { sql: cond, params } = query::compile(conn, &collection.query)?;

    // Materialize the collection's files once; everything else aggregates over it.
    conn.execute_batch("DROP TABLE IF EXISTS temp.cov; DROP TABLE IF EXISTS temp.cov_items;")?;
    conn.execute(
        &format!(
            "CREATE TEMP TABLE cov AS
             SELECT fs.drive_id AS drive_id, f.size AS size,
                    CASE WHEN h.quick IS NOT NULL THEN 'Q' || hex(h.quick)
                         ELSE 'N' || lower(f.name) || ':' || f.size END AS ident,
                    (h.quick IS NOT NULL) AS hashed,
                    CAST(COALESCE(substr(m.captured_at, 1, 4), strftime('%Y', f.mtime_ns / 1000000000, 'unixepoch')) AS INTEGER) AS year
             {FROM}
             WHERE f.kind = 0 AND (f.flags & {dup}) = 0 AND ({cond})",
            dup = flags::HARDLINK_DUP
        ),
        params_from_iter(params.iter()),
    )?;
    conn.execute_batch(
        "CREATE TEMP TABLE cov_items AS
         SELECT ident, max(size) AS size, count(DISTINCT drive_id) AS nd, max(hashed) AS hashed, min(year) AS year
         FROM cov GROUP BY ident;
         CREATE INDEX temp.cov_items_ident ON cov_items(ident);",
    )?;

    let (items, bytes, hashed_items, only_bytes): (i64, i64, i64, i64) = conn.query_row(
        "SELECT count(*), COALESCE(sum(size), 0), COALESCE(sum(hashed), 0),
                COALESCE(sum(CASE WHEN nd = 1 THEN size END), 0) FROM cov_items",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let redundancy: [u64; 3] = conn.query_row(
        "SELECT COALESCE(sum(nd = 1), 0), COALESCE(sum(nd = 2), 0), COALESCE(sum(nd >= 3), 0) FROM cov_items",
        [],
        |r| Ok([r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64]),
    )?;
    let mut stmt = conn.prepare(
        "WITH per AS (SELECT drive_id, ident, count(*) AS n, sum(size) AS bytes FROM cov GROUP BY drive_id, ident)
         SELECT per.drive_id, d.label, sum(per.n), sum(per.bytes), count(*),
                sum(i.nd = 1), COALESCE(sum(CASE WHEN i.nd = 1 THEN i.size END), 0), d.last_seen_at,
                (SELECT passed FROM smart_snapshots s WHERE s.drive_id = per.drive_id ORDER BY taken_at DESC LIMIT 1)
         FROM per JOIN cov_items i ON i.ident = per.ident JOIN drives d ON d.id = per.drive_id
         GROUP BY per.drive_id ORDER BY count(*) DESC",
    )?;
    let total = items.max(1) as f64;
    let drives = stmt
        .query_map([], |r| {
            let n_items: i64 = r.get(4)?;
            Ok(DriveCoverage {
                drive_id: r.get(0)?,
                label: r.get(1)?,
                files: r.get::<_, i64>(2)? as u64,
                bytes: r.get::<_, i64>(3)? as u64,
                items: n_items as u64,
                pct: 100.0 * n_items as f64 / total,
                only_items: r.get::<_, i64>(5)? as u64,
                only_bytes: r.get::<_, i64>(6)? as u64,
                last_seen_at: r.get(7)?,
                smart_passed: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt = conn.prepare("SELECT year, count(*) FROM cov_items WHERE year IS NOT NULL GROUP BY year ORDER BY year")?;
    let years = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)? as i32, r.get::<_, i64>(1)? as u64)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    conn.execute_batch("DROP TABLE IF EXISTS temp.cov; DROP TABLE IF EXISTS temp.cov_items;")?;
    Ok(Coverage {
        collection,
        items: items as u64,
        bytes: bytes as u64,
        hashed_items: hashed_items as u64,
        drives,
        redundancy,
        only_bytes: only_bytes as u64,
        years,
    })
}
