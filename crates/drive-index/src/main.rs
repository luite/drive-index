//! drive-index command-line interface. Run without a subcommand in a terminal, it
//! starts the TUI.

mod catalog_cmd;
mod lock;
mod output;
mod scan_cmd;
mod term;
mod watch;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use catalog_cmd::SessionGuard;
use di_core::Catalog;
use di_core::search::{SearchError, SearchRequest, SortKey, search, summarize};
use di_scan::controller::{ControllerOptions, NewDrivePolicy, Policy};
use di_scan::device::{DeviceScanRequest, IdentityChoice};
use di_tui::TuiOptions;
use di_scan::{ResumeMode, ScanRequest};
use di_walk::{Control, Progress, WalkConfig};

#[derive(Parser)]
#[command(name = "drive-index", version, about = "Catalog drives into a searchable SQLite index")]
struct Cli {
    /// Catalog database [default: platform data directory]
    #[arg(long, global = true, env = "DRIVE_INDEX_DB", value_name = "PATH")]
    db: Option<PathBuf>,

    /// Write to a catalog even though it is an older copy than this host has seen
    #[arg(long, global = true)]
    accept_stale: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a drive (/dev/disk7, disk7, /dev/sdb) or a directory into the catalog.
    /// Drives are mounted read-only, scanned, then unmounted and ejected.
    /// An interrupted scan resumes when run again.
    Scan {
        /// Device or directory
        target: String,
        /// Label for the drive (kept for later scans)
        #[arg(long)]
        label: Option<String>,
        /// Walker threads [default: number of cores, max 8]
        #[arg(long, short = 'j')]
        threads: Option<usize>,
        /// Discard an interrupted scan instead of resuming it
        #[arg(long)]
        restart: bool,
        /// Directories: descend into other filesystems mounted below DIR
        #[arg(long)]
        cross_mounts: bool,
        /// Also record access times
        #[arg(long)]
        atime: bool,
        /// Don't show the progress line
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Skip reading file contents (media metadata and quick hashes)
        #[arg(long)]
        no_content: bool,
        /// Threads for the content phase [default: 2, kind to hard disks]
        #[arg(long)]
        content_threads: Option<usize>,
        /// Read likely duplicates in full (BLAKE3) to verify them as exact duplicates
        #[arg(long, conflicts_with = "no_content")]
        verify: bool,
        /// Drives: don't read SMART health with smartctl
        #[arg(long)]
        no_smart: bool,
        /// Drives: what to do afterwards
        #[arg(long, value_enum, default_value_t = AfterArg::Eject)]
        after: AfterArg,
        /// Drives: treat as a new drive even if it matches a known one
        #[arg(long, conflicts_with = "same_as")]
        new_drive: bool,
        /// Drives: treat as this known drive (D3)
        #[arg(long, value_name = "DRIVE")]
        same_as: Option<String>,
        /// Drives: allow internal, non-removable disks (never the system disk)
        #[arg(long)]
        allow_internal: bool,
        /// Drives: don't walk filesystems scanned more recently than this (e.g. 1d, 12h);
        /// they are only read if file contents are still pending. 0 walks them all.
        #[arg(long, default_value = "0", value_name = "AGE")]
        rescan_after: String,
    },
    /// Interactive terminal UI (the default when run without a command in a terminal)
    Tui {
        /// Rescan known drives whose last scan is older than this (e.g. 30d, 12h, 0)
        #[arg(long, default_value = "30d")]
        rescan_after: String,
        /// Treat attached disk images like drives (for testing)
        #[arg(long)]
        include_images: bool,
        #[arg(long, value_enum, default_value_t = AfterArg::Eject)]
        after: AfterArg,
        #[arg(long, short = 'j')]
        threads: Option<usize>,
        /// Don't capture the mouse (keeps the terminal's own text selection)
        #[arg(long)]
        no_mouse: bool,
    },
    /// Watch for drives being connected; scan them and eject when done
    Watch {
        /// Also scan drives the catalog has never seen (otherwise they are only reported)
        #[arg(long)]
        scan_new: bool,
        /// Rescan known drives whose last scan is older than this (e.g. 30d, 12h, 0)
        #[arg(long, default_value = "30d")]
        rescan_after: String,
        /// Treat attached disk images like drives (for testing)
        #[arg(long)]
        include_images: bool,
        #[arg(long, value_enum, default_value_t = AfterArg::Eject)]
        after: AfterArg,
        #[arg(long, short = 'j')]
        threads: Option<usize>,
        #[arg(long, short = 'q')]
        quiet: bool,
    },
    /// Search the catalog (`search --filters` lists the query language)
    Search {
        /// Query terms, e.g. ext:cr2,crw date:2014 size:>10M
        #[arg(required_unless_present = "filters", num_args = 1.., allow_hyphen_values = true)]
        query: Vec<String>,
        /// Print the filter reference and exit
        #[arg(long)]
        filters: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = SortArg::Path)]
        sort: SortArg,
        /// Sort descending
        #[arg(long)]
        desc: bool,
        /// Print only the count and total size
        #[arg(long, short = 'c')]
        count: bool,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// List drives, filesystems or scans
    List {
        #[arg(value_enum, default_value_t = ListWhat::Drives)]
        what: ListWhat,
        #[arg(long)]
        json: bool,
    },
    /// Show drives, partitions and filesystems as the OS reports them (read-only)
    Info {
        /// Disk or volume (disk4, /dev/sdb1); omit to list all external drives
        dev: Option<String>,
        /// Include internal and system disks in the listing
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Walk a directory without touching any database (walker benchmark)
    #[command(hide = true)]
    BenchWalk {
        dir: PathBuf,
        #[arg(long, short = 'j')]
        threads: Option<usize>,
        /// Use the portable lister instead of the platform fast path
        #[arg(long)]
        generic: bool,
    },
    /// Duplicate files: groups of copies, largest wasted space first
    Dupes {
        /// exact (verified full hash), likely (quick hash + size), probable (name + size)
        #[arg(long, value_enum, default_value_t = ConfidenceArg::Likely)]
        confidence: ConfidenceArg,
        /// Ignore files smaller than this
        #[arg(long, default_value = "1M")]
        min_size: String,
        /// Only files matching this search query (e.g. "in:photos-raw")
        #[arg(long = "in", value_name = "QUERY")]
        filter: Option<String>,
        /// Only groups whose copies are all on one drive
        #[arg(long)]
        same_drive: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// List each group's files
        #[arg(long, short = 'f')]
        files: bool,
        #[arg(long)]
        json: bool,
    },
    /// Which drives hold how much of a collection (photos-raw, video, …); without
    /// an argument, lists the collections
    Coverage {
        collection: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// SMART health: read a connected drive now (disk4, /dev/sdb), or show the
    /// readings stored for a catalogued drive (D3)
    Smart {
        target: String,
        /// smartctl's JSON (device) or all stored readings (drive)
        #[arg(long)]
        json: bool,
    },
    /// Set a drive's label (D3 or 3)
    Label {
        drive: String,
        /// New label; omit with --clear to remove it
        label: Option<String>,
        #[arg(long, conflicts_with = "label")]
        clear: bool,
    },
    /// Check external tools, drivers and permissions, with install hints
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Catalog maintenance and moving it between hosts
    Db {
        #[command(subcommand)]
        cmd: DbCmd,
    },
}

#[derive(Subcommand)]
enum DbCmd {
    /// Catalog id, size, sessions per host, and whether it is safe to copy
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Write a consistent copy for moving the catalog to another host
    Handoff {
        /// File or directory [default: current directory]
        dest: Option<PathBuf>,
    },
    /// Replace the local catalog with a copy from another host (keeps a backup)
    Import {
        file: PathBuf,
        /// Replace even if the local catalog has work the copy lacks
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ConfidenceArg {
    Exact,
    Likely,
    Probable,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Path,
    Name,
    Size,
    Mtime,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AfterArg {
    /// Unmount and eject (power off)
    Eject,
    /// Unmount only
    Unmount,
    /// Leave mounted (read-only)
    Keep,
}

impl AfterArg {
    pub fn to_after(self) -> di_scan::device::After {
        match self {
            AfterArg::Eject => di_scan::device::After::Eject,
            AfterArg::Unmount => di_scan::device::After::Unmount,
            AfterArg::Keep => di_scan::device::After::KeepMounted,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ListWhat {
    Drives,
    Filesystems,
    Scans,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn catalog_path(cli_db: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = cli_db {
        return Ok(p);
    }
    let dirs = directories::ProjectDirs::from("", "", "drive-index")
        .context("cannot determine a data directory; pass --db")?;
    Ok(dirs.data_dir().join("index.db"))
}

fn run(cli: Cli) -> Result<ExitCode> {
    let db = catalog_path(cli.db)?;
    let accept_stale = cli.accept_stale;
    let command = match cli.command {
        Some(c) => c,
        None if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() => {
            Command::Tui { rescan_after: "30d".into(), include_images: false, after: AfterArg::Eject, threads: None, no_mouse: false }
        }
        None => {
            Cli::command().print_help()?;
            println!();
            return Ok(ExitCode::SUCCESS);
        }
    };
    match command {
        Command::Scan {
            target,
            label,
            threads,
            restart,
            cross_mounts,
            atime,
            quiet,
            no_content,
            content_threads,
            verify,
            no_smart,
            after,
            new_drive,
            same_as,
            allow_internal,
            rescan_after,
        } => {
            let content = (!no_content).then(|| {
                let mut c = di_scan::content::ContentConfig::default();
                if let Some(n) = content_threads {
                    c.threads = n.max(1);
                }
                c.verify = verify;
                c
            });
            let mut walk = walk_config(threads)?;
            walk.one_file_system = !cross_mounts;
            walk.record_atime = atime;
            let resume = if restart { ResumeMode::Restart } else { ResumeMode::Auto };
            if scan_cmd::is_device(&target) {
                let identity = match (new_drive, same_as) {
                    (true, _) => IdentityChoice::New,
                    (_, Some(d)) => IdentityChoice::SameAs(parse_drive_id(&d)?),
                    _ => IdentityChoice::Auto,
                };
                let req = DeviceScanRequest {
                    catalog_path: db,
                    dev: target,
                    label,
                    walk,
                    resume,
                    after: after.to_after(),
                    identity,
                    allow_internal,
                    content,
                    smart: !no_smart,
                    rescan_after_secs: watch::parse_age(&rescan_after)?,
                };
                let _lock = lock::lock_or_fail(&req.catalog_path)?;
                let session = SessionGuard::begin(&req.catalog_path, accept_stale)?;
                let code = scan_cmd::scan_device(&req, quiet)?;
                session.end()?;
                Ok(code)
            } else {
                let req = ScanRequest { catalog_path: db, dir: PathBuf::from(target), label, walk, resume, content };
                let session = SessionGuard::begin(&req.catalog_path, accept_stale)?;
                let code = scan_cmd::scan_dir(&req, quiet)?;
                session.end()?;
                Ok(code)
            }
        }
        Command::Tui { rescan_after, include_images, after, threads, no_mouse } => {
            let controller = ControllerOptions {
                catalog_path: db.clone(),
                policy: Policy {
                    new_drive: NewDrivePolicy::Ask,
                    rescan_after_secs: watch::parse_age(&rescan_after)?,
                    after: after.to_after(),
                    include_images,
                },
                walk: walk_config(threads)?,
            };
            let host = di_walk::host::hostname();
            match lock::try_lock(&db)? {
                Ok(_lock) => {
                    let session = SessionGuard::begin(&db, accept_stale)?;
                    di_tui::run(TuiOptions {
                        catalog_path: db.clone(),
                        host,
                        controller: Some(controller),
                        viewer_reason: None,
                        mouse: !no_mouse,
                    })?;
                    session.end()?;
                }
                Err(holder) => {
                    di_tui::run(TuiOptions {
                        catalog_path: db.clone(),
                        host,
                        controller: None,
                        viewer_reason: Some(holder),
                        mouse: !no_mouse,
                    })?
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch { scan_new, rescan_after, include_images, after, threads, quiet } => {
            let opts = watch::WatchOptions {
                catalog_path: db,
                scan_new,
                rescan_after_secs: watch::parse_age(&rescan_after)?,
                include_images,
                after: after.to_after(),
                walk: walk_config(threads)?,
                quiet,
            };
            let _lock = lock::lock_or_fail(&opts.catalog_path)?;
            let session = SessionGuard::begin(&opts.catalog_path, accept_stale)?;
            let code = watch::run(&opts)?;
            session.end()?;
            Ok(code)
        }
        Command::Search { filters: true, json, .. } => {
            output::print_filters(json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Search { query, limit, sort, desc, count, json, .. } => {
            let query = query.join(" ");
            let cat = Catalog::open_readonly(&db)?;
            cmd_search(&cat, &query, limit, sort, desc, count, json)
        }
        Command::List { what, json } => {
            let cat = Catalog::open_readonly(&db)?;
            match what {
                ListWhat::Drives => output::print_drives(&cat.drives()?, json)?,
                ListWhat::Filesystems => output::print_filesystems(&cat.filesystems()?, json)?,
                ListWhat::Scans => output::print_scans(&cat.scans(50)?, json)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Info { dev, all, json } => cmd_info(dev.as_deref(), all, json),
        Command::BenchWalk { dir, threads, generic } => cmd_bench_walk(&dir, threads, generic),
        Command::Dupes { confidence, min_size, filter, same_drive, limit, files, json } => {
            let cat = Catalog::open_readonly(&db)?;
            let q = di_core::dupes::DupeQuery {
                confidence: match confidence {
                    ConfidenceArg::Exact => di_core::dupes::Confidence::Exact,
                    ConfidenceArg::Likely => di_core::dupes::Confidence::Likely,
                    ConfidenceArg::Probable => di_core::dupes::Confidence::Probable,
                },
                min_size: di_model::units::parse_size(&min_size).map_err(|e| anyhow::anyhow!("--min-size: {e}"))?,
                filter,
                same_drive_only: same_drive,
                limit,
            };
            output::print_dupes(&cat, &q, files, json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Coverage { collection, json } => {
            let cat = Catalog::open_readonly(&db)?;
            match collection {
                Some(c) => output::print_coverage(&di_core::coverage::coverage(cat.conn(), &c)?, json)?,
                None => output::print_collections(&di_core::coverage::collections(cat.conn())?, json)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Smart { target, json } => cmd_smart(&db, &target, json),
        Command::Label { drive, label, clear } => {
            let id = parse_drive_id(&drive)?;
            if label.is_none() && !clear {
                bail!("give a label, or --clear to remove it");
            }
            let session = SessionGuard::begin(&db, accept_stale)?;
            let cat = Catalog::open(&db)?;
            let changed = cat.set_drive_label(id, label.as_deref())?;
            drop(cat);
            session.end()?;
            if !changed {
                bail!("no drive D{id} (see `drive-index list`)");
            }
            match label {
                Some(l) => println!("D{id} labelled \"{l}\""),
                None => println!("D{id} label cleared"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { json } => {
            let report = di_doctor::run_checks(&di_walk::host::hostname());
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                output::print_doctor(&report, &db, &catalog_cmd::state_path()?);
            }
            Ok(if report.essential_failures() > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        Command::Db { cmd } => match cmd {
            DbCmd::Status { json } => catalog_cmd::status(&db, json),
            DbCmd::Handoff { dest } => catalog_cmd::handoff(&db, dest),
            DbCmd::Import { file, force } => catalog_cmd::import(&db, &file, force),
        },
    }
}

fn cmd_info(dev: Option<&str>, all: bool, json: bool) -> Result<ExitCode> {
    let probes = match dev {
        Some(d) => vec![di_platform::probe(d)?],
        None => {
            let mut v = Vec::new();
            for id in di_platform::list_drives()? {
                match di_platform::probe(&id) {
                    Ok(p) if all || p.is_external() => v.push(p),
                    Ok(_) => {}
                    Err(e) => eprintln!("warning: {id}: {e:#}"),
                }
            }
            v
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&probes)?);
    } else if probes.is_empty() {
        println!("No external drives found (use --all to include internal disks).");
    } else {
        for p in &probes {
            output::print_probe(p);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_bench_walk(dir: &std::path::Path, threads: Option<usize>, generic: bool) -> Result<ExitCode> {
    use di_walk::{DirJob, Lister, best_lister, list::GenericLister, stat_path, walk};
    let root = dir.canonicalize()?;
    let (_, meta) = stat_path(&root)?;
    let mut cfg = WalkConfig::default();
    if let Some(t) = threads {
        cfg.threads = t.max(1);
    }
    let lister: Box<dyn Lister> = if generic { Box::new(GenericLister) } else { best_lister() };
    let (tx, rx) = crossbeam_channel::unbounded();
    let sink = std::thread::spawn(move || rx.iter().map(|b: di_walk::DirBatch| b.files.len() as u64).sum::<u64>());
    let job = DirJob { id: 1, os_path: vec![], path: "/".into(), depth: 0, in_trash: false };
    let progress = Progress::default();
    let start = Instant::now();
    walk(&root, meta.dev, vec![job], 2, &cfg, lister.as_ref(), &Control::default(), &progress, &tx);
    drop(tx);
    let files = sink.join().map_err(|_| anyhow::anyhow!("sink panicked"))?;
    let secs = start.elapsed().as_secs_f64();
    let dirs = progress.dirs.load(Ordering::Relaxed);
    println!(
        "{}: {} files, {} dirs in {:.0} ms with {} threads ({}/s entries)",
        lister.method(),
        di_model::units::format_count(files),
        di_model::units::format_count(dirs),
        secs * 1000.0,
        cfg.threads,
        di_model::units::format_count(((files + dirs) as f64 / secs) as u64)
    );
    Ok(ExitCode::SUCCESS)
}

pub fn parse_drive_id(s: &str) -> Result<i64> {
    let digits = s.strip_prefix(['D', 'd']).unwrap_or(s);
    digits.parse().with_context(|| format!("'{s}' is not a drive id like D3"))
}

fn walk_config(threads: Option<usize>) -> Result<WalkConfig> {
    let mut walk = WalkConfig::default();
    if let Some(t) = threads {
        if t == 0 {
            bail!("--threads must be at least 1");
        }
        walk.threads = t;
    }
    Ok(walk)
}

fn cmd_smart(db: &std::path::Path, target: &str, json: bool) -> Result<ExitCode> {
    let catalogued = target.strip_prefix(['D', 'd']).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    if catalogued {
        let id = parse_drive_id(target)?;
        let cat = Catalog::open_readonly(db)?;
        let history = di_core::smart::history(cat.conn(), id, if json { 1000 } else { 10 })?;
        let access = di_core::smart::access(cat.conn(), id)?;
        output::print_smart_history(id, &history, &access, json)?;
        return Ok(ExitCode::SUCCESS);
    }
    let probe = di_platform::probe(target)?;
    match di_platform::smart::read(&probe, None) {
        Ok(r) if json => println!("{}", r.raw_json),
        Ok(r) => output::print_smart(&probe, &r),
        Err(e) => {
            eprintln!("{}: SMART unavailable: {}", probe.dev_path, e.reason());
            if let di_model::smart::SmartUnavailable::NeedsRoot(_) = e {
                eprintln!("Try: sudo drive-index smart {target}");
            }
            return Ok(ExitCode::FAILURE);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_search(cat: &Catalog, query: &str, limit: usize, sort: SortArg, desc: bool, count: bool, json: bool) -> Result<ExitCode> {
    let report = |e: SearchError| -> Result<ExitCode> {
        match e {
            SearchError::Query(q) => {
                eprintln!("error: {}", q.message);
                eprintln!("  {query}");
                let width = query[q.start..q.end.min(query.len())].chars().count().max(1);
                eprintln!("  {}{}", " ".repeat(query[..q.start].chars().count()), "^".repeat(width));
                eprintln!("(drive-index search --filters lists every filter)");
                Ok(ExitCode::from(2))
            }
            SearchError::Db(e) => Err(e.into()),
        }
    };

    let summary = match summarize(cat.conn(), query) {
        Ok(s) => s,
        Err(e) => return report(e),
    };
    if count {
        if json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            output::print_summary(&summary, None);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let sort = match sort {
        SortArg::Path => SortKey::Path,
        SortArg::Name => SortKey::Name,
        SortArg::Size => SortKey::Size,
        SortArg::Mtime => SortKey::Mtime,
    };
    let req = SearchRequest { query, sort, descending: desc, limit, offset: 0 };
    let hits = match search(cat.conn(), &req) {
        Ok(h) => h,
        Err(e) => return report(e),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "hits": hits, "summary": summary }))?);
    } else {
        output::print_hits(&hits);
        output::print_summary(&summary, Some(hits.len()));
    }
    Ok(ExitCode::SUCCESS)
}
