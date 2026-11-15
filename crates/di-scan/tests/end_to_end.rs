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
    let req = SearchRequest { query: "dup:yes", sort: SortKey::Path, descending: false, limit: 10, offset: 0 };
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
