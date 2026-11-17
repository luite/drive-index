//! End-to-end: scan real directory trees into a catalog, rescan, resume, search.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use di_core::Catalog;
use di_core::search::{SearchRequest, SortKey, search, summarize};
use di_scan::{PhaseCell, ResumeMode, ScanRequest, ScanStatus, run_dir_scan};
use di_walk::{Control, Progress, WalkConfig};

fn request(db: &Path, dir: &Path) -> ScanRequest {
    ScanRequest {
        catalog_path: db.to_path_buf(),
        dir: dir.to_path_buf(),
        label: Some("Test Drive".into()),
        walk: WalkConfig { threads: 4, ..WalkConfig::default() },
        resume: ResumeMode::Auto,
        content: None,
    }
}

fn scan(db: &Path, dir: &Path) -> di_scan::ScanOutcome {
    run_dir_scan(&request(db, dir), &Control::default(), &Progress::default(), &PhaseCell::default()).unwrap()
}

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

fn photo_tree(root: &Path) {
    write(&root.join("Photos/2014/2014-07 Iceland/RAW/IMG_4411.CR2"), 24_000);
    write(&root.join("Photos/2014/2014-07 Iceland/RAW/IMG_4412.CR2"), 23_000);
    write(&root.join("Photos/2014/2014-07 Iceland/IMG_4411.JPG"), 5_000);
    write(&root.join("Photos/2015/beach_day.cr2"), 30_000);
    write(&root.join("Video/wedding_full.mov"), 400_000);
    write(&root.join("Docs/notes.txt"), 100);
    write(&root.join("Docs/100%_done_report.pdf"), 1_000);
    write(&root.join(".Trashes/501/old.jpg"), 2_000);
    write(&root.join(".Spotlight-V100/Store-V2/store.db"), 9_999);
}

