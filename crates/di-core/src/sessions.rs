//! Moving the catalog between hosts by copying the file (PLAN.md §5.6).
//!
//! * Every read-write use of the catalog is a *session* (row in `sessions`).
//! * Each host remembers, outside the catalog, the latest session it has seen per
//!   catalog. A catalog that lacks that session is an older copy: writing to it
//!   would silently fork history, so callers refuse unless the user insists.
//! * A session of *another* host that never ended means the file was copied while
//!   that host had it open (or it crashed): recent changes may be missing.
//! * `handoff_copy` writes a consistent, compacted copy with `VACUUM INTO`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, SCHEMA_VERSION, now_secs, random_id};

/// Per-host memory of the latest session seen in each catalog, kept outside the
/// catalog's directory so it is not copied along with it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostState {
    #[serde(default)]
    pub catalogs: HashMap<String, HostEntry>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostEntry {
    pub last_session: String,
    pub started_at: i64,
    pub host: String,
}

impl HostState {
    pub fn load(path: &Path) -> Result<HostState> {
        let mut st: HostState = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HostState::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        st.path = path.to_path_buf();
        Ok(st)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn remember(&mut self, catalog_id: &str, session: &SessionInfo) {
        self.catalogs.insert(
            catalog_id.to_string(),
            HostEntry { last_session: session.uuid.clone(), started_at: session.started_at, host: session.host.clone() },
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Lineage {
    /// This host has not used this catalog before.
    New,
    /// The catalog contains the latest session this host knows about.
    Current,
    /// The catalog lacks the latest session this host has seen: it is an older copy.
    Stale { last_session: String, started_at: i64, host: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenCheck {
    pub catalog_id: String,
    pub lineage: Lineage,
    /// Hosts with sessions that never ended (file copied while open there, or a crash).
    pub open_elsewhere: Vec<SessionInfo>,
    /// `PRAGMA quick_check` result when `open_elsewhere` is non-empty.
    pub integrity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub uuid: String,
    pub host: String,
    pub host_os: Option<String>,
    pub app_version: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub scans: i64,
    pub pid: Option<i64>,
}

fn session_from_row(r: &rusqlite::Row) -> rusqlite::Result<SessionInfo> {
    Ok(SessionInfo {
        uuid: r.get(0)?,
        host: r.get(1)?,
        host_os: r.get(2)?,
        app_version: r.get(3)?,
        started_at: r.get(4)?,
        ended_at: r.get(5)?,
        scans: r.get(6)?,
        pid: r.get(7)?,
    })
}

const SESSION_COLS: &str = "uuid, host, host_os, app_version, started_at, ended_at, scans, pid";

fn sessions(conn: &Connection, limit: usize) -> Result<Vec<SessionInfo>> {
    let mut stmt = conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY started_at DESC, id DESC LIMIT ?1"))?;
    Ok(stmt.query_map([limit as i64], session_from_row)?.collect::<rusqlite::Result<_>>()?)
}

fn session_uuids(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT uuid FROM sessions")?;
    Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && (unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogStatus {
    pub path: String,
    pub size_bytes: u64,
    pub schema_version: i64,
    pub catalog_id: String,
    pub created_at: Option<i64>,
    pub lineage: Lineage,
    pub open_sessions: Vec<SessionInfo>,
    pub recent_sessions: Vec<SessionInfo>,
}

impl Catalog {
    /// Lineage and open-session checks to run before writing.
    pub fn check_open(&self, host: &str, state: &HostState) -> Result<OpenCheck> {
        let catalog_id = self.catalog_id()?;
        let lineage = self.lineage(&catalog_id, state)?;
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE ended_at IS NULL AND host != ?1 ORDER BY started_at"
        ))?;
        let open_elsewhere: Vec<SessionInfo> =
            stmt.query_map([host], session_from_row)?.collect::<rusqlite::Result<_>>()?;
        let integrity = if open_elsewhere.is_empty() {
            None
        } else {
            Some(self.conn().query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?)
        };
        Ok(OpenCheck { catalog_id, lineage, open_elsewhere, integrity })
    }

    fn lineage(&self, catalog_id: &str, state: &HostState) -> Result<Lineage> {
        Ok(match state.catalogs.get(catalog_id) {
            None => Lineage::New,
            Some(e) => {
                let present: bool = self
                    .conn()
                    .query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE uuid = ?1)", [&e.last_session], |r| r.get(0))?;
                if present {
                    Lineage::Current
                } else {
                    Lineage::Stale { last_session: e.last_session.clone(), started_at: e.started_at, host: e.host.clone() }
                }
            }
        })
    }

    /// Start a session: record it, close this host's dead sessions, remember it locally.
    pub fn begin_session(&self, host: &str, state: &mut HostState) -> Result<String> {
        let now = now_secs();
        // Sessions of this host whose process is gone ended without saying so (crash).
        let mut stmt = self.conn().prepare("SELECT uuid, pid FROM sessions WHERE ended_at IS NULL AND host = ?1")?;
        let dangling: Vec<(String, Option<i64>)> =
            stmt.query_map([host], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (uuid, pid) in dangling {
            if !pid.is_some_and(pid_alive) {
                self.conn().execute("UPDATE sessions SET ended_at = ?2 WHERE uuid = ?1", params![uuid, now])?;
            }
        }
        let session = SessionInfo {
            uuid: random_id(),
            host: host.to_string(),
            host_os: Some(std::env::consts::OS.to_string()),
            app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            started_at: now,
            ended_at: None,
            scans: 0,
            pid: Some(std::process::id() as i64),
        };
        self.conn().execute(
            "INSERT INTO sessions (uuid, host, host_os, app_version, started_at, pid, first_scan_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT COALESCE(MAX(id), 0) FROM scans))",
            params![session.uuid, session.host, session.host_os, session.app_version, session.started_at, session.pid],
        )?;
        state.remember(&self.catalog_id()?, &session);
        state.save()?;
        Ok(session.uuid)
    }

    /// End a session and checkpoint the WAL so a closed catalog is a single,
    /// self-contained file.
    pub fn end_session(&self, uuid: &str) -> Result<()> {
        let now = now_secs();
        self.conn().execute(
            "UPDATE sessions SET ended_at = ?2,
                    scans = (SELECT count(*) FROM scans
                             WHERE scans.id > COALESCE(sessions.first_scan_id, 0) AND scans.host = sessions.host)
             WHERE uuid = ?1",
            params![uuid, now],
        )?;
        // Best effort: other connections (a running viewer) can keep part of the WAL.
        let _ = self.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(())
    }

    /// Write a consistent, compacted copy for moving to another host.
    pub fn handoff_copy(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        let d = dest.to_str().context("destination path is not UTF-8")?;
        self.conn().execute("VACUUM INTO ?1", [d]).context("writing the copy")?;
        Ok(())
    }

    pub fn status(&self, state: &HostState) -> Result<CatalogStatus> {
        let size = std::fs::metadata(self.path()).map(|m| m.len()).unwrap_or(0);
        let catalog_id = self.catalog_id()?;
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE ended_at IS NULL ORDER BY started_at"
        ))?;
        let open_sessions: Vec<SessionInfo> = stmt
            .query_map([], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|s| s.pid.is_none_or(pid_alive) || s.host != crate_host_hint())
            .collect();
        Ok(CatalogStatus {
            path: self.path().display().to_string(),
            size_bytes: size,
            schema_version: self.conn().query_row("PRAGMA user_version", [], |r| r.get(0))?,
            created_at: self
                .conn()
                .query_row("SELECT value FROM meta WHERE key = 'created_at'", [], |r| r.get::<_, String>(0))
                .optional()?
                .and_then(|v| v.parse().ok()),
            lineage: self.lineage(&catalog_id, state)?,
            catalog_id,
            open_sessions,
            recent_sessions: sessions(self.conn(), 10)?,
        })
    }
}

/// Host name as recorded in sessions (set by the binary through `set_host_hint`).
fn crate_host_hint() -> String {
    HOST_HINT.get().cloned().unwrap_or_default()
}
static HOST_HINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Tell the catalog which host name this process records (used to tell this host's
/// crashed sessions from other hosts' open ones in `status`).
pub fn set_host_hint(host: &str) {
    let _ = HOST_HINT.set(host.to_string());
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportCheck {
    pub incoming_id: String,
    pub local_id: Option<String>,
    /// Sessions in the local catalog that the incoming copy lacks (work that would be lost).
    pub local_only: Vec<SessionInfo>,
    pub incoming_sessions: usize,
}

/// Compare an incoming catalog copy with the local catalog before replacing it.
pub fn check_import(incoming: &Path, local: &Path) -> Result<ImportCheck> {
    let inc = Connection::open_with_flags(incoming, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", incoming.display()))?;
    let version: i64 = inc.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        bail!("{} is not a drive-index catalog", incoming.display());
    }
    if version > SCHEMA_VERSION {
        bail!("{} has schema version {version}; this build understands up to {SCHEMA_VERSION}", incoming.display());
    }
    let incoming_id: String = inc.query_row("SELECT value FROM meta WHERE key = 'catalog_id'", [], |r| r.get(0))?;
    let has_sessions_table: bool =
        inc.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'sessions')", [], |r| r.get(0))?;
    let incoming_uuids = if has_sessions_table { session_uuids(&inc)? } else { HashSet::new() };

    if !local.exists() {
        return Ok(ImportCheck { incoming_id, local_id: None, local_only: Vec::new(), incoming_sessions: incoming_uuids.len() });
    }
    let loc = Catalog::open(local)?;
    let local_id = loc.catalog_id()?;
    let local_only: Vec<SessionInfo> = sessions(loc.conn(), usize::MAX >> 1)?
        .into_iter()
        .filter(|s| !incoming_uuids.contains(&s.uuid))
        .collect();
    Ok(ImportCheck { incoming_id, local_id: Some(local_id), local_only, incoming_sessions: incoming_uuids.len() })
}

/// Replace the local catalog with `incoming` (the old one is kept as a backup) and
/// remember the incoming catalog's latest session as seen by this host.
/// Returns the backup path, if there was a local catalog.
pub fn import(incoming: &Path, local: &Path, state: &mut HostState) -> Result<Option<PathBuf>> {
    let backup = if local.exists() {
        {
            let cat = Catalog::open(local)?;
            let _ = cat.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        let stamp = now_secs();
        let bak = local.with_file_name(format!(
            "{}.bak-{stamp}",
            local.file_name().and_then(|n| n.to_str()).unwrap_or("index.db")
        ));
        std::fs::rename(local, &bak).with_context(|| format!("backing up {}", local.display()))?;
        for suffix in ["-wal", "-shm"] {
            let mut p = local.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
        Some(bak)
    } else {
        if let Some(dir) = local.parent() {
            std::fs::create_dir_all(dir)?;
        }
        None
    };
    std::fs::copy(incoming, local).with_context(|| format!("copying {} into place", incoming.display()))?;
    let cat = Catalog::open(local)?; // migrates if the copy is older
    if let Some(latest) = sessions(cat.conn(), 1)?.into_iter().next() {
        state.remember(&cat.catalog_id()?, &latest);
        state.save()?;
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, HostState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("cat/index.db");
        let state = HostState::load(&tmp.path().join("state/hosts.json")).unwrap();
        (tmp, db, state)
    }

    #[test]
    fn stale_copy_is_detected() {
        let (tmp, db, mut mac) = setup();
        let mut linux = HostState::load(&tmp.path().join("state/linux.json")).unwrap();

        let cat = Catalog::open(&db).unwrap();
        assert_eq!(cat.check_open("mac", &mac).unwrap().lineage, Lineage::New);
        let s1 = cat.begin_session("mac", &mut mac).unwrap();
        cat.end_session(&s1).unwrap();
        let copy_for_linux = tmp.path().join("handoff.db");
        cat.handoff_copy(&copy_for_linux).unwrap();
        assert!(cat.handoff_copy(&copy_for_linux).is_err(), "never overwrites");

        // The Mac keeps working on its own copy after the handoff.
        let s2 = cat.begin_session("mac", &mut mac).unwrap();
        cat.end_session(&s2).unwrap();
        drop(cat);

        // Linux works on the handed-off copy.
        let lcat = Catalog::open(&copy_for_linux).unwrap();
        assert_eq!(lcat.check_open("linux", &linux).unwrap().lineage, Lineage::New);
        let l1 = lcat.begin_session("linux", &mut linux).unwrap();
        lcat.end_session(&l1).unwrap();
        drop(lcat);

        // Back on the Mac, the Linux copy lacks the Mac's latest session s2.
        let lcat = Catalog::open(&copy_for_linux).unwrap();
        assert!(matches!(lcat.check_open("mac", &mac).unwrap().lineage, Lineage::Stale { last_session, .. } if last_session == s2));
        // ... and import refuses because the local catalog has work the copy lacks.
        let chk = check_import(&copy_for_linux, &db).unwrap();
        assert_eq!(chk.local_only.iter().map(|s| s.uuid.clone()).collect::<Vec<_>>(), vec![s2.clone()]);
        assert_eq!(chk.local_id.as_deref(), Some(chk.incoming_id.as_str()));
    }

    #[test]
    fn copied_while_open_and_import() {
        let (tmp, db, mut mac) = setup();
        let cat = Catalog::open(&db).unwrap();
        let _open = cat.begin_session("mac", &mut mac).unwrap();
        let copy = tmp.path().join("while-open.db");
        cat.handoff_copy(&copy).unwrap();

        let mut linux = HostState::load(&tmp.path().join("state/linux.json")).unwrap();
        let ccat = Catalog::open(&copy).unwrap();
        let chk = ccat.check_open("linux", &linux).unwrap();
        assert_eq!(chk.open_elsewhere.len(), 1);
        assert_eq!(chk.open_elsewhere[0].host, "mac");
        assert_eq!(chk.integrity.as_deref(), Some("ok"));
        drop(ccat);

        // Importing into a fresh location works and records the lineage.
        let target = tmp.path().join("linux/index.db");
        assert!(check_import(&copy, &target).unwrap().local_id.is_none());
        assert!(import(&copy, &target, &mut linux).unwrap().is_none());
        let t = Catalog::open(&target).unwrap();
        assert_eq!(t.check_open("linux", &linux).unwrap().lineage, Lineage::Current);
        // A second import keeps a backup of the replaced catalog.
        drop(t);
        let bak = import(&copy, &target, &mut linux).unwrap().unwrap();
        assert!(bak.exists());
    }

    #[test]
    fn dead_sessions_of_this_host_are_closed() {
        let (_tmp, db, mut st) = setup();
        let cat = Catalog::open(&db).unwrap();
        cat.conn()
            .execute(
                "INSERT INTO sessions (uuid, host, started_at, pid) VALUES ('dead', 'mac', 1, 999999999)",
                [],
            )
            .unwrap();
        cat.begin_session("mac", &mut st).unwrap();
        let ended: Option<i64> =
            cat.conn().query_row("SELECT ended_at FROM sessions WHERE uuid = 'dead'", [], |r| r.get(0)).unwrap();
        assert!(ended.is_some());
    }
}
