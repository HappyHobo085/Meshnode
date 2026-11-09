//! Demo and manual-check driver for the node core (spec §11.3). Replaced by `meshd` in
//! sub-spec 4.
//!
//! `meshdev <data-dir> init | join <ticket-text> | run`, then a REPL on stdin.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mesh_core::{Node, NodeConfig, NodeError, NodeEvent};
use mesh_proto::{InviteTicket, NodeId, Role};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "commands: invite [host|client] | members | put <ns> <key> <value...> | \
get <ns> <key> | del <ns> <key> | ls <ns> [prefix] | remove <id-prefix> | status | sync | \
blob add <path> | blob get <hash> <out> | blob getdir <hash> | quit";
const INVITE_TTL: Duration = Duration::from_secs(3600);
const FIRST_SYNC_WAIT: Duration = Duration::from_secs(60);
const BLOB_WAIT: Duration = Duration::from_secs(120);
const LS_LIMIT: u32 = 1000;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,meshdev=info")),
        )
        .init();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start runtime: {e}");
            std::process::exit(1);
        }
    };
    if let Err(msg) = rt.block_on(run()) {
        eprintln!("error: {msg}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: meshdev <data-dir> init | join <ticket-text> | run";
    let (dir, mode) = match args.as_slice() {
        [dir, mode, ..] => (PathBuf::from(dir), mode.as_str()),
        _ => return Err(usage.into()),
    };
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "meshdev".into());
    let cfg = NodeConfig::new(&dir, name);
    let node = match (mode, args.get(2)) {
        ("init", None) => Node::create_mesh(cfg).await,
        ("join", Some(text)) => {
            let ticket =
                InviteTicket::from_text(text.trim()).map_err(|e| format!("bad ticket: {e}"))?;
            Node::join(cfg, ticket).await
        }
        ("run", None) => Node::open(cfg).await,
        _ => return Err(usage.into()),
    }
    .map_err(|e| format!("cannot start node: {e}"))?;

    println!("node {} mesh {:?}", node.node_id().short(), node.mesh());
    spawn_event_log(&node);
    if mode == "join" && !node.wait_first_sync(FIRST_SYNC_WAIT).await {
        println!("first sync not complete after {FIRST_SYNC_WAIT:?}; continuing");
    }
    println!("{USAGE}");
    repl(&node).await;
    node.shutdown().await;
    Ok(())
}

fn spawn_event_log(node: &Node) {
    let mut rx = node.subscribe();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(ev) => log_event(&ev),
                Err(RecvError::Lagged(n)) => tracing::warn!("missed {n} events"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn log_event(ev: &NodeEvent) {
    match ev {
        NodeEvent::PeerUp(id) => tracing::info!("peer up {}", id.short()),
        NodeEvent::PeerDown(id) => tracing::info!("peer down {}", id.short()),
        NodeEvent::RoundComplete(id) => tracing::debug!("round complete {}", id.short()),
        other => tracing::info!("event {other:?}"),
    }
}

async fn repl(node: &Node) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break,
        };
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        if words[0] == "quit" || words[0] == "exit" {
            break;
        }
        if let Err(msg) = command(node, &line, &words).await {
            println!("error: {msg}");
        }
    }
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn find_member(node: &Node, prefix: &str) -> Result<NodeId, String> {
    let matches: Vec<NodeId> = node
        .members()
        .into_iter()
        .map(|m| m.id)
        .filter(|id| hex::encode(id.0).starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("no member with id prefix {prefix}")),
        _ => Err(format!("id prefix {prefix} is ambiguous")),
    }
}

fn parse_hash(s: &str) -> Result<mesh_core::BlobHash, String> {
    s.parse().map_err(|e| format!("bad hash: {e}"))
}