fn paths(cat: &Catalog, q: &str) -> BTreeSet<String> {
    let req = SearchRequest { query: q, sort: SortKey::Path, descending: false, limit: 10_000, offset: 0 };
    search(cat.conn(), &req).unwrap_or_else(|e| panic!("query {q:?}: {e}")).into_iter().map(|h| h.path).collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn scan_search_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("cat/index.db");
    let data = tmp.path().join("data");
    photo_tree(&data);

    let out = scan(&db, &data);
    assert_eq!(out.status, ScanStatus::Complete);
    assert_eq!(out.totals.files, 8, "spotlight store must be skipped");
    let cat = Catalog::open_readonly(&db).unwrap();

    // Name search: substring (FTS path), glob, short term (no FTS), LIKE-special chars.
    assert_eq!(
        paths(&cat, "iceland"),
        BTreeSet::new(),
        "names only; 'iceland' is in the dir path"
    );
    assert_eq!(
        paths(&cat, "IMG_4411"),
        set(&["/Photos/2014/2014-07 Iceland/IMG_4411.JPG", "/Photos/2014/2014-07 Iceland/RAW/IMG_4411.CR2"])
    );
    assert_eq!(paths(&cat, "IMG_44*.cr2").len(), 2);
    assert_eq!(paths(&cat, "mo"), set(&["/Video/wedding_full.mov"]), "2-char term, no FTS");
    assert_eq!(paths(&cat, "100%"), set(&["/Docs/100%_done_report.pdf"]));
    assert_eq!(paths(&cat, "1_0").len(), 0, "underscore is literal, not a wildcard");

    // Filters.
    assert_eq!(paths(&cat, "ext:cr2").len(), 3);
    assert_eq!(paths(&cat, "type:photo_raw path:2014").len(), 2);
    assert_eq!(paths(&cat, "in:photos-raw -path:2014"), set(&["/Photos/2015/beach_day.cr2"]));
    assert_eq!(paths(&cat, "size:>100k"), set(&["/Video/wedding_full.mov"]));
    assert_eq!(paths(&cat, "size:20k..25k").len(), 2);
    assert_eq!(paths(&cat, "trash:yes"), set(&["/.Trashes/501/old.jpg"]));
    assert_eq!(paths(&cat, "drive:\"test*\" ext:txt"), set(&["/Docs/notes.txt"]));
    assert_eq!(paths(&cat, &format!("drive:D{} ext:pdf", out.drive_id)).len(), 1);
    assert_eq!(paths(&cat, "date:<1d").len(), 8, "everything was written just now");
    assert_eq!(paths(&cat, "date:>1d").len(), 0);
    assert_eq!(paths(&cat, "date:2014").len(), 0);

    let summary = summarize(cat.conn(), "type:photo_raw").unwrap();
    assert_eq!((summary.files, summary.bytes), (3, 77_000));

    // Errors are reported, not panics.
    let req = SearchRequest { query: "online:yes", sort: SortKey::Path, descending: false, limit: 10, offset: 0 };
    assert!(search(cat.conn(), &req).unwrap_err().to_string().contains("not built yet"));

    // Aggregates: root totals equal the scan totals.
    let (tf, tb): (i64, i64) = cat
        .conn()
        .query_row(
            "SELECT tree_files, tree_bytes FROM dirs WHERE filesystem_id = ?1 AND parent_id IS NULL",
            [out.filesystem_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((tf as u64, tb as u64), (out.totals.files, out.totals.bytes));
    drop(cat);

    // Pretend the content phase hashed two files.
    {
        let cat = Catalog::open(&db).unwrap();
        cat.conn()
            .execute_batch(
                "INSERT INTO file_hashes (file_id, quick, hashed_at)
                 SELECT id, x'01', 1 FROM files WHERE name IN ('notes.txt', 'beach_day.cr2');",
            )
            .unwrap();
    }

    // Modify the tree and rescan: one changed file, one deleted, one added.
    write(&data.join("Photos/2015/beach_day.cr2"), 31_000);
    fs::remove_file(data.join("Video/wedding_full.mov")).unwrap();
    write(&data.join("Video/new_clip.mp4"), 1234);

    let out2 = scan(&db, &data);
    assert_eq!(out2.status, ScanStatus::Complete);
    assert_eq!((out2.drive_id, out2.filesystem_id), (out.drive_id, out.filesystem_id));
    assert_eq!(out2.merge.old_files_deleted, 8);
    assert_eq!(out2.merge.hashes_carried, 1, "only the unchanged file keeps its hash");

    let cat = Catalog::open_readonly(&db).unwrap();
    assert!(paths(&cat, "wedding").is_empty(), "deleted file gone, FTS entry too");
    assert_eq!(paths(&cat, "new_clip"), set(&["/Video/new_clip.mp4"]));
    let hashed: Vec<String> = cat
        .conn()
        .prepare("SELECT f.name FROM file_hashes h JOIN files f ON f.id = h.file_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(hashed, vec!["notes.txt".to_string()]);

    // FTS index and table agree exactly.
    let fts_rows: i64 = cat.conn().query_row("SELECT count(*) FROM files_fts WHERE name LIKE '%'", [], |r| r.get(0)).unwrap();
    let integrity: String = cat
        .conn()
        .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
        .map(|n| format!("{n}"))
        .unwrap();
    assert_eq!(fts_rows.to_string(), integrity);
    let scans = cat.scans(10).unwrap();
    assert_eq!(scans.iter().map(|s| s.status.as_str()).collect::<Vec<_>>(), vec!["complete", "superseded"]);
    let fk_violations: i64 =
        cat.conn().query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
    assert_eq!(fk_violations, 0);
}

#[test]
fn interrupted_scan_resumes_without_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let data = tmp.path().join("data");
    let mut expected = 0;
    for a in 0..30 {
        for b in 0..20 {
            write(&data.join(format!("d{a}/e{b}/f.bin")), a + b);
            write(&data.join(format!("d{a}/e{b}/g.bin")), 1);
            expected += 2;
        }
    }

    // First run: cancel once some directories are done.
    let control = Arc::new(Control::default());
    let progress = Arc::new(Progress::default());
    let mut req = request(&db, &data);
    req.walk.threads = 1;
    let watcher = {
        let (c, p) = (control.clone(), progress.clone());
        std::thread::spawn(move || {
            while p.dirs.load(Ordering::Relaxed) < 40 {
                std::thread::yield_now();
            }
            c.cancel();
        })
    };
    let first = run_dir_scan(&req, &control, &progress, &PhaseCell::default()).unwrap();
    watcher.join().unwrap();
    assert_eq!(first.status, ScanStatus::Interrupted);
    {
        let cat = Catalog::open_readonly(&db).unwrap();
        let n: i64 = cat.conn().query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "nothing merged before completion");
        assert_eq!(cat.scans(1).unwrap()[0].status, "interrupted");
    }

    // Second run resumes the same scan and finishes it.
    let second = scan(&db, &data);
    assert_eq!(second.status, ScanStatus::Complete);
    assert_eq!(second.scan_id, first.scan_id);
    assert!(second.resumed_dirs.unwrap() >= 40);
    assert_eq!(second.totals.files, expected as u64);

    let cat = Catalog::open_readonly(&db).unwrap();
    let distinct: i64 = cat
        .conn()
        .query_row("SELECT count(DISTINCT di.path || '/' || f.name) FROM files f JOIN dirs di ON di.id = f.dir_id", [], |r| r.get(0))
        .unwrap();
    assert_eq!(distinct, expected as i64);
    assert!(!cat.staging_dir().join(format!("{}.db", first.scan_id)).exists(), "staging cleaned up");
}

#[test]
fn restart_discards_interrupted_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let data = tmp.path().join("data");
    write(&data.join("a/b.txt"), 10);

    let control = Control::default();
    control.cancel();
    let first = run_dir_scan(&request(&db, &data), &control, &Progress::default(), &PhaseCell::default()).unwrap();
    assert_eq!(first.status, ScanStatus::Interrupted);

    let mut req = request(&db, &data);
    req.resume = ResumeMode::Restart;
    let second = run_dir_scan(&req, &Control::default(), &Progress::default(), &PhaseCell::default()).unwrap();
    assert_ne!(second.scan_id, first.scan_id);
    let cat = Catalog::open_readonly(&db).unwrap();
    let statuses: Vec<String> = cat.scans(10).unwrap().into_iter().map(|s| s.status).collect();
    assert_eq!(statuses, vec!["complete", "aborted"]);
}

