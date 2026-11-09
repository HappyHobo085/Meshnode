//! Shared harness for the in-process node tests (spec §11.1).
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mesh_core::membership::{CONSUMED, MEMBERS, MESH_NS, REMOVED};
use mesh_core::{Node, NodeConfig, NodeEvent, RelayChoice, Timings};
use mesh_hlc::ManualClock;
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep, timeout};

/// Default timeout for a single await in the tests.
pub const T: Duration = Duration::from_secs(30);
/// How often polling helpers re-check their condition.
const POLL: Duration = Duration::from_millis(25);
const PAGE: u32 = 256;

/// A loopback-only config: relay disabled, discovery off, fast timings and its own clock.
pub fn test_cfg(dir: &Path, name: &str) -> (NodeConfig, ManualClock) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1_700_000_000_000);
    let clock = ManualClock::new(now);
    let mut cfg = NodeConfig::new(dir, name);
    cfg.relay = RelayChoice::Disabled;
    cfg.discovery = false;
    cfg.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    cfg.timings = Timings::fast_for_tests();
    cfg.wall = Arc::new(clock.clone());
    (cfg, clock)
}

/// Every register (per namespace) and every `_mesh` set of one node.
#[derive(Debug, PartialEq, Eq)]
struct Dump {
    regs: Vec<(String, String, Vec<u8>)>,
    sets: Vec<(String, Vec<Vec<u8>>)>,
}

async fn dump(node: &Node, namespaces: &BTreeSet<String>) -> Dump {
    let store = node.store();
    let mut regs = Vec::new();
    for ns in namespaces {
        let mut after: Option<String> = None;
        loop {
            let keys = store.list_keys(ns, "", after.clone(), PAGE).await.unwrap();
            for k in &keys {
                if let Some(v) = store.get(ns, k).await.unwrap() {
                    regs.push((ns.clone(), k.clone(), v));
                }
            }
            if keys.len() < PAGE as usize {
                break;
            }
            after = keys.last().cloned();
        }
    }
    let mut sets = Vec::new();
    for set in [MEMBERS, REMOVED, CONSUMED] {
        let mut all: Vec<Vec<u8>> = Vec::new();
        loop {
            let page = store
                .set_members(MESH_NS, set, all.last().cloned(), PAGE)
                .await
                .unwrap();
            let done = page.len() < PAGE as usize;
            all.extend(page);
            if done {
                break;
            }
        }
        sets.push((set.to_owned(), all));
    }
    Dump { regs, sets }
}

/// Waits until every node returns the same registers (over every namespace any of them has)
/// and the same `_mesh` sets. Panics after `limit`.
pub async fn converged(nodes: &[&Node], limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        let mut namespaces = BTreeSet::new();
        for n in nodes {
            let sizes = n.store().call(|s| s.namespace_sizes()).await.unwrap();
            namespaces.extend(sizes.into_keys());
        }
        let mut dumps = Vec::new();
        for n in nodes {
            dumps.push(dump(n, &namespaces).await);
        }
        if dumps.windows(2).all(|w| w[0] == w[1]) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("nodes did not converge within {limit:?}: {dumps:#?}");
        }
        sleep(POLL).await;
    }
}

/// Waits for the first event matching `pred`. Panics after `limit`.
pub async fn wait_event(
    rx: &mut broadcast::Receiver<NodeEvent>,
    mut pred: impl FnMut(&NodeEvent) -> bool,
    limit: Duration,
) -> NodeEvent {
    let fut = async {
        loop {
            match rx.recv().await {
                Ok(ev) if pred(&ev) => return ev,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("event channel closed"),
            }
        }
    };
    timeout(limit, fut)
        .await
        .expect("timed out waiting for event")
}

/// Polls `cond` until it holds. Panics after `limit`.
pub async fn wait_until<F, Fut>(what: &str, limit: Duration, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + limit;
    while !cond().await {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        sleep(POLL).await;
    }
}