async fn command(node: &Node, line: &str, w: &[&str]) -> Result<(), String> {
    match w {
        ["invite"] | ["invite", "host"] => invite(node, Role::Host).await,
        ["invite", "client"] => invite(node, Role::Client).await,
        ["members"] => {
            for m in node.members() {
                println!(
                    "{} {:?} effective={} online={} rtt={:?}ms lag={} name={}",
                    m.id.short(),
                    m.role,
                    m.effective,
                    m.online,
                    m.rtt_ms,
                    m.lag,
                    m.info.as_ref().map(|i| i.name.as_str()).unwrap_or("?"),
                );
            }
            Ok(())
        }
        ["put", ..] => {
            let Some((ns, key, value)) = parse_put(line) else {
                println!("{USAGE}");
                return Ok(());
            };
            node.store()
                .set(ns, key, value.as_bytes().to_vec())
                .await
                .map_err(err)?;
            println!("ok");
            Ok(())
        }
        ["get", ns, key] => {
            match node.store().get(ns, key).await.map_err(err)? {
                Some(v) => println!("{}", String::from_utf8_lossy(&v)),
                None => println!("(none)"),
            }
            Ok(())
        }
        ["del", ns, key] => {
            node.store().delete(ns, key).await.map_err(err)?;
            println!("ok");
            Ok(())
        }
        ["ls", ns] => ls(node, ns, "").await,
        ["ls", ns, prefix] => ls(node, ns, prefix).await,
        ["remove", prefix] => {
            let id = find_member(node, prefix)?;
            node.remove(id).await.map_err(err)?;
            println!("removed {}", id.short());
            Ok(())
        }
        ["status"] => {
            println!("{:?}", node.status());
            println!("first sync complete: {}", node.first_sync_complete());
            if let Some(ms) = node.clock_floor_ahead_ms().await {
                println!("clock floor is {ms} ms ahead of the wall clock");
            }
            Ok(())
        }
        ["sync"] => {
            node.sync_now();
            println!("sync requested");
            Ok(())
        }
        ["blob", "add", path] => {
            let hash = node.blobs().add_path(Path::new(path)).await.map_err(err)?;
            println!("{hash}");
            Ok(())
        }
        ["blob", "get", hash, out] => {
            let hash = parse_hash(hash)?;
            with_timeout(node.blobs().fetch(hash)).await?;
            node.blobs()
                .export(hash, Path::new(out))
                .await
                .map_err(err)?;
            println!("saved {out}");
            Ok(())
        }
        ["blob", "getdir", hash] => {
            let hash = parse_hash(hash)?;
            with_timeout(node.blobs().fetch_collection(hash)).await?;
            println!("collection {hash} fetched");
            Ok(())
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

/// Splits `put <ns> <key> <value...>`: ns and key are the first two words after `put`; the
/// value is the rest of the line after the key, minus its leading whitespace.
fn parse_put(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.trim_start().strip_prefix("put")?;
    let (ns, rest) = next_word(rest)?;
    let (key, rest) = next_word(rest)?;
    let value = rest.trim_start();
    if value.is_empty() {
        return None;
    }
    Some((ns, key, value))
}

/// The first whitespace-delimited word of `s` and the text after it.
fn next_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((&s[..end], &s[end..]))
}

async fn with_timeout(
    f: impl std::future::Future<Output = Result<(), NodeError>>,
) -> Result<(), String> {
    match tokio::time::timeout(BLOB_WAIT, f).await {
        Ok(r) => r.map_err(err),
        Err(_) => Err(format!("timed out after {BLOB_WAIT:?}")),
    }
}

async fn invite(node: &Node, role: Role) -> Result<(), String> {
    let ticket = node.invite(role, INVITE_TTL).await.map_err(err)?;
    println!("{}", ticket.to_text());
    Ok(())
}

async fn ls(node: &Node, ns: &str, prefix: &str) -> Result<(), String> {
    let keys = node
        .store()
        .list_keys(ns, prefix, None, LS_LIMIT)
        .await
        .map_err(err)?;
    for k in keys {
        match node.store().get(ns, &k).await.map_err(err)? {
            Some(v) => println!("{k} = {}", String::from_utf8_lossy(&v)),
            None => println!("{k}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_put;

    #[test]
    fn put_ignores_extra_whitespace_before_value() {
        assert_eq!(parse_put("put n k v"), Some(("n", "k", "v")));
        assert_eq!(parse_put("put  n   k    v"), Some(("n", "k", "v")));
        assert_eq!(parse_put("  put	n	k	v"), Some(("n", "k", "v")));
    }

    #[test]
    fn put_keeps_internal_value_whitespace() {
        assert_eq!(
            parse_put("put n k hello  big	world "),
            Some(("n", "k", "hello  big	world "))
        );
    }

    #[test]
    fn put_with_missing_parts_is_none() {
        assert_eq!(parse_put("put"), None);
        assert_eq!(parse_put("put n"), None);
        assert_eq!(parse_put("put n k"), None);
        assert_eq!(parse_put("put n k   "), None);
    }
}