#[test]
fn hardlinks_counted_once() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let data = tmp.path().join("data");
    write(&data.join("big.bin"), 50_000);
    fs::create_dir_all(data.join("links")).unwrap();
    fs::hard_link(data.join("big.bin"), data.join("links/big-again.bin")).unwrap();
    let out = scan(&db, &data);
    assert_eq!(out.totals.files, 2);
    assert_eq!(out.totals.bytes, 50_000);
}

#[test]
fn content_phase_extracts_media_once() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../di-media/tests/fixtures");
    let data = tmp.path().join("media");
    fs::create_dir_all(&data).unwrap();
    for e in fs::read_dir(&fixtures).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), data.join(e.file_name())).unwrap();
    }
    // A duplicate candidate: two identical non-media files of 2 MiB.
    write(&data.join("big1.bin"), 2 << 20);
    write(&data.join("big2.bin"), 2 << 20);

    let mut req = request(&db, &data);
    req.content = Some(di_scan::content::ContentConfig { ffprobe: None, exiftool: None, ..Default::default() });
    let out = run_dir_scan(&req, &Control::default(), &Progress::default(), &PhaseCell::default()).unwrap();
    let c = out.content.unwrap();
    assert_eq!(c.media, 9, "{c:?}");
    assert_eq!(c.hashed, 11, "9 media files + 2 duplicate candidates");
    assert_eq!(c.errors, 0);

    let cat = Catalog::open_readonly(&db).unwrap();
    for (q, n) in [
        ("taken:2014-07", 1),
        ("camera:canon", 1),
        ("camera:\"iphone*\"", 1),
        ("camera:\"EOS 5D*\"", 1),
        ("gps:yes", 4),
        ("artist:testers", 1),
        ("year:<2000", 1),
        ("duration:>1500ms", 2),
        ("res:>=320x240", 3),
        ("media:none", 2),
        ("media:partial", 3),
    ] {
        assert_eq!(paths(&cat, q).len(), n, "query {q}");
    }
    let same: i64 = cat
        .conn()
        .query_row(
            "SELECT count(*) FROM file_hashes a JOIN file_hashes b ON a.quick = b.quick AND a.file_id < b.file_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(same, 1, "the two identical files share a quick hash");
    drop(cat);

    // Rescan: unchanged files keep their rows, nothing is read again.
    let again = run_dir_scan(&req, &Control::default(), &Progress::default(), &PhaseCell::default()).unwrap();
    assert_eq!(again.content.unwrap().processed, 0);
    assert_eq!(again.merge.media_carried, 9);
    assert_eq!(again.merge.hashes_carried, 11);
}

