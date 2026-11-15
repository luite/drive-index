//! Runs searches off the UI thread. A newer request interrupts the running query
//! (SQLite interrupt) and only the latest request's result is delivered.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use di_core::Catalog;
use di_core::query::QueryError;
use di_core::search::{Hit, SearchError, SearchRequest, SortKey, Summary, search, summarize};

pub struct Request {
    pub generation: u64,
    pub query: String,
    pub sort: SortKey,
    pub descending: bool,
    pub limit: usize,
}

pub enum Outcome {
    Hits { hits: Vec<Hit>, summary: Summary },
    QueryError(QueryError),
    Failed(String),
}

pub struct Response {
    pub generation: u64,
    pub outcome: Outcome,
}

pub struct SearchWorker {
    tx: Sender<Request>,
    pub results: Receiver<Response>,
    interrupt: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
}

impl SearchWorker {
    pub fn start(catalog: PathBuf) -> SearchWorker {
        let (tx, rx) = mpsc::channel::<Request>();
        let (res_tx, results) = mpsc::channel();
        let interrupt = Arc::new(Mutex::new(None));
        let ih = interrupt.clone();
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let mut cat: Option<Catalog> = None;
                while let Ok(mut req) = rx.recv() {
                    // Only the newest request matters.
                    while let Ok(newer) = rx.try_recv() {
                        req = newer;
                    }
                    if cat.is_none() {
                        cat = Catalog::open_readonly(&catalog).ok();
                        if let Some(c) = &cat {
                            *ih.lock().unwrap() = Some(c.conn().get_interrupt_handle());
                        }
                    }
                    let Some(c) = &cat else {
                        let _ = res_tx.send(Response {
                            generation: req.generation,
                            outcome: Outcome::Failed("the catalog is empty: scan a drive first".into()),
                        });
                        continue;
                    };
                    let sreq = SearchRequest {
                        query: &req.query,
                        sort: req.sort,
                        descending: req.descending,
                        limit: req.limit,
                        offset: 0,
                    };
                    let outcome = match search(c.conn(), &sreq).and_then(|h| summarize(c.conn(), &req.query).map(|s| (h, s))) {
                        Ok((hits, summary)) => Outcome::Hits { hits, summary },
                        Err(SearchError::Query(q)) => Outcome::QueryError(q),
                        Err(SearchError::Db(e)) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) => {
                            continue
                        }
                        Err(SearchError::Db(e)) => Outcome::Failed(e.to_string()),
                    };
                    if res_tx.send(Response { generation: req.generation, outcome }).is_err() {
                        return;
                    }
                }
            })
            .expect("spawning search thread");
        SearchWorker { tx, results, interrupt }
    }

    pub fn submit(&self, req: Request) {
        if let Some(h) = self.interrupt.lock().unwrap().as_ref() {
            h.interrupt();
        }
        let _ = self.tx.send(req);
    }
}
