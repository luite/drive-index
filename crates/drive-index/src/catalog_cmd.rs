//! Catalog sessions and `drive-index db …`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use di_core::Catalog;
use di_core::sessions::{self, HostState, Lineage, SessionInfo};
use di_model::units::format_size;
use di_walk::host;

use crate::output::local_time;

/// Host-local state file, outside the catalog's directory so copying the catalog
/// folder never copies it. `DRIVE_INDEX_STATE` overrides (testing).
pub fn state_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("DRIVE_INDEX_STATE") {
        return Ok(PathBuf::from(p));
    }
    let dirs = directories::ProjectDirs::from("", "", "drive-index").context("cannot determine a state directory")?;
    let dir = dirs.state_dir().unwrap_or_else(|| dirs.preference_dir()).to_path_buf();
    Ok(dir.join("hosts.json"))
}

fn describe_session(s: &SessionInfo) -> String {
    let end = match s.ended_at {
        Some(t) => local_time(Some(t)),
        None => "still open".into(),
    };
    format!(
        "{} → {}  {}  {} {}  {} scan(s)",
        local_time(Some(s.started_at)),
        end,
        s.host,
        s.host_os.as_deref().unwrap_or("?"),
        s.app_version.as_deref().unwrap_or("?"),
        s.scans
    )
}

/// A read-write use of the catalog. Refuses stale copies unless `accept_stale`.
pub struct SessionGuard {
    db: PathBuf,
    uuid: Option<String>,
}

impl SessionGuard {
    pub fn begin(db: &Path, accept_stale: bool) -> Result<SessionGuard> {
        let host = host::hostname();
        sessions::set_host_hint(&host);
        let cat = Catalog::open(db)?;
        let mut state = HostState::load(&state_path()?)?;
        let chk = cat.check_open(&host, &state)?;
        for s in &chk.open_elsewhere {
            eprintln!(
                "warning: this catalog is still marked open on {} (since {}). It was probably copied while in use there, \
                 so the newest changes from that host may be missing. Use `drive-index db handoff` to move catalogs.",
                s.host,
                local_time(Some(s.started_at))
            );
        }
        if let Some(ic) = &chk.integrity {
            if ic != "ok" {
                bail!("catalog integrity check failed ({ic}). Restore a good copy with `drive-index db import`.");
            }
        }
        if let Lineage::Stale { started_at, host: last_host, .. } = &chk.lineage {
            let msg = format!(
                "this catalog is an older copy: it lacks the session from {last_host} at {} that this host has already seen. \
                 Writing to it would lose track of that work. If an old file was copied over a newer one, restore the newer one \
                 (`drive-index db import`).",
                local_time(Some(*started_at))
            );
            if !accept_stale {
                bail!("{msg} To use this copy anyway, pass --accept-stale.");
            }
            eprintln!("warning: {msg} Continuing because of --accept-stale.");
        }
        let uuid = cat.begin_session(&host, &mut state)?;
        Ok(SessionGuard { db: db.to_path_buf(), uuid: Some(uuid) })
    }

    pub fn end(mut self) -> Result<()> {
        if let Some(u) = self.uuid.take() {
            Catalog::open(&self.db)?.end_session(&u)?;
        }
        Ok(())
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(u) = self.uuid.take() {
            if let Ok(c) = Catalog::open(&self.db) {
                let _ = c.end_session(&u);
            }
        }
    }
}

pub fn status(db: &Path, json: bool) -> Result<ExitCode> {
    sessions::set_host_hint(&host::hostname());
    let cat = Catalog::open_readonly(db)?;
    let state = HostState::load(&state_path()?)?;
    let st = cat.status(&state)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("Catalog   {}  ({}, schema {})", st.path, format_size(st.size_bytes), st.schema_version);
    println!("ID        {}", st.catalog_id);
    println!("Created   {}", local_time(st.created_at));
    let lineage = match &st.lineage {
        Lineage::New => "first use on this host".to_string(),
        Lineage::Current => "current (contains the latest session this host has seen)".to_string(),
        Lineage::Stale { host, started_at, .. } => format!(
            "STALE: lacks the session from {host} at {} that this host has seen; this is an older copy",
            local_time(Some(*started_at))
        ),
    };
    println!("Lineage   {lineage}");
    if st.open_sessions.is_empty() {
        println!("Open      no (closed cleanly; safe to copy)");
    } else {
        for s in &st.open_sessions {
            println!("Open      {} pid {} since {}", s.host, s.pid.unwrap_or(0), local_time(Some(s.started_at)));
        }
    }
    println!("Sessions (latest first):");
    for s in &st.recent_sessions {
        println!("  {}", describe_session(s));
    }
    Ok(ExitCode::SUCCESS)
}

pub fn handoff(db: &Path, dest: Option<PathBuf>) -> Result<ExitCode> {
    let host = host::hostname();
    let stamp = jiff::Zoned::now().strftime("%Y-%m-%d-%H%M%S").to_string();
    let name = format!("drive-index-{host}-{stamp}.db");
    let dest = match dest {
        Some(d) if d.is_dir() => d.join(name),
        Some(d) => d,
        None => std::env::current_dir()?.join(name),
    };
    let cat = Catalog::open(db)?;
    cat.handoff_copy(&dest)?;
    drop(cat);
    let bytes = std::fs::read(&dest)?;
    let hash = blake3::hash(&bytes).to_hex();
    println!("Wrote {} ({}, BLAKE3 {}).", dest.display(), format_size(bytes.len() as u64), &hash[..16]);
    println!("Copy it to the other host and run `drive-index db import <file>` there.");
    Ok(ExitCode::SUCCESS)
}

pub fn import(db: &Path, file: &Path, force: bool) -> Result<ExitCode> {
    let chk = sessions::check_import(file, db)?;
    if let Some(local_id) = &chk.local_id {
        if *local_id != chk.incoming_id && !force {
            bail!(
                "{} is a different catalog (id {}) than the local one (id {local_id}). Pass --force to replace the local catalog.",
                file.display(),
                chk.incoming_id
            );
        }
        if !chk.local_only.is_empty() && !force {
            let latest = &chk.local_only[0];
            bail!(
                "the local catalog has {} session(s) the incoming copy lacks (latest: {}). Importing would lose that work. \
                 Hand the local catalog off to the other host first, or pass --force (the local catalog is kept as a backup).",
                chk.local_only.len(),
                describe_session(latest)
            );
        }
    }
    let mut state = HostState::load(&state_path()?)?;
    let backup = sessions::import(file, db, &mut state)?;
    println!("Imported {} as {}.", file.display(), db.display());
    if let Some(b) = backup {
        println!("The previous catalog is kept at {}.", b.display());
    }
    Ok(ExitCode::SUCCESS)
}