fn write_fill(path: &Path, bytes: usize, fill: u8) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut v = vec![fill; bytes];
    v[bytes / 2] = fill.wrapping_add(1); // not all one byte
    fs::write(path, v).unwrap();
}

#[test]
fn duplicates_and_coverage_across_drives() {
    use di_core::coverage::coverage;
    use di_core::dupes::{Confidence, DupeQuery, groups, members};
    const M: usize = 1 << 20;
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write_fill(&a.join("RAW/a.CR2"), 2 * M, 1); // content X
    write_fill(&a.join("RAW/copy_of_a.CR2"), 2 * M, 1); // X again, same drive
    write_fill(&a.join("RAW/b.CR2"), 2 * M, 2); // Y
    write_fill(&a.join("RAW/c.CR2"), 3 * M, 3); // Z, only here
    write_fill(&b.join("a.CR2"), 2 * M, 1); // X on the other drive
    write_fill(&b.join("b.CR2"), 2 * M, 4); // same name and size as Y, different content
    write_fill(&b.join("misc/d.bin"), 2 * M, 1); // X under another name

    let content = |verify| Some(di_scan::content::ContentConfig { ffprobe: None, exiftool: None, verify, ..Default::default() });
    let run = |dir: &Path, verify: bool| {
        let mut req = request(&db, dir);
        req.label = None;
        req.content = content(verify);
        run_dir_scan(&req, &Control::default(), &Progress::default(), &PhaseCell::default()).unwrap()
    };
    let (da, db_) = (run(&a, false).drive_id, run(&b, false).drive_id);
    assert_ne!(da, db_);

    let cat = Catalog::open_readonly(&db).unwrap();
    let q = |confidence, filter: Option<&str>, same_drive_only| DupeQuery {
        confidence,
        min_size: 0,
        filter: filter.map(str::to_string),
        same_drive_only,
        limit: 100,
    };
    let (g, sum) = groups(cat.conn(), &q(Confidence::Likely, None, false)).unwrap();
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!((g[0].copies, g[0].size, g[0].wasted), (4, 2 * M as u64, 6 * M as u64));
    assert_eq!(g[0].drives.len(), 2);
    assert_eq!((sum.groups, sum.files), (1, 4));
    let m = members(cat.conn(), Confidence::Likely, &g[0].key, g[0].size).unwrap();
    assert_eq!(m.iter().map(|m| m.path.as_str()).collect::<BTreeSet<_>>(), set(&["/RAW/a.CR2", "/RAW/copy_of_a.CR2", "/a.CR2", "/misc/d.bin"]).iter().map(String::as_str).collect());

    let (g, _) = groups(cat.conn(), &q(Confidence::Likely, Some("ext:cr2"), false)).unwrap();
    assert_eq!(g[0].copies, 3);
    assert!(groups(cat.conn(), &q(Confidence::Likely, None, true)).unwrap().0.is_empty(), "the group spans two drives");
    let (g, _) = groups(cat.conn(), &q(Confidence::Probable, None, false)).unwrap();
    assert_eq!(g.len(), 2, "a.cr2 and b.cr2 by name and size: {g:?}");
    assert!(groups(cat.conn(), &q(Confidence::Exact, None, false)).unwrap().0.is_empty(), "nothing verified yet");

    // Search filters agree.
    assert_eq!(paths(&cat, "dup:likely").len(), 4);
    assert_eq!(paths(&cat, "dup:probable").len(), 4);
    assert_eq!(paths(&cat, "type:photo_raw copies:1"), set(&["/RAW/b.CR2", "/RAW/c.CR2", "/b.CR2"]));
    assert_eq!(paths(&cat, "copies:2").len(), 4);

    // Coverage of RAW photos: items X, Y, Z (drive A) and Y' (drive B).
    let c = coverage(cat.conn(), "photos-raw").unwrap();
    assert_eq!(c.items, 4, "{c:?}");
    assert_eq!(c.redundancy, [3, 1, 0]);
    assert_eq!(c.hashed_items, 4);
    let on = |d: i64| c.drives.iter().find(|x| x.drive_id == d).unwrap();
    assert_eq!((on(da).items, on(da).only_items, on(da).files), (3, 2, 4));
    assert_eq!((on(db_).items, on(db_).only_items), (2, 1));
    assert!((on(da).pct - 75.0).abs() < 0.01);
    assert_eq!(c.only_bytes, (2 + 3 + 2) * M as u64);
    assert!(coverage(cat.conn(), "nope").is_err());
    drop(cat);

    // Verification: full hashes turn likely duplicates into exact ones.
    let out = run(&b, true);
    assert_eq!(out.content.unwrap().verified, 2, "a.CR2 and d.bin on B");
    let out = run(&a, true);
    assert_eq!(out.content.unwrap().verified, 2, "a.CR2 and its copy on A");
    let cat = Catalog::open_readonly(&db).unwrap();
    let (g, _) = groups(cat.conn(), &q(Confidence::Exact, None, false)).unwrap();
    assert_eq!((g.len(), g[0].copies), (1, 4));
    assert_eq!(paths(&cat, "dup:exact").len(), 4);
    // A rescan does not read anything again.
    drop(cat);
    let out = run(&b, true);
    assert_eq!(out.content.unwrap().verified, 0);
}

