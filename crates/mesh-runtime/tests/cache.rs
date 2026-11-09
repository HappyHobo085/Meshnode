mod common;

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use common::fixture;
use mesh_proto::{Limits, NodeId};
use mesh_runtime::{CodeCache, MemKv, RequestCtx, Runtime, RuntimeConfig};

fn runtime(cache: CodeCache) -> Runtime {
    Runtime::new(RuntimeConfig {
        cache,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

async fn echoes(rt: &Runtime, loaded: &mesh_runtime::Loaded) {
    let ctx = RequestCtx {
        service: "echo".into(),
        node_id: NodeId([7; 32]),
        kv: Arc::new(MemKv::default()),
        limits: Limits::default(),
        outbound: Vec::new(),
    };
    let req = http::Request::post("http://echo.mesh.localhost/")
        .body(Bytes::from_static(b"hello, cache"))
        .unwrap();
    let resp = rt.handle(loaded, req, ctx).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body().as_ref(), b"hello, cache");
}

fn cwasm_files(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn code_cache_hit_after_restart_mmap_and_read_modes() {
    let wasm = fixture("echo");
    for mode in [CodeCache::Mmap as fn(_) -> _, CodeCache::Read] {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(mode(dir.path().to_path_buf()));
        let l = rt.load([1; 32], &wasm).await.unwrap();
        echoes(&rt, &l).await;
        assert_eq!(rt.cache_stats(), (0, 1));
        let files = cwasm_files(dir.path());
        assert_eq!(files.len(), 1, "exactly one entry and no temp files");
        assert_eq!(files[0].extension().unwrap(), "cwasm");
        drop(l);
        drop(rt);

        let rt = runtime(mode(dir.path().to_path_buf()));
        let l = rt.load([1; 32], &wasm).await.unwrap();
        assert_eq!(rt.cache_stats(), (1, 0));
        echoes(&rt, &l).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_cache_entry_recompiled() {
    let wasm = fixture("echo");
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(CodeCache::Read(dir.path().to_path_buf()));
    rt.load([2; 32], &wasm).await.unwrap();
    let entry = cwasm_files(dir.path()).remove(0);
    std::fs::write(&entry, b"junk, not a compiled component").unwrap();
    drop(rt);

    let rt = runtime(CodeCache::Read(dir.path().to_path_buf()));
    let l = rt.load([2; 32], &wasm).await.unwrap();
    assert_eq!(rt.cache_stats(), (0, 1), "a corrupt entry counts as a miss");
    echoes(&rt, &l).await;
    assert!(
        std::fs::read(&entry).unwrap().len() > 100,
        "entry rewritten"
    );
    drop(rt);

    let rt = runtime(CodeCache::Read(dir.path().to_path_buf()));
    rt.load([2; 32], &wasm).await.unwrap();
    assert_eq!(rt.cache_stats(), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_write_failure_still_returns_component() {
    let wasm = fixture("echo");
    let dir = tempfile::tempdir().unwrap();
    // A file where the cache directory should be: every write fails.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let rt = runtime(CodeCache::Mmap(blocker.join("cache")));
    let l = rt.load([3; 32], &wasm).await.unwrap();
    echoes(&rt, &l).await;
    assert_eq!(rt.cache_stats(), (0, 1));
}
