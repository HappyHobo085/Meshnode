//! Demo and manual-check driver for the node core and its services. Replaced by `meshd` in
//! sub-spec 4.
//!
//! `meshdev <data-dir> init | join <ticket-text> | run`, then a REPL on stdin.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use mesh_core::{Node, NodeConfig, NodeError, NodeEvent};
use mesh_proto::{InviteTicket, NodeId, Placement, Role};
use mesh_runtime::CodeCache;
use mesh_services::{DeployOpts, LocalStatus, Services, ServicesConfig};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "commands: invite [host|client] | members | put <ns> <key> <value...> | \
get <ns> <key> | del <ns> <key> | ls <ns> [prefix] | remove <id-prefix> | status | sync | \
blob add <path> | blob get <hash> <out> | blob getdir <hash> | \
deploy wasm <name> <file> [--tags a,b | --nodes id,..] [--outbound host:port,..] | \
deploy static <name> <dir> [--tags a,b | --nodes id,..] | services | undeploy <name> | \
purge <name> | call <name> <METHOD> <path> [body] | quit";
/// Largest response body printed in full by `call`.
const BODY_PRINT_LIMIT: usize = 4096;
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
    let services = match Services::start(
        &node,
        ServicesConfig {
            cache: CodeCache::Mmap(dir.join("cache")),
            ..ServicesConfig::default()
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            node.shutdown().await;
            return Err(format!("cannot start services: {e}"));
        }
    };
    println!("{USAGE}");
    repl(&node, &services).await;
    services.shutdown().await;
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

async fn repl(node: &Node, services: &Services) {
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
        if let Err(msg) = command(node, services, &line, &words).await {
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

async fn command(node: &Node, services: &Services, line: &str, w: &[&str]) -> Result<(), String> {
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
        ["deploy", "wasm", name, file, flags @ ..] => {
            let opts = deploy_opts(node, flags, true)?;
            let wasm = tokio::fs::read(file)
                .await
                .map_err(|e| format!("cannot read {file}: {e}"))?;
            let m = services
                .deploy_wasm(name, Bytes::from(wasm), opts)
                .await
                .map_err(err)?;
            println!("deployed {} version {}", m.name, m.version);
            Ok(())
        }
        ["deploy", "static", name, dir, flags @ ..] => {
            let opts = deploy_opts(node, flags, false)?;
            let m = services
                .deploy_static(name, Path::new(dir), opts)
                .await
                .map_err(err)?;
            println!("deployed {} version {}", m.name, m.version);
            Ok(())
        }
        ["services"] => {
            list_services(services);
            Ok(())
        }
        ["undeploy", name] => {
            services.undeploy(name).await.map_err(err)?;
            println!("ok");
            Ok(())
        }
        ["purge", name] => {
            services.purge(name).await.map_err(err)?;
            println!("ok");
            Ok(())
        }
        ["call", ..] => {
            let Some((name, method, path, body)) = parse_call(line) else {
                println!("{USAGE}");
                return Ok(());
            };
            call(services, name, method, path, body).await
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

/// Parses the flags after `deploy wasm|static <name> <src>`. `--outbound` is wasm only.
fn deploy_opts(node: &Node, flags: &[&str], allow_outbound: bool) -> Result<DeployOpts, String> {
    let mut opts = DeployOpts::default();
    let mut it = flags.iter();
    while let Some(&flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let items = || value.split(',').filter(|s| !s.is_empty());
        match flag {
            "--tags" => opts.placement = Placement::Tags(items().map(String::from).collect()),
            "--nodes" => {
                let ids = items()
                    .map(|p| find_member(node, p))
                    .collect::<Result<_, _>>()?;
                opts.placement = Placement::Nodes(ids);
            }
            "--outbound" if allow_outbound => opts.outbound = items().map(String::from).collect(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(opts)
}

fn list_services(services: &Services) {
    let all = services.services();
    if all.is_empty() {
        println!("(no services)");
    }
    for s in all {
        let local = match &s.local {
            LocalStatus::NotPlaced => "not placed".to_owned(),
            LocalStatus::Fetching => "fetching".to_owned(),
            LocalStatus::Ready { version } => format!("ready v{version}"),
            LocalStatus::Failed { error, .. } => format!("failed: {error}"),
        };
        let hosts: Vec<String> = s.placed_hosts.iter().map(|h| h.short()).collect();
        println!(
            "{} v{} gen={} local={} hosts=[{}] data={}B",
            s.manifest.name,
            s.manifest.version,
            s.generation,
            local,
            hosts.join(","),
            s.data_bytes,
        );
    }
}

/// Splits `call <name> <METHOD> <path> [body...]`; the body is the rest of the line.
fn parse_call(line: &str) -> Option<(&str, &str, &str, &str)> {
    let rest = line.trim_start().strip_prefix("call")?;
    let (name, rest) = next_word(rest)?;
    let (method, rest) = next_word(rest)?;
    let (path, rest) = next_word(rest)?;
    Some((name, method, path, rest.trim_start()))
}

async fn call(
    services: &Services,
    name: &str,
    method: &str,
    path: &str,
    body: &str,
) -> Result<(), String> {
    let method = method
        .to_ascii_uppercase()
        .parse::<http::Method>()
        .map_err(|e| format!("bad method: {e}"))?;
    let req = http::Request::builder()
        .method(method)
        .uri(path)
        .body(Bytes::from(body.to_owned()))
        .map_err(|e| format!("bad request: {e}"))?;
    let resp = services.handle(name, req).await;
    println!("{}", resp.status());
    for (k, v) in resp.headers() {
        println!("{k}: {}", String::from_utf8_lossy(v.as_bytes()));
    }
    println!();
    println!("{}", render_body(resp.body()));
    Ok(())
}

/// The body as lossy UTF-8, cut after [`BODY_PRINT_LIMIT`] bytes.
fn render_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(BODY_PRINT_LIMIT)]);
    if body.len() > BODY_PRINT_LIMIT {
        format!("{text}\n... ({} bytes total, truncated)", body.len())
    } else {
        text.into_owned()
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
    use super::{parse_call, parse_put, render_body};

    #[test]
    fn call_splits_body_from_path() {
        assert_eq!(
            parse_call("call c post /x  hello world"),
            Some(("c", "post", "/x", "hello world"))
        );
        assert_eq!(parse_call("call c GET /"), Some(("c", "GET", "/", "")));
        assert_eq!(parse_call("call c GET"), None);
    }

    #[test]
    fn render_body_truncates_large_and_non_utf8() {
        assert_eq!(render_body(b"ok"), "ok");
        let big = vec![b'a'; 10_000];
        assert!(render_body(&big).contains("10000 bytes total"));
        assert!(!render_body(&[0xff, 0xfe]).is_empty());
    }

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