#[test]
fn walk_of_a_vanished_source_is_not_merged() {
    use di_scan::{FsTarget, run_fs_scan};
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let data = tmp.path().join("data");
    photo_tree(&data);
    let first = scan(&db, &data);

    // Rescan, but the source is gone by the end of the walk.
    write(&data.join("Docs/new.txt"), 5);
    let target = FsTarget { drive_id: first.drive_id, filesystem_id: first.filesystem_id, root: data.canonicalize().unwrap() };
    let walk = WalkConfig { threads: 2, ..WalkConfig::default() };
    let mut cat = Catalog::open(&db).unwrap();
    let lost = run_fs_scan(
        &mut cat,
        &target,
        &walk,
        ResumeMode::Auto,
        &|| false,
        &Control::default(),
        &Progress::default(),
        &PhaseCell::default(),
    )
    .unwrap();
    drop(cat);
    assert_eq!(lost.status, ScanStatus::Lost);
    {
        let cat = Catalog::open_readonly(&db).unwrap();
        assert_eq!(paths(&cat, "ext:txt"), set(&["/Docs/notes.txt"]), "the earlier scan is untouched");
        assert_eq!(cat.scans(1).unwrap()[0].status, "interrupted");
    }

    // The next run resumes that scan and merges it.
    let again = scan(&db, &data);
    assert_eq!((again.status, again.scan_id), (ScanStatus::Complete, lost.scan_id));
    let cat = Catalog::open_readonly(&db).unwrap();
    assert_eq!(paths(&cat, "ext:txt"), set(&["/Docs/notes.txt", "/Docs/new.txt"]));
}

