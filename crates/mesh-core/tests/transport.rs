use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use iroh::endpoint::{Connection, ConnectionError};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use mesh_core::frame::{FrameError, JOIN_MAX_FRAME, SYNC_MAX_FRAME, read_frame, write_frame};
use mesh_core::membership::{MEMBERS, MESH_NS, MembershipSnapshot, REMOVED, member_bytes};
use mesh_core::transport::{
    ALPN_SYNC, Gate, NOT_MEMBER, REMOVED as CLOSE_REMOVED, addr_from_bytes, addr_to_bytes,
    build_endpoint, node_id_of,
};
use mesh_core::{CoreStore, DynClock, NodeConfig, RelayChoice};
use mesh_hlc::ManualClock;
use mesh_proto::{MeshGenesis, NodeId, RemovalRecord, Role};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

const T: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- frames

#[tokio::test]
async fn frame_round_trip() {
    let (mut a, mut b) = tokio::io::duplex(4096);
    let msg = (7u32, "hello".to_string());
    write_frame(&mut a, &msg).await.unwrap();
    write_frame(&mut a, &msg).await.unwrap();
    let got: Option<(u32, String)> = read_frame(&mut b, 1024).await.unwrap();
    assert_eq!(got, Some(msg.clone()));
    let got: Option<(u32, String)> = read_frame(&mut b, 1024).await.unwrap();
    assert_eq!(got, Some(msg));
}

#[tokio::test]
async fn frame_too_large() {
    assert_eq!(SYNC_MAX_FRAME, 1_049_600);
    assert_eq!(JOIN_MAX_FRAME, 262_144);
    let (mut a, mut b) = tokio::io::duplex(64);
    a.write_all(&((SYNC_MAX_FRAME as u32) + 1).to_be_bytes())
        .await
        .unwrap();
    let r: Result<Option<String>, _> = read_frame(&mut b, SYNC_MAX_FRAME).await;
    assert!(matches!(r, Err(FrameError::TooLarge(n)) if n == SYNC_MAX_FRAME + 1));
}

#[tokio::test]
async fn frame_clean_eof_is_none() {
    let (a, mut b) = tokio::io::duplex(64);
    drop(a);
    let r: Option<String> = read_frame(&mut b, 1024).await.unwrap();
    assert_eq!(r, None);
}

#[tokio::test]
async fn frame_truncated_is_io_error_and_garbage_is_proto() {
    let (mut a, mut b) = tokio::io::duplex(64);
    a.write_all(&10u32.to_be_bytes()).await.unwrap();
    a.write_all(&[1, 2]).await.unwrap();
    drop(a);
    let r: Result<Option<String>, _> = read_frame(&mut b, 1024).await;
    assert!(matches!(r, Err(FrameError::Io(_))));

    let (mut a, mut b) = tokio::io::duplex(64);
    a.write_all(&2u32.to_be_bytes()).await.unwrap();
    a.write_all(&[99, 99]).await.unwrap(); // bad version byte
    let r: Result<Option<String>, _> = read_frame(&mut b, 1024).await;
    assert!(matches!(r, Err(FrameError::Proto(_))));
}

// ---------------------------------------------------------------- gate

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn id(n: u8) -> NodeId {
    NodeId(key(n).verifying_key().to_bytes())
}

fn cfg(dir: &std::path::Path) -> NodeConfig {
    let mut c = NodeConfig::new(dir, "t");
    c.relay = RelayChoice::Disabled;
    c.discovery = false;
    c.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    c
}

/// Node 1 is the founder (always a member); node 2 is optionally added and optionally removed.
fn snapshot(dir: &std::path::Path, add2: bool, remove2: bool) -> Arc<MembershipSnapshot> {
    let g = MeshGenesis {
        founder: id(1),
        nonce: [9; 32],
    };
    let store = CoreStore::open(
        &dir.join("s.redb"),
        g.mesh_id(),
        key(1),
        DynClock(Arc::new(ManualClock::new(1_000_000))),
    )
    .unwrap();
    if add2 {
        store
            .set_add(MESH_NS, MEMBERS, &member_bytes(&id(2), Role::Client))
            .unwrap();
    }
    if remove2 {
        let rec = postcard::to_allocvec(&RemovalRecord {
            node_id: id(2),
            cutoff_seq: 5,
        })
        .unwrap();
        store.set_add(MESH_NS, REMOVED, &rec).unwrap();
    }
    Arc::new(MembershipSnapshot::load(&store, &g).unwrap())
}

#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let (mut s, mut r) = conn.accept_bi().await.map_err(AcceptError::from_err)?;
        let msg = r.read_to_end(1024).await.map_err(AcceptError::from_err)?;
        s.write_all(&msg).await.map_err(AcceptError::from_err)?;
        s.finish().map_err(AcceptError::from_err)?;
        conn.closed().await;
        Ok(())
    }
}

/// Starts a gated echo server (node 1) and returns a connection from node 2 plus the keep-alive handles.
async fn dial(snap: Arc<MembershipSnapshot>) -> (Connection, Router, iroh::Endpoint) {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let server = build_endpoint(&cfg(d1.path()), &key(1)).await.unwrap();
    let (_tx, rx) = watch::channel(snap);
    // Keep the sender alive for the whole test.
    std::mem::forget(_tx);
    let router = Router::builder(server.clone())
        .accept(ALPN_SYNC, Gate::new(Echo, rx))
        .spawn();
    let client = build_endpoint(&cfg(d2.path()), &key(2)).await.unwrap();
    assert_eq!(node_id_of(client.id()), id(2));
    let conn = tokio::time::timeout(T, client.connect(server.addr(), ALPN_SYNC))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    (conn, router, client)
}

async fn close_code(conn: &Connection) -> u64 {
    match tokio::time::timeout(T, conn.closed())
        .await
        .expect("no close")
    {
        ConnectionError::ApplicationClosed(c) => c.error_code.into_inner(),
        other => panic!("unexpected close: {other:?}"),
    }
}

#[tokio::test]
async fn gate_refuses_non_member() {
    let d = tempfile::tempdir().unwrap();
    let (conn, router, _c) = dial(snapshot(d.path(), false, false)).await;
    assert_eq!(close_code(&conn).await, NOT_MEMBER.into_inner());
    assert_eq!(NOT_MEMBER.into_inner(), 1);
    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn gate_refuses_removed() {
    let d = tempfile::tempdir().unwrap();
    let (conn, router, _c) = dial(snapshot(d.path(), true, true)).await;
    assert_eq!(close_code(&conn).await, CLOSE_REMOVED.into_inner());
    assert_eq!(CLOSE_REMOVED.into_inner(), 2);
    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn gate_admits_member() {
    let d = tempfile::tempdir().unwrap();
    let (conn, router, _c) = dial(snapshot(d.path(), true, false)).await;
    let (mut s, mut r) = conn.open_bi().await.unwrap();
    s.write_all(b"ping").await.unwrap();
    s.finish().unwrap();
    let echo = tokio::time::timeout(T, r.read_to_end(1024))
        .await
        .expect("echo timed out")
        .unwrap();
    assert_eq!(echo, b"ping");
    conn.close(0u32.into(), b"done");
    router.shutdown().await.unwrap();
}

#[tokio::test]
async fn addr_bytes_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let ep = build_endpoint(&cfg(d.path()), &key(1)).await.unwrap();
    let addr = ep.addr();
    let back = addr_from_bytes(&addr_to_bytes(&addr)).unwrap();
    assert_eq!(back, addr);
    assert!(addr_from_bytes(&mesh_proto::NodeAddrBytes(vec![0xff; 3])).is_err());
    ep.close().await;
}
