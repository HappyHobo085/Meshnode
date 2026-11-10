//! `mesh-admin` over a real socket / named pipe with a fake backend.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use mesh_admin::{
    Access, AdminBackend, AdminClient, AdminError, AdminRequest, AdminResponse, AdminServer,
    AdminStream, DaemonState, DeployOpts, ErrorCode, FileData, FileMeta, Hello, LocalStateView,
    MAX_CONNECTIONS, MAX_UPLOAD_FILE, MAX_UPLOAD_FILES, MemberInfoView, PROTOCOL_VERSION,
    RequestIo, ServerTimeouts, ServiceInfoView, ServiceKindView, StatusInfo, connect_raw,
    endpoint_for,
};
use mesh_core::frame::{read_frame, write_frame};
use mesh_proto::{Limits, NodeId, Placement, RefuseReason, Role, ServiceKind, ServiceManifest};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// An upload the fake received: service name and files in order.
type Upload = (String, Vec<(FileMeta, Vec<u8>)>);

struct Fake {
    state: Mutex<DaemonState>,
    uploads: Mutex<Vec<Upload>>,
    /// `DeployStatic` named `saved` is written here with `save_files`.
    save_dir: PathBuf,
    push_log: Notify,
    follow_ended: AtomicBool,
}

impl Fake {
    fn new(save_dir: &Path) -> Arc<Fake> {
        Arc::new(Fake {
            state: Mutex::new(DaemonState::Running),
            uploads: Mutex::new(Vec::new()),
            save_dir: save_dir.to_path_buf(),
            push_log: Notify::new(),
            follow_ended: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl AdminBackend for Fake {
    fn state(&self) -> DaemonState {
        *self.state.lock().unwrap()
    }

    async fn handle(&self, req: AdminRequest, io: &mut RequestIo<'_>) -> AdminResponse {
        match req {
            AdminRequest::DeployStatic { name, .. } if name == "saved" => {
                match io.save_files(&self.save_dir).await {
                    Ok(()) => AdminResponse::Ok,
                    Err(e) => AdminResponse::error(ErrorCode::Internal, e.to_string()),
                }
            }
            AdminRequest::DeployStatic { name, .. } => {
                let mut got = Vec::new();
                loop {
                    match io.next_file().await {
                        Ok(Some(f)) => got.push(f),
                        Ok(None) => break,
                        Err(e) => return AdminResponse::error(ErrorCode::Internal, e.to_string()),
                    }
                }
                self.uploads.lock().unwrap().push((name, got));
                AdminResponse::Ok
            }
            AdminRequest::LogsTail { lines, follow } => {
                let first = (0..lines).map(|i| format!("line {i}")).collect();
                if io.send_lines(first).await.is_err() {
                    return AdminResponse::Ok;
                }
                if follow {
                    let mut n = 0;
                    loop {
                        tokio::select! {
                            _ = self.push_log.notified() => {
                                n += 1;
                                if io.send_lines(vec![format!("new {n}")]).await.is_err() {
                                    break;
                                }
                            }
                            _ = io.client_gone() => break,
                        }
                    }
                    self.follow_ended.store(true, Ordering::SeqCst);
                }
                AdminResponse::Ok
            }
            other => expected_response(&other),
        }
    }
}

fn node(b: u8) -> NodeId {
    NodeId([b; 32])
}

fn manifest(
    name: &str,
    placement: Placement,
    limits: Limits,
    outbound: Vec<String>,
) -> ServiceManifest {
    ServiceManifest {
        name: name.into(),
        kind: ServiceKind::Static { root: [3; 32] },
        version: 2,
        limits,
        placement,
        outbound,
    }
}

/// A distinct typed answer per request, covering every response variant.
fn expected_response(req: &AdminRequest) -> AdminResponse {
    use AdminRequest as R;
    match req {
        R::Status => AdminResponse::Status(StatusInfo {
            state: DaemonState::Running,
            daemon_version: "0.1.0".into(),
            data_dir: "/var/lib/meshnode".into(),
            node_id: Some(node(1)),
            mesh_id: None,
            name: Some("pc".into()),
            role: Some(Role::Host),
            tags: vec!["desktop".into()],
            members_total: 3,
            members_online: 2,
            services: 1,
            gateway: Some("127.0.0.1:7777".into()),
        }),
        R::Members => AdminResponse::Members(vec![MemberInfoView {
            id: node(2),
            name: "phone".into(),
            role: Role::Client,
            platform: "android".into(),
            tags: vec![],
            is_self: false,
            effective: true,
            online: true,
            rtt_ms: Some(12),
            last_seen_ms: Some(1000),
            lag: 0,
        }]),
        R::Services => AdminResponse::Services(vec![ServiceInfoView {
            name: "site".into(),
            kind: ServiceKindView::Static,
            version: 2,
            placement: Placement::AllHosts,
            placed_hosts: vec![node(1)],
            local: LocalStateView::Failed {
                error: "boom".into(),
            },
            limits: Limits::default(),
            outbound: vec![],
            data_bytes: 42,
        }]),
        R::Invite { role, ttl_secs } => AdminResponse::Ticket {
            text: format!("ticket {role:?} {ttl_secs}"),
        },
        R::SetPlacement { name, placement } => {
            AdminResponse::Manifest(manifest(name, placement.clone(), Limits::default(), vec![]))
        }
        R::SetOutbound { name, outbound } => AdminResponse::Manifest(manifest(
            name,
            Placement::AllHosts,
            Limits::default(),
            outbound.clone(),
        )),
        R::SetLimits { name, limits } => {
            AdminResponse::Manifest(manifest(name, Placement::AllHosts, *limits, vec![]))
        }
        R::DeployWasm { name, wasm, opts } => AdminResponse::Manifest(manifest(
            name,
            opts.placement.clone(),
            opts.limits,
            vec![format!("len:{}", wasm.len())],
        )),
        R::ConfigGet { key } => AdminResponse::Config(vec![(
            key.clone().unwrap_or_else(|| "all".into()),
            "value".into(),
        )]),
        R::ConfigSet { key, .. } => AdminResponse::ConfigSet {
            restart_required: key.starts_with("gateway."),
        },
        R::Join { .. } => AdminResponse::error(
            ErrorCode::JoinRefused(RefuseReason::Expired),
            "the invite has expired",
        ),
        R::Remove { .. } => AdminResponse::error(ErrorCode::LastHost, "last host"),
        other => AdminResponse::Error {
            code: ErrorCode::NotFound,
            message: format!("{other:?}"),
        },
    }
}

fn every_request() -> Vec<AdminRequest> {
    let opts = DeployOpts {
        limits: Limits {
            memory_mb: 32,
            request_timeout_ms: 500,
            max_body_bytes: 1024,
        },
        placement: Placement::Nodes(vec![node(4)]),
        outbound: vec!["example.com:443".into()],
    };
    vec![
        AdminRequest::Status,
        AdminRequest::Members,
        AdminRequest::Services,
        AdminRequest::Init { name: "pc".into() },
        AdminRequest::Join {
            ticket: "mesh1abc".into(),
            name: "pc".into(),
        },
        AdminRequest::ResetIdentity { confirm: true },
        AdminRequest::Invite {
            role: Role::Client,
            ttl_secs: 900,
        },
        AdminRequest::Remove { id: node(9) },
        AdminRequest::SetInfo {
            name: Some("n".into()),
            tags: Some(vec!["a".into()]),
        },
        AdminRequest::SetTagsOf {
            id: node(5),
            tags: vec!["b".into()],
        },
        AdminRequest::DeployWasm {
            name: "counter".into(),
            wasm: vec![0, 97, 115, 109],
            opts,
        },
        AdminRequest::Undeploy { name: "x".into() },
        AdminRequest::Purge { name: "y".into() },
        AdminRequest::SetPlacement {
            name: "z".into(),
            placement: Placement::Tags(vec!["t".into()]),
        },
        AdminRequest::SetOutbound {
            name: "z".into(),
            outbound: vec!["h:1".into()],
        },
        AdminRequest::SetLimits {
            name: "z".into(),
            limits: Limits::default(),
        },
        AdminRequest::ConfigGet { key: None },
        AdminRequest::ConfigGet {
            key: Some("gateway.listen".into()),
        },
        AdminRequest::ConfigSet {
            key: "gateway.listen".into(),
            value: "127.0.0.1:7778".into(),
        },
    ]
}

struct Daemon {
    dir: tempfile::TempDir,
    fake: Arc<Fake>,
    server: Option<AdminServer>,
}

impl Daemon {
    fn endpoint(&self) -> mesh_admin::Endpoint {
        endpoint_for(self.dir.path())
    }

    async fn client(&self) -> AdminClient {
        timeout(WAIT, AdminClient::connect(&self.endpoint()))
            .await
            .expect("connect timed out")
            .expect("connect")
    }

    async fn stop(mut self) {
        self.server.take().unwrap().shutdown().await;
    }
}

async fn daemon() -> Daemon {
    daemon_with(ServerTimeouts::default()).await
}

async fn daemon_with(timeouts: ServerTimeouts) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let save = dir.path().join("saved");
    let fake = Fake::new(&save);
    let server = AdminServer::bind_with_timeouts(
        &endpoint_for(dir.path()),
        Access::CurrentUserOnly,
        fake.clone(),
        timeouts,
    )
    .expect("bind");
    Daemon {
        dir,
        fake,
        server: Some(server),
    }
}

/// A raw connection that has done the `Hello` exchange.
async fn raw_hello(d: &Daemon) -> AdminStream {
    let mut s = connect_raw(&d.endpoint()).await.expect("connect_raw");
    write_frame(
        &mut s,
        &Hello {
            version: PROTOCOL_VERSION,
            state: None,
        },
    )
    .await
    .unwrap();
    let h: Hello = read_frame(&mut s, 1024).await.unwrap().unwrap();
    assert_eq!(h.version, PROTOCOL_VERSION);
    s
}

/// The next frame, `None` once the daemon closed the connection (EOF or reset).
async fn next_response(s: &mut AdminStream) -> Option<AdminResponse> {
    timeout(
        WAIT,
        read_frame::<_, AdminResponse>(s, mesh_admin::MAX_FRAME),
    )
    .await
    .expect("read timed out")
    .ok()
    .flatten()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_version_mismatch_closes() {
    let d = daemon().await;
    let mut s = connect_raw(&d.endpoint()).await.unwrap();
    write_frame(
        &mut s,
        &Hello {
            version: 2,
            state: None,
        },
    )
    .await
    .unwrap();
    let h: Hello = read_frame(&mut s, 1024).await.unwrap().unwrap();
    assert_eq!(
        h,
        Hello {
            version: 1,
            state: Some(DaemonState::Running)
        }
    );
    // The daemon closed: nothing more arrives, and a request goes unanswered.
    let _ = write_frame(&mut s, &AdminRequest::Status).await;
    assert_eq!(next_response(&mut s).await, None);
    let mismatch = AdminError::VersionMismatch { cli: 1, daemon: 2 };
    assert_eq!(
        mismatch.to_string(),
        "meshd and the running daemon are different versions (cli 1, daemon 2)"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_round_trip_every_variant() {
    let d = daemon().await;
    let mut c = d.client().await;
    assert_eq!(c.daemon_state(), DaemonState::Running);
    for req in every_request() {
        // Init, Join and ResetIdentity are only accepted by an uninitialised daemon.
        let state = match req {
            AdminRequest::Init { .. }
            | AdminRequest::Join { .. }
            | AdminRequest::ResetIdentity { .. } => DaemonState::Uninitialised,
            _ => DaemonState::Running,
        };
        *d.fake.state.lock().unwrap() = state;
        let want = expected_response(&req);
        let got = c.request(req.clone()).await.expect("request");
        assert_eq!(got, want, "for {req:?}");
    }
    // Streamed requests have their own methods.
    let r = c
        .request(AdminRequest::LogsTail {
            lines: 1,
            follow: false,
        })
        .await;
    assert!(matches!(r, Err(AdminError::Protocol(_))), "{r:?}");
    let r = c
        .request(AdminRequest::DeployStatic {
            name: "s".into(),
            opts: DeployOpts::default(),
            files: vec![],
        })
        .await;
    assert!(matches!(r, Err(AdminError::Protocol(_))), "{r:?}");
    // The connection is still usable.
    assert_eq!(
        c.request(AdminRequest::Members).await.unwrap(),
        expected_response(&AdminRequest::Members)
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uninitialised_accepts_only_init_join() {
    let d = daemon().await;
    *d.fake.state.lock().unwrap() = DaemonState::Uninitialised;
    let mut c = d.client().await;
    assert_eq!(c.daemon_state(), DaemonState::Uninitialised);
    for req in [
        AdminRequest::Members,
        AdminRequest::Services,
        AdminRequest::Invite {
            role: Role::Host,
            ttl_secs: 60,
        },
        AdminRequest::Undeploy { name: "x".into() },
    ] {
        let got = c.request(req.clone()).await.unwrap();
        assert!(
            matches!(
                got,
                AdminResponse::Error {
                    code: ErrorCode::WrongState,
                    ..
                }
            ),
            "{req:?} -> {got:?}"
        );
    }
    for req in [
        AdminRequest::Status,
        AdminRequest::Init { name: "pc".into() },
        AdminRequest::Join {
            ticket: "t".into(),
            name: "pc".into(),
        },
        AdminRequest::ConfigGet { key: None },
    ] {
        assert_eq!(
            c.request(req.clone()).await.unwrap(),
            expected_response(&req)
        );
    }
    // Init and Join only while uninitialised.
    *d.fake.state.lock().unwrap() = DaemonState::Running;
    for req in [
        AdminRequest::Init { name: "pc".into() },
        AdminRequest::Join {
            ticket: "t".into(),
            name: "pc".into(),
        },
    ] {
        let got = c.request(req).await.unwrap();
        assert!(
            matches!(
                got,
                AdminResponse::Error {
                    code: ErrorCode::WrongState,
                    ..
                }
            ),
            "{got:?}"
        );
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reset_identity_only_when_removed() {
    let d = daemon().await;
    let mut c = d.client().await;
    let reset = AdminRequest::ResetIdentity { confirm: true };
    for (state, allowed) in [
        (DaemonState::Running, false),
        (DaemonState::Rebootstrapping, false),
        (DaemonState::Removed, true),
        (DaemonState::Uninitialised, true),
    ] {
        *d.fake.state.lock().unwrap() = state;
        let got = c.request(reset.clone()).await.unwrap();
        if allowed {
            assert_eq!(got, expected_response(&reset), "{state:?}");
        } else {
            assert!(
                matches!(
                    got,
                    AdminResponse::Error {
                        code: ErrorCode::WrongState,
                        ..
                    }
                ),
                "{state:?}: {got:?}"
            );
        }
    }
    *d.fake.state.lock().unwrap() = DaemonState::Removed;
    let got = c
        .request(AdminRequest::ResetIdentity { confirm: false })
        .await
        .unwrap();
    assert!(
        matches!(
            got,
            AdminResponse::Error {
                code: ErrorCode::InvalidArgument,
                ..
            }
        ),
        "{got:?}"
    );
    d.stop().await;
}

fn write_tree(root: &Path, files: &[(&str, Vec<u8>)]) -> Vec<(FileMeta, PathBuf)> {
    files
        .iter()
        .map(|(rel, bytes)| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
            (
                FileMeta {
                    path: (*rel).into(),
                    size: bytes.len() as u64,
                },
                p,
            )
        })
        .collect()
}

/// Sends a `DeployStatic` on a raw connection and expects `InvalidArgument`, then the
/// connection closed.
async fn expect_rejected(d: &Daemon, files: Vec<FileMeta>, data: Vec<Vec<u8>>) {
    let mut s = raw_hello(d).await;
    write_frame(
        &mut s,
        &AdminRequest::DeployStatic {
            name: "site".into(),
            opts: DeployOpts::default(),
            files: files.clone(),
        },
    )
    .await
    .unwrap();
    for bytes in data {
        if write_frame(&mut s, &FileData { bytes }).await.is_err() {
            break;
        }
    }
    match next_response(&mut s).await {
        Some(AdminResponse::Error {
            code: ErrorCode::InvalidArgument,
            ..
        }) => {}
        other => panic!("{files:?}: {other:?}"),
    }
    assert_eq!(next_response(&mut s).await, None, "{files:?}: still open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_static_upload() {
    let d = daemon().await;
    let src = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let files = write_tree(
        src.path(),
        &[
            ("index.html", b"<h1>hi</h1>".to_vec()),
            ("assets/app.bin", big.clone()),
            ("empty", Vec::new()),
        ],
    );
    let mut c = d.client().await;
    let r = c
        .deploy_static("site".into(), DeployOpts::default(), files.clone())
        .await
        .unwrap();
    assert_eq!(r, AdminResponse::Ok);
    {
        let uploads = d.fake.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        let (name, got) = &uploads[0];
        assert_eq!(name, "site");
        let want: Vec<_> = files
            .iter()
            .map(|(m, p)| (m.clone(), std::fs::read(p).unwrap()))
            .collect();
        assert_eq!(got, &want);
    }
    // `save_files` writes the tree under the backend's directory.
    let r = c
        .deploy_static("saved".into(), DeployOpts::default(), files.clone())
        .await
        .unwrap();
    assert_eq!(r, AdminResponse::Ok);
    let save = d.dir.path().join("saved");
    assert_eq!(
        std::fs::read(save.join("index.html")).unwrap(),
        b"<h1>hi</h1>"
    );
    assert_eq!(
        std::fs::read(save.join("assets").join("app.bin")).unwrap(),
        big
    );
    assert_eq!(std::fs::read(save.join("empty")).unwrap(), b"");
    // The connection stays usable after an upload.
    assert!(matches!(
        c.request(AdminRequest::Status).await.unwrap(),
        AdminResponse::Status(_)
    ));

    // The client refuses invalid paths before sending.
    let mut bad = files.clone();
    bad[0].0.path = "../x".into();
    let r = c
        .deploy_static("site".into(), DeployOpts::default(), bad)
        .await;
    assert!(matches!(r, Err(AdminError::InvalidUpload(_))), "{r:?}");

    // The daemon refuses them itself, whatever the client.
    let meta = |p: &str, size| FileMeta {
        path: p.into(),
        size,
    };
    expect_rejected(&d, vec![meta("../x", 1)], vec![vec![1]]).await;
    expect_rejected(&d, vec![meta("/etc/x", 1)], vec![vec![1]]).await;
    expect_rejected(&d, vec![meta("C:/x", 1)], vec![vec![1]]).await;
    expect_rejected(&d, vec![meta("a\\b", 1)], vec![vec![1]]).await;
    // Declared 10 bytes, sent 5.
    expect_rejected(&d, vec![meta("a", 10)], vec![vec![0; 5]]).await;
    // Declared 5, sent 10.
    expect_rejected(
        &d,
        vec![meta("a", 5), meta("b", 1)],
        vec![vec![0; 10], vec![1]],
    )
    .await;
    // A file over 64 MiB, and a total of 1 GiB + 1 (declared only; nothing is sent).
    expect_rejected(&d, vec![meta("a", MAX_UPLOAD_FILE + 1)], vec![]).await;
    let mut total: Vec<_> = (0..16)
        .map(|i| meta(&format!("f{i}"), MAX_UPLOAD_FILE))
        .collect();
    total.push(meta("one-more", 1));
    expect_rejected(&d, total, vec![]).await;
    // More than 100 000 files (declared only).
    let many: Vec<_> = (0..=MAX_UPLOAD_FILES)
        .map(|i| meta(&format!("f{i}"), 0))
        .collect();
    expect_rejected(&d, many, vec![]).await;
    // A colon in any segment.
    expect_rejected(&d, vec![meta("a/b:c", 1)], vec![vec![1]]).await;
    assert_eq!(
        d.fake.uploads.lock().unwrap().len(),
        1,
        "no rejected upload reached the backend"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploy_static_refused_by_state_reports_daemon_error() {
    let d = daemon().await;
    *d.fake.state.lock().unwrap() = DaemonState::Uninitialised;
    let src = tempfile::tempdir().unwrap();
    // More than any pipe or socket buffer, so the daemon closes while the client writes.
    let files = write_tree(
        src.path(),
        &[("a", vec![1; 3 << 20]), ("b", vec![2; 3 << 20])],
    );
    let mut c = d.client().await;
    let r = c
        .deploy_static("site".into(), DeployOpts::default(), files)
        .await
        .unwrap();
    assert!(
        matches!(
            r,
            AdminResponse::Error {
                code: ErrorCode::WrongState,
                ..
            }
        ),
        "{r:?}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_tail_follow_streams() {
    let d = daemon().await;
    let mut c = d.client().await;
    {
        let s = c.logs_tail(3, false);
        let all: Vec<_> = s.collect().await;
        let all: Vec<Vec<String>> = all.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(all, vec![vec!["line 0", "line 1", "line 2"]]);
    }
    // The connection is usable after a finished tail.
    assert!(matches!(
        c.request(AdminRequest::Status).await.unwrap(),
        AdminResponse::Status(_)
    ));

    let mut s = Box::pin(c.logs_tail(2, true));
    let first = timeout(WAIT, s.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(first, vec!["line 0", "line 1"]);
    for n in 1..=2 {
        d.fake.push_log.notify_one();
        let next = timeout(WAIT, s.next()).await.unwrap().unwrap().unwrap();
        assert_eq!(next, vec![format!("new {n}")]);
    }
    drop(s);
    drop(c);
    // With no new lines to write, the daemon still notices the client left.
    timeout(WAIT, async {
        while !d.fake.follow_ended.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("follow did not end after the client left");

    let mut c = d.client().await;
    let r = c
        .request(AdminRequest::LogsTail {
            lines: 10,
            follow: false,
        })
        .await;
    assert!(r.is_err());
    // Over 10 000 lines is refused.
    let s = c.logs_tail(10_001, false);
    let all: Vec<_> = s.collect().await;
    assert!(
        matches!(
            all.as_slice(),
            [Err(AdminError::Daemon {
                code: ErrorCode::InvalidArgument,
                ..
            })]
        ),
        "{all:?}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_frame_closes_connection_only() {
    let d = daemon().await;
    let mut good = d.client().await;

    // Undecodable bytes in a well-formed frame.
    let mut a = raw_hello(&d).await;
    a.write_all(&[0, 0, 0, 3, 1, 0xff, 0xff]).await.unwrap();
    a.flush().await.unwrap();
    assert_eq!(next_response(&mut a).await, None);

    // A length prefix over the limit.
    let mut b = raw_hello(&d).await;
    b.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    b.flush().await.unwrap();
    assert_eq!(next_response(&mut b).await, None);

    // Garbage instead of a Hello.
    let mut h = connect_raw(&d.endpoint()).await.unwrap();
    h.write_all(&[0, 0, 0, 2, 9, 9]).await.unwrap();
    h.flush().await.unwrap();
    assert!(
        timeout(WAIT, read_frame::<_, Hello>(&mut h, 1024))
            .await
            .unwrap()
            .ok()
            .flatten()
            .is_none()
    );

    // Everyone else is unaffected.
    assert!(matches!(
        good.request(AdminRequest::Status).await.unwrap(),
        AdminResponse::Status(_)
    ));
    let mut fresh = d.client().await;
    assert!(matches!(
        fresh.request(AdminRequest::Members).await.unwrap(),
        AdminResponse::Members(_)
    ));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoint_in_use_refused() {
    let d = daemon().await;
    let other = Fake::new(d.dir.path());
    let r = AdminServer::bind(&d.endpoint(), Access::CurrentUserOnly, other.clone());
    assert!(matches!(r, Err(AdminError::InUse(_))), "{:?}", r.err());
    // The first daemon still serves.
    let mut c = d.client().await;
    assert!(c.request(AdminRequest::Status).await.is_ok());
    drop(c);
    let ep = d.endpoint();
    let dir = d.dir.path().to_path_buf();
    let Daemon {
        dir: keep, server, ..
    } = d;
    server.unwrap().shutdown().await;
    // Once it stopped, the endpoint is free again. On Windows a pipe handle with pending
    // overlapped I/O is closed by the runtime when that I/O completes, a moment after
    // `shutdown` returns, so the rebind is retried briefly.
    let again = timeout(WAIT, async {
        loop {
            match AdminServer::bind(&ep, Access::CurrentUserOnly, other.clone()) {
                Err(AdminError::InUse(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
                r => break r,
            }
        }
    })
    .await
    .expect("endpoint still in use after shutdown")
    .expect("rebind");
    let mut c = AdminClient::connect(&ep).await.unwrap();
    assert!(c.request(AdminRequest::Status).await.is_ok());
    drop(c);
    again.shutdown().await;
    assert!(dir.exists());
    drop(keep);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_socket_file_is_replaced_and_mode_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ep = endpoint_for(dir.path());
    let mesh_admin::Endpoint::Socket(path) = &ep;
    drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    assert!(path.exists(), "stale socket file left behind");
    let server = AdminServer::bind(&ep, Access::CurrentUserOnly, Fake::new(dir.path())).unwrap();
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let mut c = AdminClient::connect(&ep).await.unwrap();
    assert!(c.request(AdminRequest::Status).await.is_ok());
    drop(c);
    server.shutdown().await;
    assert!(!path.exists(), "socket file removed on shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_running_reported() {
    let dir = tempfile::tempdir().unwrap();
    let r = AdminClient::connect(&endpoint_for(dir.path())).await;
    assert!(matches!(r, Err(AdminError::NotRunning)), "{:?}", r.err());
    assert_eq!(
        AdminError::NotRunning.to_string(),
        "meshd is not running (start it with \"meshd run\" or \"meshd service install\")"
    );
    // After shutdown, too.
    let d = daemon().await;
    let ep = d.endpoint();
    d.stop().await;
    // A pipe instance may outlive `shutdown` by a moment (see `endpoint_in_use_refused`).
    timeout(WAIT, async {
        while !matches!(AdminClient::connect(&ep).await, Err(AdminError::NotRunning)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("still reachable after shutdown");
}

#[test]
fn error_codes_render() {
    let codes = [
        ErrorCode::NotRunning,
        ErrorCode::WrongState,
        ErrorCode::InvalidArgument,
        ErrorCode::NotFound,
        ErrorCode::JoinRefused(RefuseReason::Expired),
        ErrorCode::JoinRefused(RefuseReason::Used),
        ErrorCode::JoinRefused(RefuseReason::Unknown),
        ErrorCode::JoinRefused(RefuseReason::WrongMesh),
        ErrorCode::JoinRefused(RefuseReason::Removed),
        ErrorCode::JoinRefused(RefuseReason::InviterUnavailable),
        ErrorCode::LastHost,
        ErrorCode::Busy,
        ErrorCode::Internal,
    ];
    let texts: Vec<String> = codes.iter().map(|c| c.to_string()).collect();
    for (c, t) in codes.iter().zip(&texts) {
        assert!(t.len() > 5, "{c:?} renders {t:?}");
        assert!(
            !t.contains("JoinRefused") && !t.contains('{'),
            "{c:?} renders {t:?}"
        );
    }
    let mut distinct = texts.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), texts.len(), "{texts:?}");
    assert_eq!(
        ErrorCode::JoinRefused(RefuseReason::Expired).to_string(),
        "join refused: the invite has expired"
    );
    let e = AdminError::Daemon {
        code: ErrorCode::NotFound,
        message: "no service x".into(),
    };
    assert_eq!(e.to_string(), "not found: no service x");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seventeenth_connection_waits_or_refused() {
    let d = daemon().await;
    let mut clients = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        clients.push(d.client().await);
    }
    let ep = d.endpoint();
    let mut waiting = tokio::spawn(async move { AdminClient::connect(&ep).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !waiting.is_finished(),
        "the 17th connection was served at once"
    );
    assert!(
        timeout(Duration::from_millis(100), &mut waiting)
            .await
            .is_err(),
        "the 17th connection was answered"
    );
    // One leaves: the waiting one is served.
    drop(clients.pop());
    let mut c = timeout(WAIT, waiting)
        .await
        .expect("still waiting after a slot freed")
        .unwrap()
        .expect("connect");
    assert!(c.request(AdminRequest::Status).await.is_ok());
    for c in clients.iter_mut() {
        assert!(c.request(AdminRequest::Status).await.is_ok());
    }
    d.stop().await;
}

#[test]
fn endpoint_for_is_per_data_dir() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    assert_ne!(endpoint_for(a.path()), endpoint_for(b.path()));
    assert_eq!(endpoint_for(a.path()), endpoint_for(a.path()));
    #[cfg(unix)]
    assert_eq!(
        endpoint_for(a.path()),
        mesh_admin::Endpoint::Socket(a.path().join("admin.sock"))
    );
    #[cfg(windows)]
    {
        let mesh_admin::Endpoint::Pipe(name) = endpoint_for(a.path());
        let suffix = name.strip_prefix(r"\\.\pipe\meshnode-admin-").expect(&name);
        assert_eq!(suffix.len(), 8);
        assert!(
            suffix
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            endpoint_for(&mesh_admin::default_data_dir()),
            mesh_admin::Endpoint::Pipe(r"\\.\pipe\meshnode-admin".into())
        );
    }
}

const SHORT: Duration = Duration::from_millis(300);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_connection_is_closed() {
    let d = daemon_with(ServerTimeouts {
        idle: SHORT,
        upload_frame: Duration::from_secs(30),
    })
    .await;
    let mut busy = d.client().await;
    let mut idle = d.client().await;
    // Requests within the idle time keep a connection open.
    for _ in 0..4 {
        tokio::time::sleep(SHORT / 3).await;
        assert!(busy.request(AdminRequest::Status).await.is_ok());
    }
    tokio::time::sleep(SHORT * 3).await;
    assert!(idle.request(AdminRequest::Status).await.is_err());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_upload_is_closed() {
    let d = daemon_with(ServerTimeouts {
        idle: Duration::from_secs(30),
        upload_frame: SHORT,
    })
    .await;
    let mut s = raw_hello(&d).await;
    let files = vec![FileMeta {
        path: "a".into(),
        size: 10,
    }];
    write_frame(
        &mut s,
        &AdminRequest::DeployStatic {
            name: "site".into(),
            opts: DeployOpts::default(),
            files,
        },
    )
    .await
    .unwrap();
    // No FileData follows: the daemon gives up and closes without an answer.
    let started = std::time::Instant::now();
    assert_eq!(next_response(&mut s).await, None);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_stream_is_not_idle() {
    let d = daemon_with(ServerTimeouts {
        idle: SHORT,
        upload_frame: SHORT,
    })
    .await;
    let mut c = d.client().await;
    let mut s = Box::pin(c.logs_tail(1, true));
    let first = timeout(WAIT, s.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(first, vec!["line 0"]);
    tokio::time::sleep(SHORT * 3).await;
    d.fake.push_log.notify_one();
    let next = timeout(WAIT, s.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(next, vec!["new 1"]);
    drop(s);
    drop(c);
    d.stop().await;
}
