//! drive-index command-line interface (PLAN.md §8). The TUI (M3) will become the
//! default command; until then running without a subcommand prints help.

mod output;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use di_core::Catalog;
use di_core::search::{SearchError, SearchRequest, SortKey, search, summarize};
use di_scan::{Phase, PhaseCell, ResumeMode, ScanRequest, ScanStatus, run_dir_scan};
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
    /// Scan a directory or mounted volume into the catalog (resumes an interrupted scan)
    Scan {
        dir: PathBuf,
        /// Label for the drive (kept for later scans)
        #[arg(long)]
        label: Option<String>,
        /// Walker threads [default: number of cores, max 8]
        #[arg(long, short = 'j')]
        threads: Option<usize>,
        /// Discard an interrupted scan instead of resuming it
        #[arg(long)]
        restart: bool,
        /// Descend into other filesystems mounted below DIR
        #[arg(long)]
        cross_mounts: bool,
        /// Also record access times
        #[arg(long)]
        atime: bool,
        /// Don't show the progress line
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
        Command::Scan { dir, label, threads, restart, cross_mounts, atime, quiet } => {
            let mut walk = WalkConfig::default();
            if let Some(t) = threads {
                if t == 0 {
                    bail!("--threads must be at least 1");
                }
                walk.threads = t;
            }
            walk.one_file_system = !cross_mounts;
            walk.record_atime = atime;
            let req = ScanRequest {
                catalog_path: db,
                dir,
                label,
                walk,
                resume: if restart { ResumeMode::Restart } else { ResumeMode::Auto },
            };
            cmd_scan(&req, quiet)
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

fn parse_drive_id(s: &str) -> Result<i64> {
    let digits = s.strip_prefix(['D', 'd']).unwrap_or(s);
    digits.parse().with_context(|| format!("'{s}' is not a drive id like D3"))
}

fn cmd_scan(req: &ScanRequest, quiet: bool) -> Result<ExitCode> {
    let control = Arc::new(Control::default());
    let progress = Arc::new(Progress::default());
    let phase = Arc::new(PhaseCell::default());

    let interrupts = Arc::new(AtomicUsize::new(0));
    {
        let (control, interrupts) = (control.clone(), interrupts.clone());
        ctrlc::set_handler(move || {
            if interrupts.fetch_add(1, Ordering::SeqCst) == 0 {
                control.cancel();
            } else {
                // Second Ctrl-C: give up immediately. The staging DB stays consistent.
                std::process::exit(130);
            }
        })
        .context("installing Ctrl-C handler")?;
    }

    let show = !quiet && std::io::stderr().is_terminal();
    let start = Instant::now();
    let reporter = show.then(|| {
        let (progress, phase, control) = (progress.clone(), phase.clone(), control.clone());
        std::thread::spawn(move || output::progress_loop(&progress, &phase, &control, start))
    });

    let result = run_dir_scan(req, &control, &progress, &phase);
    phase.set(Phase::Done);
    if let Some(r) = reporter {
        let _ = r.join();
    }
    let out = result?;
    let t = &out.totals;
    match out.status {
        ScanStatus::Complete => {
            let files = progress.files.load(Ordering::Relaxed);
            let secs = out.walk_ms as f64 / 1000.0;
            let rate = if secs > 0.0 { files as f64 / secs } else { 0.0 };
            if let Some(done) = out.resumed_dirs {
                println!(
                    "Resumed scan #{} ({} directories were already done).",
                    out.scan_id,
                    di_model::units::format_count(done)
                );
            }
            println!(
                "Scanned D{} — {} files, {} dirs, {} in {} ({}/s, {}, {} threads); merged in {}.",
                out.drive_id,
                di_model::units::format_count(t.files),
                di_model::units::format_count(t.dirs),
                di_model::units::format_size(t.bytes),
                output::format_duration(Duration::from_millis(out.walk_ms)),
                di_model::units::format_count(rate as u64),
                out.method,
                req.walk.threads,
                output::format_duration(Duration::from_millis(out.merge_ms)),
            );
            if t.errors > 0 {
                println!("{} entries could not be read (permissions or I/O errors).", t.errors);
            }
            Ok(ExitCode::SUCCESS)
        }
        ScanStatus::Interrupted => {
            println!(
                "Scan #{} interrupted after {} directories. Run the same command again to resume, or add --restart.",
                out.scan_id,
                di_model::units::format_count(progress.dirs.load(Ordering::Relaxed))
            );
            Ok(ExitCode::from(130))
        }
    }
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