#[test]
fn walk_plan_follows_the_last_complete_scan() {
    use di_scan::device::{WalkPlan, walk_plan};
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let data = tmp.path().join("data");
    photo_tree(&data);
    let out = scan(&db, &data);
    let fs = out.filesystem_id;
    {
        let cat = Catalog::open_readonly(&db).unwrap();
        let now = di_core::now_secs();
        assert_eq!(walk_plan(&cat, fs, 0, now).unwrap(), WalkPlan::Walk, "0 always walks");
        assert!(matches!(walk_plan(&cat, fs, 3600, now).unwrap(), WalkPlan::Fresh { .. }));
        assert_eq!(walk_plan(&cat, fs, 3600, now + 7200).unwrap(), WalkPlan::Walk, "too old");
    }
    // An interrupted rescan is resumed, however recent the last complete one.
    let control = Control::default();
    control.cancel();
    let cut = run_dir_scan(&request(&db, &data), &control, &Progress::default(), &PhaseCell::default()).unwrap();
    assert_eq!(cut.status, ScanStatus::Interrupted);
    let cat = Catalog::open_readonly(&db).unwrap();
    assert_eq!(walk_plan(&cat, fs, 3600, di_core::now_secs()).unwrap(), WalkPlan::Walk);
}

#[test]
fn a_stalled_read_stops_the_content_phase() {
    use di_scan::content::{ContentConfig, DriveTrouble, has_pending, run_content};
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let data = tmp.path().join("data");
    // Duplicate candidates (same size, at least 1 MiB): each is read for a quick hash.
    for i in 0..20 {
        write(&data.join(format!("f{i:02}.bin")), 1 << 20);
    }
    let out = scan(&db, &data);
    let fs = out.filesystem_id;
    let cfg = ContentConfig {
        threads: 1,
        ffprobe: None,
        exiftool: None,
        stall_after: Duration::from_millis(300),
        ..Default::default()
    };
    let mut cat = Catalog::open(&db).unwrap();
    assert!(has_pending(cat.conn(), fs, &cfg).unwrap());

    // The first file in read order (by inode) now blocks the reader, like a drive that
    // stops answering: opening a FIFO waits for a writer.
    let first: String =
        cat.conn().query_row("SELECT name FROM files ORDER BY inode LIMIT 1", [], |r| r.get(0)).unwrap();
    let fifo = data.canonicalize().unwrap().join(&first);
    fs::remove_file(&fifo).unwrap();
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    // Let the read finish well after the stall was noticed.
    let unblock = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
                return;
            }
            assert!(Instant::now() < deadline, "the reader never opened the FIFO");
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let c = run_content(&mut cat, fs, &data.canonicalize().unwrap(), &cfg, &Control::default(), &Progress::default(), &mut |_| {}).unwrap();
    unblock.join().unwrap();
    assert!(matches!(&c.stopped, Some(DriveTrouble::Stalled { path, .. }) if path.ends_with(&first)), "{c:?}");
    assert_eq!(c.processed, 1, "no file is started after the stall: {c:?}");
    assert!(!c.cancelled);
    let content_pending: Option<i64> = cat
        .conn()
        .query_row("SELECT content_pending FROM scans WHERE id = ?1", [out.scan_id], |r| r.get(0))
        .unwrap();
    assert_eq!(content_pending, Some(1));

    // The rest is read next time.
    assert!(has_pending(cat.conn(), fs, &cfg).unwrap());
    let rest = run_content(&mut cat, fs, &data.canonicalize().unwrap(), &cfg, &Control::default(), &Progress::default(), &mut |_| {}).unwrap();
    assert_eq!((rest.processed, rest.stopped), (19, None));
    assert!(!has_pending(cat.conn(), fs, &cfg).unwrap());
}
