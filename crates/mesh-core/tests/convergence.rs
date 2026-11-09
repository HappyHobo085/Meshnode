//! Multi-node convergence scenarios (spec §6, §11.2): live push, split and heal, relaying,
//! late join, restart, an oversized batch and clock skew.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{T, converged, test_cfg, wait_until};
use mesh_core::{MaintJob, Node, NodeConfig};
use mesh_hlc::ManualClock;
use mesh_proto::{NodeId, Role, SyncMsg, VersionVector};
use mesh_store::{RegEntry, SyncItem};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tempfile::TempDir;
use tokio::time::timeout;

const TTL: Duration = Duration::from_secs(900);
const NS: &str = "app";
const SET: &str = "tags";
/// Close code for a protocol violation (spec §6.2).
const PROTOCOL: u64 = 3;
const TEN_MIN_MS: u64 = 10 * 60 * 1000;

fn online(n: &Node, id: NodeId) -> bool {
    n.members().iter().any(|m| m.id == id && m.online)
}

async fn val(n: &Node, key: &str) -> Option<Vec<u8>> {
    n.store().get(NS, key).await.unwrap()
}

async fn put(n: &Node, key: &str, v: &str) {
    n.store().set(NS, key, v.as_bytes().to_vec()).await.unwrap();
}

async fn entry(n: &Node, key: &'static str) -> Option<RegEntry> {
    n.store().call(move |s| s.get_entry(NS, key)).await.unwrap()
}

async fn set_members(n: &Node) -> Vec<Vec<u8>> {
    n.store().set_members(NS, SET, None, 10_000).await.unwrap()
}

async fn pending_len(n: &Node) -> u64 {
    n.store().call(|s| s.pending_len()).await.unwrap()
}

/// Joins the mesh `inviter` belongs to with `cfg`, as a host, and waits for the first sync.
async fn invited_with(inviter: &Node, cfg: NodeConfig) -> Node {
    let ticket = inviter.invite(Role::Host, TTL).await.unwrap();
    let n = timeout(T, Node::join(cfg, ticket))
        .await
        .expect("join timed out")
        .unwrap();
    assert!(n.wait_first_sync(T).await, "first sync");
    n
}

async fn invited(inviter: &Node, dir: &Path, name: &str) -> Node {
    invited_with(inviter, test_cfg(dir, name).0).await
}

/// Waits until every pair of `nodes` has a session.
async fn fully_connected(nodes: &[&Node]) {
    for n in nodes {
        wait_until("full mesh", T, || async {
            nodes
                .iter()
                .all(|o| o.node_id() == n.node_id() || online(n, o.node_id()))
                && n.members().len() == nodes.len()
        })
        .await;
    }
}

struct Mesh {
    nodes: Vec<Node>,
    clocks: Vec<ManualClock>,
    dirs: Vec<TempDir>,
}

/// `count` nodes (the first founds, the others join as hosts), fully connected and
/// converged; `tweak` adjusts every config.
async fn mesh(count: usize, tweak: impl Fn(&mut NodeConfig)) -> Mesh {
    let dirs: Vec<TempDir> = (0..count).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes: Vec<Node> = Vec::new();
    let mut clocks = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let (mut cfg, clock) = test_cfg(d.path(), &format!("n{i}"));
        tweak(&mut cfg);
        clocks.push(clock);
        let n = match nodes.first() {
            None => Node::create_mesh(cfg).await.unwrap(),
            Some(founder) => invited_with(founder, cfg).await,
        };
        nodes.push(n);
    }
    let refs: Vec<&Node> = nodes.iter().collect();
    fully_connected(&refs).await;
    converged(&refs, T).await;
    Mesh {
        nodes,
        clocks,
        dirs,
    }
}

async fn shutdown_all(nodes: Vec<Node>) {
    for n in nodes {
        timeout(T, n.shutdown()).await.expect("shutdown timed out");
    }
}

