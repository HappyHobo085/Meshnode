//! Minimal blobs (spec §8): add, fetch from members, pin, behind the gate.

mod common;

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use common::{T, test_cfg, wait_until};
use iroh::endpoint::ConnectionError;
use mesh_core::transport::{NOT_MEMBER, build_endpoint};
use mesh_core::{BlobHash, Node, NodeError};
use mesh_proto::Role;
use tokio::time::timeout;

const TTL: Duration = Duration::from_secs(900);

/// Founder A plus joined B, with each seeing the other online.
async fn pair(da: &Path, db: &Path) -> (Node, Node) {
    let a = Node::create_mesh(test_cfg(da, "a").0).await.unwrap();
    let ticket = a.invite(Role::Host, TTL).await.unwrap();
    let b = timeout(T, Node::join(test_cfg(db, "b").0, ticket))
        .await
        .expect("join timed out")
        .unwrap();
    for (x, y) in [(&a, &b), (&b, &a)] {
        let id = y.node_id();
        wait_until("peer online", T, || async {
            x.members().iter().any(|m| m.id == id && m.online)
        })
        .await;
    }
    (a, b)
}

fn data(n: usize) -> Bytes {
    Bytes::from((0..n).map(|i| (i * 31 % 251) as u8).collect::<Vec<u8>>())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_add_fetch() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path()).await;
    let bytes = data(1 << 20);
    let hash = a.blobs().add_bytes(bytes.clone()).await.unwrap();
    assert!(a.blobs().has(hash).await.unwrap());
    assert!(!b.blobs().has(hash).await.unwrap());
    timeout(T, b.blobs().fetch(hash))
        .await
        .expect("fetch timed out")
        .unwrap();
    assert!(b.blobs().has(hash).await.unwrap());
    assert_eq!(b.blobs().read(hash).await.unwrap(), bytes);
    assert!(a.blobs().is_pinned(hash).await.unwrap());
    assert!(b.blobs().is_pinned(hash).await.unwrap());
    // Present now: a second fetch is a no-op.
    b.blobs().fetch(hash).await.unwrap();
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collection_add_fetch_export() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path()).await;
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("a")).unwrap();
    std::fs::write(src.path().join("index.html"), b"<html>hi</html>").unwrap();
    std::fs::write(src.path().join("a/b.txt"), data(100_000)).unwrap();
    let hash = a.blobs().add_path(src.path()).await.unwrap();
    assert!(a.blobs().has_collection(hash).await.unwrap());
    assert!(!b.blobs().has_collection(hash).await.unwrap());
    timeout(T, b.blobs().fetch_collection(hash))
        .await
        .expect("fetch timed out")
        .unwrap();
    assert!(a.blobs().is_pinned(hash).await.unwrap());
    assert!(b.blobs().is_pinned(hash).await.unwrap());
    assert!(b.blobs().has_collection(hash).await.unwrap());
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("site");
    b.blobs().export(hash, &dest).await.unwrap();
    assert_eq!(
        std::fs::read(dest.join("index.html")).unwrap(),
        b"<html>hi</html>"
    );
    assert_eq!(
        std::fs::read(dest.join("a/b.txt")).unwrap(),
        &data(100_000)[..]
    );
    // A single file added by path is a plain blob and exports to a file.
    let h = a
        .blobs()
        .add_path(&src.path().join("index.html"))
        .await
        .unwrap();
    b.blobs().fetch(h).await.unwrap();
    let f = out.path().join("copy.html");
    b.blobs().export(h, &f).await.unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"<html>hi</html>");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_unknown_hash_unavailable() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path()).await;
    let unknown = BlobHash::new(b"nobody has this");
    let r = timeout(T, b.blobs().fetch(unknown))
        .await
        .expect("fetch timed out");
    assert!(matches!(r, Err(NodeError::BlobUnavailable)), "{r:?}");
    assert!(!b.blobs().has(unknown).await.unwrap());
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_fetch_gated() {
    let da = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(da.path(), "a").0).await.unwrap();
    let hash = a.blobs().add_bytes(data(1000)).await.unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let key = ed25519_dalek::SigningKey::from_bytes(&[77; 32]);
    let (cfg, _) = test_cfg(d2.path(), "x");
    let ep = build_endpoint(&cfg, &key).await.unwrap();
    let conn = timeout(T, ep.connect(a.endpoint_addr_for_test(), iroh_blobs::ALPN))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    match timeout(T, conn.closed()).await.expect("no close") {
        ConnectionError::ApplicationClosed(c) => {
            assert_eq!(c.error_code.into_inner(), NOT_MEMBER.into_inner())
        }
        other => panic!("unexpected close: {other:?}"),
    }
    assert!(a.blobs().has(hash).await.unwrap());
    ep.close().await;
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pins_survive_restart() {
    let da = tempfile::tempdir().unwrap();
    let (cfg, _) = test_cfg(da.path(), "a");
    let a = Node::create_mesh(cfg).await.unwrap();
    let bytes = data(50_000);
    let hash = a.blobs().add_bytes(bytes.clone()).await.unwrap();
    a.shutdown().await;
    let (cfg, _) = test_cfg(da.path(), "a");
    let a = Node::open(cfg).await.unwrap();
    assert!(a.blobs().has(hash).await.unwrap());
    assert!(a.blobs().is_pinned(hash).await.unwrap());
    assert_eq!(a.blobs().read(hash).await.unwrap(), bytes);
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_collection_names_refused() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path()).await;
    for bad in ["../x", "a\\b"] {
        let hash = a
            .blobs()
            .add_named_for_test(vec![("ok.txt".into(), data(10)), (bad.into(), data(20))])
            .await
            .unwrap();
        timeout(T, b.blobs().fetch_collection(hash))
            .await
            .expect("fetch timed out")
            .unwrap();
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("d");
        assert!(b.blobs().export(hash, &dest).await.is_err(), "{bad}");
        assert!(!dest.exists(), "{bad}: nothing written");
        assert!(!out.path().join("x").exists());
    }
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_blob_of_hash_size_is_not_a_collection() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = pair(da.path(), db.path()).await;
    let bytes = data(64);
    let hash = a.blobs().add_bytes(bytes.clone()).await.unwrap();
    timeout(T, b.blobs().fetch(hash))
        .await
        .expect("fetch timed out")
        .unwrap();
    assert_eq!(b.blobs().read(hash).await.unwrap(), bytes);
    assert!(b.blobs().is_pinned(hash).await.unwrap());
    assert!(!b.blobs().has_collection(hash).await.unwrap());
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("f.bin");
    b.blobs().export(hash, &f).await.unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), &bytes[..]);
    // A missing blob reads as unavailable.
    let r = b.blobs().read(BlobHash::new(b"absent")).await;
    assert!(matches!(r, Err(NodeError::BlobUnavailable)), "{r:?}");
    a.shutdown().await;
    b.shutdown().await;
}
