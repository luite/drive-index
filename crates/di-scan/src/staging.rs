//! Per-scan staging database.
//!
//! Same tables as the main catalog's `dirs`/`files`/`scan_errors`, but without
//! secondary indexes, so inserts are fast. `dirs.state` makes the staging DB a
//! checkpoint: directories still `pending` are exactly the remaining work.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use rusqlite::{Connection, params};

use di_model::{FileKind, flags};
use di_walk::{DirBatch, DirJob, DirState, Meta};

const SCHEMA: &str = "
CREATE TABLE info (key TEXT PRIMARY KEY, value TEXT) WITHOUT ROWID;
CREATE TABLE dirs (
  id INTEGER PRIMARY KEY, parent_id INTEGER, name TEXT NOT NULL, name_raw BLOB,
  path TEXT NOT NULL, os_path BLOB NOT NULL, depth INTEGER NOT NULL,
  mtime_ns INTEGER, btime_ns INTEGER, mode INTEGER, uid INTEGER, gid INTEGER, inode INTEGER,
  flags INTEGER NOT NULL DEFAULT 0, entry_count INTEGER, state INTEGER NOT NULL DEFAULT 0,
  tree_files INTEGER, tree_dirs INTEGER, tree_bytes INTEGER, tree_alloc_bytes INTEGER
);
CREATE TABLE files (
  id INTEGER PRIMARY KEY, dir_id INTEGER NOT NULL, name TEXT NOT NULL, name_raw BLOB, ext TEXT,
  kind INTEGER NOT NULL, size INTEGER NOT NULL, alloc_size INTEGER,
  mtime_ns INTEGER, ctime_ns INTEGER, atime_ns INTEGER, btime_ns INTEGER,
  mode INTEGER, uid INTEGER, gid INTEGER, inode INTEGER, nlink INTEGER,
  flags INTEGER NOT NULL DEFAULT 0, link_target TEXT
);
CREATE TABLE scan_errors (path TEXT NOT NULL, op TEXT NOT NULL, errno INTEGER, message TEXT);
";

pub struct Staging {
    pub conn: Connection,
    pub path: PathBuf,
}

/// Totals of a finished walk, written to `scans` and `filesystems`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Logical bytes, hardlinked content counted once.
    pub bytes: u64,
    pub alloc_bytes: u64,
    pub errors: u64,
}

fn configure(conn: &Connection) -> Result<()> {
    // synchronous=OFF: an app crash cannot lose WAL data (only an OS crash can, and
    // then the scan is simply restarted).
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = OFF;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -131072;",
    )?;
    Ok(())
}

impl Staging {
    /// Create a fresh staging DB with the root directory as the only pending job.
    pub fn create(path: &Path, scan_id: i64, root_meta: &Meta) -> Result<Staging> {
        remove_db_files(path);
        let conn = Connection::open(path).with_context(|| format!("creating staging {}", path.display()))?;
        configure(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute("INSERT INTO info (key, value) VALUES ('scan_id', ?1)", [scan_id.to_string()])?;
        conn.execute(
            "INSERT INTO dirs (id, parent_id, name, path, os_path, depth, mtime_ns, btime_ns, mode, uid, gid,
                               inode, flags, state)
             VALUES (1, NULL, '', '/', x'', 0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                root_meta.mtime_ns,
                root_meta.btime_ns,
                root_meta.mode,
                root_meta.uid,
                root_meta.gid,
                root_meta.inode.map(|v| v as i64),
                root_meta.flags
            ],
        )?;
        Ok(Staging { conn, path: path.to_path_buf() })
    }

    /// Reopen an interrupted scan's staging DB.
    pub fn open(path: &Path) -> Result<Staging> {
        let conn = Connection::open(path).with_context(|| format!("opening staging {}", path.display()))?;
        configure(&conn)?;
        let ok: bool = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('dirs','files','info')", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n == 3)
            .unwrap_or(false);
        anyhow::ensure!(ok, "staging database {} is incomplete", path.display());
        Ok(Staging { conn, path: path.to_path_buf() })
    }

