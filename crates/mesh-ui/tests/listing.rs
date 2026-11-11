//! The listing and reading rules shared by every `FileSource` (the Android `SafSource` uses
//! them on what the document provider reports), and `OnRuntime`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mesh_admin::{
    AdminRequest, AdminResponse, DeployOpts, FileMeta, MAX_UPLOAD_FILE, MAX_UPLOAD_FILES,
};
use mesh_ui::{
    AdminTransport, FileSource, OnRuntime, UiError, UiErrorCode, check_listing, read_sized,
};
use tokio::sync::mpsc;

fn meta(path: &str, size: u64) -> FileMeta {
    FileMeta {
        path: path.into(),
        size,
    }
}

fn invalid(e: UiError, needle: &str) {
    assert_eq!(e.code, UiErrorCode::InvalidArgument, "{e:?}");
    assert!(e.message.contains(needle), "{}", e.message);
}

#[test]
fn check_listing_sorts_and_drops_os_files() {
    let files = vec![
        meta("site/b.html", 1),
        meta("site/.DS_Store", 1),
        meta("Thumbs.db", 1),
        meta("a/desktop.ini", 1),
        meta("a.css", 2),
    ];
    let paths: Vec<String> = check_listing(files)
        .unwrap()
        .into_iter()
        .map(|m| m.path)
        .collect();
    assert_eq!(paths, ["a.css", "site/b.html"]);
}

#[test]
fn check_listing_refuses_empty_and_os_files_only() {
    invalid(check_listing(vec![]).unwrap_err(), "The folder is empty");
    invalid(
        check_listing(vec![meta(".DS_Store", 3)]).unwrap_err(),
        "The folder is empty",
    );
}

#[test]
fn check_listing_enforces_the_upload_limits_from_the_sizes() {
    invalid(
        check_listing(vec![
            meta("ok", 1),
            meta("big/huge.bin", MAX_UPLOAD_FILE + 1),
        ])
        .unwrap_err(),
        "big/huge.bin",
    );
    let total: Vec<FileMeta> = (0..17)
        .map(|i| meta(&format!("f{i:02}"), MAX_UPLOAD_FILE))
        .collect();
    invalid(check_listing(total).unwrap_err(), "1 GiB");
    let many: Vec<FileMeta> = (0..=MAX_UPLOAD_FILES)
        .map(|i| meta(&format!("f{i}"), 0))
        .collect();
    invalid(check_listing(many).unwrap_err(), "files");
    // mesh-admin's path rules (a duplicate path here).
    assert!(check_listing(vec![meta("a", 1), meta("a", 1)]).is_err());
}

/// A provider that serves `data` and then `extra` more bytes (a file that grew).
fn reader(data: Vec<u8>, extra: usize) -> impl FnMut(u64, usize) -> Result<Vec<u8>, UiError> {
    let mut full = data;
    full.extend(std::iter::repeat_n(b'+', extra));
    move |offset, len| {
        let start = (offset as usize).min(full.len());
        let end = (start + len).min(full.len());
        Ok(full[start..end].to_vec())
    }
}

#[test]
fn read_sized_reads_exactly_the_listed_size_in_chunks() {
    let data: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
    let calls = AtomicUsize::new(0);
    let mut inner = reader(data.clone(), 0);
    let got = read_sized("a", 2500, 1000, |o, l| {
        calls.fetch_add(1, Ordering::SeqCst);
        assert!(l <= 1000);
        inner(o, l)
    })
    .unwrap();
    assert_eq!(got, data);
    // No more memory than the file: the buffer is allocated once at its listed size.
    assert_eq!(got.capacity(), 2500);
    // 3 chunks and one probe for growth.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(read_sized("e", 0, 1000, reader(vec![], 0)).unwrap(), b"");
}

#[test]
fn read_sized_refuses_a_file_that_changed() {
    invalid(
        read_sized("site/a.css", 10, 4, reader(vec![1; 7], 0)).unwrap_err(),
        "site/a.css",
    );
    invalid(
        read_sized("site/a.css", 10, 4, reader(vec![1; 10], 3)).unwrap_err(),
        "site/a.css",
    );
}

/// Records the name of the thread each call runs on.
struct Where(std::sync::Mutex<Vec<String>>);

impl Where {
    fn here(&self) {
        let name = std::thread::current().name().unwrap_or("").to_owned();
        self.0.lock().unwrap().push(name);
    }
}

#[async_trait]
impl AdminTransport for Where {
    async fn request(&self, _req: AdminRequest) -> Result<AdminResponse, UiError> {
        self.here();
        Ok(AdminResponse::Ok)
    }

    async fn stream(&self, _req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        self.here();
        tx.send("line".into()).await.ok();
        Ok(())
    }

    async fn deploy_static(
        &self,
        _name: String,
        _opts: DeployOpts,
        _src: Box<dyn FileSource>,
        _progress: mpsc::Sender<u64>,
    ) -> Result<AdminResponse, UiError> {
        self.here();
        Ok(AdminResponse::Ok)
    }
}

struct Empty;

impl FileSource for Empty {
    fn list(&mut self) -> Result<Vec<FileMeta>, UiError> {
        Ok(vec![])
    }
    fn read(&mut self, _path: &str) -> Result<Vec<u8>, UiError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn on_runtime_runs_every_call_on_that_runtime() {
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("the-host")
        .enable_all()
        .build()
        .unwrap();
    let inner = Arc::new(Where(Default::default()));
    let t = OnRuntime::new(host.handle().clone(), inner.clone());
    t.request(AdminRequest::Status).await.unwrap();
    let (tx, mut rx) = mpsc::channel(4);
    t.stream(AdminRequest::Status, tx).await.unwrap();
    assert_eq!(rx.recv().await.as_deref(), Some("line"));
    let (ptx, _prx) = mpsc::channel(4);
    t.deploy_static("s".into(), DeployOpts::default(), Box::new(Empty), ptx)
        .await
        .unwrap();
    assert_eq!(*inner.0.lock().unwrap(), ["the-host"; 3]);
    host.shutdown_background();
}
