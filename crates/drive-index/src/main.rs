//! drive-index command-line interface (PLAN.md §8). The TUI (M3) will become the
//! default command; until then running without a subcommand prints help.

mod output;
mod scan_cmd;
mod term;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use di_core::Catalog;
use di_core::search::{SearchError, SearchRequest, SortKey, search, summarize};
use di_scan::device::{DeviceScanRequest, IdentityChoice};
use di_scan::{ResumeMode, ScanRequest};
use di_walk::{Control, Progress, WalkConfig};

#[derive(Parser)]
#[command(name = "drive-index", version, about = "Catalog drives into a searchable SQLite index")]
struct Cli {
    /// Catalog database [default: platform data directory]
    #[arg(long, global = true, env = "DRIVE_INDEX_DB", value_name = "PATH")]
    db: Option<PathBuf>,

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
    /// Search the catalog (see PLAN.md §6 for the query language)
    Search {
        /// Query terms, e.g. ext:cr2,crw date:2014 size:>10M
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        query: Vec<String>,
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
    /// Set a drive's label (D3 or 3)
    Label {
        drive: String,
        /// New label; omit with --clear to remove it
        label: Option<String>,
        #[arg(long, conflicts_with = "label")]
        clear: bool,
    },
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
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(ExitCode::SUCCESS);
    };
    let db = catalog_path(cli.db)?;
    match command {
        Command::Scan {
            target,
            label,
            threads,
            restart,
            cross_mounts,
            atime,
            quiet,
            after,
            new_drive,
            same_as,
            allow_internal,
        } => {
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
                };
                scan_cmd::scan_device(&req, quiet)
            } else {
                let req = ScanRequest { catalog_path: db, dir: PathBuf::from(target), label, walk, resume };
                scan_cmd::scan_dir(&req, quiet)
            }
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
            watch::run(&opts)
        }
        Command::Search { query, limit, sort, desc, count, json } => {
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
        Command::Label { drive, label, clear } => {
            let id = parse_drive_id(&drive)?;
            if label.is_none() && !clear {
                bail!("give a label, or --clear to remove it");
            }
            let cat = Catalog::open(&db)?;
            if !cat.set_drive_label(id, label.as_deref())? {
                bail!("no drive D{id} (see `drive-index list`)");
            }
            match label {
                Some(l) => println!("D{id} labelled \"{l}\""),
                None => println!("D{id} label cleared"),
            }
            Ok(ExitCode::SUCCESS)
        }
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

fn cmd_search(cat: &Catalog, query: &str, limit: usize, sort: SortArg, desc: bool, count: bool, json: bool) -> Result<ExitCode> {
    let report = |e: SearchError| -> Result<ExitCode> {
        match e {
            SearchError::Query(q) => {
                eprintln!("error: {}", q.message);
                eprintln!("  {query}");
                let width = query[q.start..q.end.min(query.len())].chars().count().max(1);
                eprintln!("  {}{}", " ".repeat(query[..q.start].chars().count()), "^".repeat(width));
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
