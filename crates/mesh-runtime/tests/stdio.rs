mod common;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use common::fixture;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::{MemKv, RequestCtx, Runtime, RuntimeConfig};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for Capture {
    type Writer = Capture;
    fn make_writer(&self) -> Capture {
        self.clone()
    }
}

/// The guest runs on another thread than the test, so the subscriber is global. Tests tell
/// their lines apart by content.
fn log() -> &'static Capture {
    static LOG: OnceLock<Capture> = OnceLock::new();
    LOG.get_or_init(|| {
        let capture = Capture::default();
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .init();
        capture
    })
}

/// Runs `echo` on `uri`; returns everything logged so far.
async fn run_echo(uri: &str) -> String {
    let capture = log();
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let svc = rt.load([1; 32], &fixture("echo")).await.unwrap();
    let ctx = RequestCtx {
        service: "echo".to_string(),
        node_id: NodeId([7; 32]),
        kv: Arc::new(MemKv::default()),
        limits: Limits::default(),
        outbound: Vec::new(),
    };
    let req = http::Request::get(uri).body(Bytes::new()).unwrap();
    assert_eq!(rt.handle(&svc, req, ctx).await.status(), 200);
    let log = capture.0.lock().unwrap().clone();
    String::from_utf8(log).unwrap()
}

fn line<'a>(log: &'a str, needle: &str) -> &'a str {
    log.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no log line with {needle:?} in:\n{log}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn stdout_routed_to_log() {
    let log = run_echo("http://echo.mesh.localhost/hello").await;
    let out = line(&log, "echo: GET");
    assert!(out.contains("INFO"), "{out}");
    assert!(out.contains("service=echo"), "{out}");
    let err = line(&log, "echo: stderr line");
    assert!(err.contains("WARN"), "{err}");
    assert!(err.contains("service=echo"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn long_lines_are_truncated() {
    // The fixture prints `echo: GET <uri>`, so a 10 KiB query makes that line over 10 KiB.
    let prefix = "echo: GET http://echo.mesh.localhost/?q=";
    let log = run_echo(&format!(
        "http://echo.mesh.localhost/?q={}",
        "x".repeat(10 * 1024)
    ))
    .await;
    let out = line(&log, prefix);
    // Exactly the first 4096 bytes of the line are kept.
    assert_eq!(out.matches('x').count(), 4096 - prefix.len(), "{out:.200}");
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_last_line_is_flushed_when_the_instance_ends() {
    let log = run_echo("http://echo.mesh.localhost/?partial=no-newline-at-end").await;
    let line = line(&log, "no-newline-at-end");
    assert!(line.contains("INFO"), "{line}");
    assert!(line.contains("service=echo"), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn output_past_the_line_cap_is_dropped_with_one_warning() {
    let log = run_echo("http://echo.mesh.localhost/?lines=3000&cap=1").await;
    let logged = log.lines().filter(|l| l.contains("loop line")).count();
    // The request's own "echo: GET" line counts toward the same 1000.
    assert_eq!(logged, 999);
    let warnings: Vec<_> = log
        .lines()
        .filter(|l| l.contains("output truncated") && l.contains("stdout"))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("WARN"), "{}", warnings[0]);
}