/// One random write on `n`: a `set`, a `delete` or a `set_add`. Shared keys are `s0`..`s4`.
async fn random_write(n: &Node, rng: &mut StdRng, side: &str, i: usize) {
    let key = if rng.random_bool(0.4) {
        format!("s{}", rng.random_range(0..5))
    } else {
        format!("{side}{}", rng.random_range(0..10))
    };
    let store = n.store();
    match rng.random_range(0..3) {
        0 => store
            .set(NS, &key, format!("{side}-{i}").into_bytes())
            .await
            .unwrap(),
        1 => store.delete(NS, &key).await.unwrap(),
        _ => store
            .set_add(NS, SET, format!("{side}-{i}").into_bytes())
            .await
            .unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_push_latency() {
    // Rounds never come on their own: only live push can deliver.
    let Mesh {
        nodes, dirs: _d, ..
    } = mesh(2, |c| c.timings.sync_interval = Duration::from_secs(3600)).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    for i in 0..5 {
        let key = format!("live{i}");
        put(a, &key, "v").await;
        wait_until("B sees A's write", Duration::from_secs(1), || async {
            val(b, &key).await.is_some()
        })
        .await;
        put(b, &key, "w").await;
        wait_until("A sees B's write", Duration::from_secs(1), || async {
            val(a, &key).await.as_deref() == Some(&b"w"[..])
        })
        .await;
    }
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn split_and_heal() {
    let Mesh {
        nodes, dirs: _d, ..
    } = mesh(3, |_| {}).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());

    a.partition(&[ib, ic]);
    b.partition(&[ia]);
    c.partition(&[ia]);
    wait_until("A cut off", T, || async {
        !online(a, ib) && !online(a, ic) && !online(b, ia) && !online(c, ia)
    })
    .await;
    assert!(online(b, ic) && online(c, ib), "B and C stay connected");

    let mut rng = StdRng::seed_from_u64(0x5eed);
    // The same five keys are written on both sides first, then 45 random ops each.
    for k in 0..5 {
        a.store()
            .set(NS, &format!("s{k}"), b"a".to_vec())
            .await
            .unwrap();
        let bc = if k % 2 == 0 { b } else { c };
        bc.store()
            .set(NS, &format!("s{k}"), b"bc".to_vec())
            .await
            .unwrap();
    }
    for i in 5..50 {
        random_write(a, &mut rng, "a", i).await;
        let bc = if i % 2 == 0 { b } else { c };
        random_write(bc, &mut rng, "bc", i).await;
    }

    // Nothing crossed the split.
    assert_ne!(entry(a, "s0").await, entry(b, "s0").await, "split held");

    a.heal();
    b.heal();
    c.heal();
    converged(&[a, b, c], Duration::from_secs(10)).await;
    for key in ["s0", "s1", "s2", "s3", "s4"] {
        let ea = entry(a, key).await;
        assert!(ea.is_some(), "{key} has an entry");
        assert_eq!(ea, entry(b, key).await, "{key}: A and B differ");
        assert_eq!(ea, entry(c, key).await, "{key}: A and C differ");
    }
    let sa = set_members(a).await;
    assert_eq!(sa, set_members(b).await);
    assert_eq!(sa, set_members(c).await);
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_through_third_node() {
    let Mesh {
        nodes, dirs: _d, ..
    } = mesh(3, |_| {}).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let (ia, ib, ic) = (a.node_id(), b.node_id(), c.node_id());

    a.partition(&[ic]);
    c.partition(&[ia]);
    wait_until("A and C apart", T, || async {
        !online(a, ic) && !online(c, ia)
    })
    .await;
    wait_until("both linked to B", T, || async {
        online(a, ib) && online(c, ib) && online(b, ia) && online(b, ic)
    })
    .await;

    put(a, "from_a", "1").await;
    put(c, "from_c", "2").await;
    wait_until("C has A's write", T, || async {
        val(c, "from_a").await.is_some()
    })
    .await;
    wait_until("A has C's write", T, || async {
        val(a, "from_c").await.is_some()
    })
    .await;
    assert!(!online(a, ic) && !online(c, ia), "A and C never linked");
    converged(&[a, b, c], T).await;
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_join_gets_history() {
    let da = tempfile::tempdir().unwrap();
    let dd = tempfile::tempdir().unwrap();
    let a = Node::create_mesh(test_cfg(da.path(), "a").0).await.unwrap();
    a.store()
        .call(|s| {
            for i in 0..1000u32 {
                let key = format!("k{}", i % 400);
                if i % 7 == 6 {
                    s.delete(NS, &key)?;
                } else {
                    s.set(NS, &key, &i.to_be_bytes())?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    let d = invited(&a, dd.path(), "d").await;
    converged(&[&a, &d], T).await;
    shutdown_all(vec![a, d]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resumes() {
    let Mesh {
        mut nodes, dirs, ..
    } = mesh(2, |_| {}).await;
    let b = nodes.pop().unwrap();
    let a = nodes.pop().unwrap();
    let ib = b.node_id();
    timeout(T, b.shutdown()).await.expect("shutdown timed out");
    wait_until("A sees B gone", T, || async { !online(&a, ib) }).await;

    for i in 0..20 {
        put(&a, &format!("while_down{i}"), "x").await;
    }
    let b = Node::open(test_cfg(dirs[1].path(), "n1").0).await.unwrap();
    assert_eq!(b.node_id(), ib);
    converged(&[&a, &b], T).await;
    assert_eq!(val(&b, "while_down19").await.as_deref(), Some(&b"x"[..]));
    put(&b, "after_restart", "y").await;
    converged(&[&a, &b], T).await;
    shutdown_all(vec![a, b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_batch_closes_session() {
    let Mesh {
        nodes, dirs: _d, ..
    } = mesh(2, |_| {}).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let (ia, ib) = (a.node_id(), b.node_id());
    b.store()
        .call(|s| {
            for i in 0..257u32 {
                s.set(NS, &format!("b{i}"), &i.to_be_bytes())?;
            }
            Ok(())
        })
        .await
        .unwrap();
    converged(&[a, b], T).await;

    // 257 genuine ops of B in one `Ops` message: one over the limit.
    let ops: Vec<_> = b
        .store()
        .call(move |s| s.diff_since(&VersionVector::default(), 1000))
        .await
        .unwrap()
        .into_iter()
        .filter_map(|i| match i {
            SyncItem::Op(op) if op.body.origin == ib => Some(op),
            _ => None,
        })
        .take(257)
        .collect();
    assert_eq!(ops.len(), 257);
    let body = mesh_proto::encode(&SyncMsg::Ops { ops });
    let mut frame = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
    frame.extend_from_slice(&body);

    assert_ne!(b.last_peer_close_code(ia), Some(PROTOCOL));
    assert!(b.send_raw_sync_frame(ia, frame), "B has a session with A");
    wait_until("A closes with PROTOCOL", T, || async {
        b.last_peer_close_code(ia) == Some(PROTOCOL)
    })
    .await;

    // B reconnects with backoff and replication resumes.
    wait_until("B reconnected", T, || async {
        online(a, ib) && online(b, ia)
    })
    .await;
    put(b, "after", "1").await;
    wait_until("A has B's new write", T, || async {
        val(a, "after").await.is_some()
    })
    .await;
    converged(&[a, b], T).await;
    shutdown_all(nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skew_deferred_then_applied() {
    let Mesh {
        nodes,
        clocks,
        dirs: _d,
    } = mesh(2, |_| {}).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    assert_eq!(pending_len(a).await, 0);

    clocks[1].advance(TEN_MIN_MS);
    put(b, "skewed", "v").await;
    wait_until("A defers B's op", T, || async { pending_len(a).await >= 1 }).await;
    assert_eq!(val(a, "skewed").await, None, "deferred op not visible");
    // Still too far ahead: a retry leaves it pending.
    a.maint_now(MaintJob::RetryPending).await.unwrap();
    assert_eq!(val(a, "skewed").await, None, "still deferred");
    assert!(pending_len(a).await >= 1);

    clocks[0].advance(TEN_MIN_MS);
    a.maint_now(MaintJob::RetryPending).await.unwrap();
    assert_eq!(val(a, "skewed").await.as_deref(), Some(&b"v"[..]));
    assert_eq!(pending_len(a).await, 0);
    converged(&[a, b], T).await;
    shutdown_all(nodes).await;
}