    /// Directories not yet listed, as walker jobs.
    pub fn pending_jobs(&self) -> Result<Vec<DirJob>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, os_path, path, depth, flags FROM dirs WHERE state = 0 ORDER BY depth DESC, id")?;
        let jobs = stmt
            .query_map([], |r| {
                Ok(DirJob {
                    id: r.get::<_, i64>(0)? as u64,
                    os_path: r.get(1)?,
                    path: r.get(2)?,
                    depth: r.get::<_, i64>(3)? as u32,
                    in_trash: r.get::<_, i64>(4)? as u32 & flags::IN_TRASH != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(jobs)
    }

    pub fn next_dir_id(&self) -> Result<u64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM dirs", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Inodes of multiply-linked files already recorded (for hardlink dedupe on resume).
    pub fn hardlink_inodes(&self) -> Result<HashSet<u64>> {
        let mut stmt = self.conn.prepare("SELECT inode FROM files WHERE nlink > 1 AND inode IS NOT NULL")?;
        let set = stmt
            .query_map([], |r| r.get::<_, i64>(0).map(|v| v as u64))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(set)
    }

    pub fn listed_dirs(&self) -> Result<u64> {
        Ok(self.conn.query_row("SELECT count(*) FROM dirs WHERE state != 0", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Compute ncdu-style recursive totals for every directory and return scan totals.
    pub fn compute_aggregates(&mut self) -> Result<Totals> {
        struct Node {
            parent: Option<u64>,
            depth: u32,
            files: u64,
            dirs: u64,
            bytes: u64,
            alloc: u64,
        }
        let mut nodes: HashMap<u64, Node> = HashMap::new();
        {
            let mut stmt = self.conn.prepare("SELECT id, parent_id, depth FROM dirs")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                nodes.insert(
                    r.get::<_, i64>(0)? as u64,
                    Node {
                        parent: r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                        depth: r.get::<_, i64>(2)? as u32,
                        files: 0,
                        dirs: 0,
                        bytes: 0,
                        alloc: 0,
                    },
                );
            }
        }
        let mut totals = Totals { dirs: nodes.len() as u64, ..Totals::default() };
        {
            let sql = format!(
                "SELECT dir_id, count(*),
                        COALESCE(sum(CASE WHEN flags & {dup} = 0 THEN size END), 0),
                        COALESCE(sum(CASE WHEN flags & {dup} = 0 THEN alloc_size END), 0),
                        sum(kind = {link})
                 FROM files GROUP BY dir_id",
                dup = flags::HARDLINK_DUP,
                link = FileKind::Symlink.code()
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let id = r.get::<_, i64>(0)? as u64;
                let (files, bytes, alloc, links) = (
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                    r.get::<_, i64>(4)? as u64,
                );
                totals.files += files;
                totals.bytes += bytes;
                totals.alloc_bytes += alloc;
                totals.symlinks += links;
                if let Some(n) = nodes.get_mut(&id) {
                    n.files += files;
                    n.bytes += bytes;
                    n.alloc += alloc;
                }
            }
        }
        totals.errors =
            self.conn.query_row("SELECT count(*) FROM scan_errors", [], |r| r.get::<_, i64>(0))? as u64;

        // Propagate bottom-up: deepest directories first.
        let mut order: Vec<(u32, u64)> = nodes.iter().map(|(id, n)| (n.depth, *id)).collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        for (_, id) in &order {
            let (parent, files, dirs, bytes, alloc) = {
                let n = &nodes[id];
                (n.parent, n.files, n.dirs, n.bytes, n.alloc)
            };
            if let Some(p) = parent.and_then(|p| nodes.get_mut(&p)) {
                p.files += files;
                p.dirs += dirs + 1;
                p.bytes += bytes;
                p.alloc += alloc;
            }
        }

        let tx = self.conn.transaction()?;
        {
            let mut up = tx.prepare(
                "UPDATE dirs SET tree_files = ?2, tree_dirs = ?3, tree_bytes = ?4, tree_alloc_bytes = ?5 WHERE id = ?1",
            )?;
            for (id, n) in &nodes {
                up.execute(params![*id as i64, n.files as i64, n.dirs as i64, n.bytes as i64, n.alloc as i64])?;
            }
        }
        tx.commit()?;
        Ok(totals)
    }
}

pub fn remove_db_files(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// Writer thread body: persist batches until the channel closes. Commits every
/// 250 ms or 50k rows, whichever comes first.
pub fn run_writer(staging: Staging, rx: Receiver<DirBatch>, mut hardlinks: HashSet<u64>) -> Result<Staging> {
    const COMMIT_ROWS: usize = 50_000;
    const COMMIT_EVERY: Duration = Duration::from_millis(250);

    let conn = &staging.conn;
    conn.execute_batch("BEGIN")?;
    let mut rows = 0usize;
    let mut last_commit = Instant::now();
    loop {
        match rx.recv_timeout(COMMIT_EVERY) {
            Ok(batch) => rows += apply(conn, batch, &mut hardlinks)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if rows > 0 && (rows >= COMMIT_ROWS || last_commit.elapsed() >= COMMIT_EVERY) {
            conn.execute_batch("COMMIT; BEGIN")?;
            rows = 0;
            last_commit = Instant::now();
        }
    }
    conn.execute_batch("COMMIT")?;
    Ok(staging)
}

fn opt_i64(v: Option<u64>) -> Option<i64> {
    v.map(|x| x as i64)
}

fn apply(conn: &Connection, batch: DirBatch, hardlinks: &mut HashSet<u64>) -> Result<usize> {
    let mut n = 1;
    conn.prepare_cached("UPDATE dirs SET state = ?2, entry_count = ?3, flags = flags | ?4 WHERE id = ?1")?
        .execute(params![batch.dir_id as i64, batch.state as u8, batch.entry_count, batch.dir_flags])?;

    if !batch.subdirs.is_empty() {
        let mut ins = conn.prepare_cached(
            "INSERT INTO dirs (id, parent_id, name, name_raw, path, os_path, depth, mtime_ns, btime_ns, mode,
                               uid, gid, inode, flags, entry_count, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        for d in &batch.subdirs {
            let m = &d.meta;
            ins.execute(params![
                d.id as i64,
                d.parent_id as i64,
                d.name,
                d.name_raw,
                d.path,
                d.os_path,
                d.depth,
                m.mtime_ns,
                m.btime_ns,
                m.mode,
                m.uid,
                m.gid,
                opt_i64(m.inode),
                m.flags,
                m.entry_count,
                if d.state == DirState::Pending { 0 } else { 1 },
            ])?;
            n += 1;
        }
    }

    if !batch.files.is_empty() {
        let mut ins = conn.prepare_cached(
            "INSERT INTO files (dir_id, name, name_raw, ext, kind, size, alloc_size, mtime_ns, ctime_ns, atime_ns,
                                btime_ns, mode, uid, gid, inode, nlink, flags, link_target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )?;
        for f in &batch.files {
            let m = &f.meta;
            let mut fl = m.flags;
            if let (Some(nlink), Some(ino)) = (m.nlink, m.inode) {
                if nlink > 1 && f.kind == FileKind::File && !hardlinks.insert(ino) {
                    fl |= flags::HARDLINK_DUP;
                }
            }
            ins.execute(params![
                batch.dir_id as i64,
                f.name,
                f.name_raw,
                f.ext,
                f.kind.code(),
                m.size as i64,
                opt_i64(m.alloc),
                m.mtime_ns,
                m.ctime_ns,
                m.atime_ns,
                m.btime_ns,
                m.mode,
                m.uid,
                m.gid,
                opt_i64(m.inode),
                opt_i64(m.nlink),
                fl,
                f.link_target,
            ])?;
            n += 1;
        }
    }

    if !batch.errors.is_empty() {
        let mut ins =
            conn.prepare_cached("INSERT INTO scan_errors (path, op, errno, message) VALUES (?1, ?2, ?3, ?4)")?;
        for e in &batch.errors {
            ins.execute(params![e.path, e.op, e.errno, e.message])?;
        }
    }
    Ok(n)
}
