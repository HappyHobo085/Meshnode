//! `init`, `join` and `reset-identity` on the data directory when no daemon is reachable
//! (spec §6.2). The node runs only for as long as the command needs it.

use std::path::Path;
use std::time::Duration;

use mesh_core::local::read_genesis;
use mesh_core::{Node, NodeConfig, NodeStatus};
use mesh_proto::InviteTicket;
use meshd::MeshdConfig;
use tokio::time::{Instant, sleep};

use crate::commands::{CliError, confirm};

/// How long a local `join` waits for the first sync.
const FIRST_SYNC_WAIT: Duration = Duration::from_secs(60);
/// How long `reset-identity` waits for an opened node to show that it was removed.
const REMOVED_WAIT: Duration = Duration::from_secs(5);

fn failed(what: &str, e: impl std::fmt::Display) -> CliError {
    CliError::failed(format!("{what}: {e}"))
}

/// The config with `name` set; the data directory created.
fn prepare(data: &Path, name: &str) -> Result<MeshdConfig, CliError> {
    let mut cfg =
        MeshdConfig::load_or_default(data).map_err(|e| CliError::failed(e.to_string()))?;
    cfg.set("name", name)
        .map_err(|e| CliError::failed(e.to_string()))?;
    std::fs::create_dir_all(data)
        .map_err(|e| failed(&format!("cannot create {}", data.display()), e))?;
    Ok(cfg)
}

fn node_config(data: &Path, cfg: &MeshdConfig) -> NodeConfig {
    let mut nc = NodeConfig::new(data, cfg.name.clone());
    nc.tags = cfg.tags.clone();
    nc.relay = cfg.relay_choice();
    nc.discovery = cfg.discovery;
    nc
}

pub async fn init(data: &Path, name: &str) -> Result<(), CliError> {
    let cfg = prepare(data, name)?;
    let node = Node::create_mesh(node_config(data, &cfg))
        .await
        .map_err(|e| CliError::failed(e.to_string()))?;
    let saved = cfg.save(data);
    node.shutdown().await;
    saved.map_err(|e| CliError::failed(e.to_string()))?;
    println!("Created a new mesh; this node is {name:?}. Start the daemon with \"meshd run\".");
    Ok(())
}

pub async fn join(data: &Path, ticket: &str, name: &str) -> Result<(), CliError> {
    let ticket = InviteTicket::from_text(ticket)
        .map_err(|e| CliError::failed(format!("invalid invite ticket: {e}")))?;
    let cfg = prepare(data, name)?;
    let node = Node::join(node_config(data, &cfg), ticket)
        .await
        .map_err(|e| CliError::failed(e.to_string()))?;
    if let Err(e) = cfg.save(data) {
        node.shutdown().await;
        return Err(CliError::failed(e.to_string()));
    }
    println!("Joined; waiting for the first sync (up to 60 s)...");
    let synced = node.wait_first_sync(FIRST_SYNC_WAIT).await;
    node.shutdown().await;
    if synced {
        println!("Joined the mesh as {name:?}. Start the daemon with \"meshd run\".");
    } else {
        eprintln!(
            "warning: the first sync did not finish within 60 s; the daemon will continue it"
        );
        println!("Joined the mesh as {name:?}. Start the daemon with \"meshd run\".");
    }
    Ok(())
}

/// Only for a node that was removed from its mesh (or never initialised).
pub async fn reset_identity(data: &Path, yes: bool) -> Result<(), CliError> {
    if !data.exists() {
        println!("Nothing to reset: {} does not exist.", data.display());
        return Ok(());
    }
    let initialised = read_genesis(data)
        .map_err(|e| failed("cannot read the data directory", e))?
        .is_some();
    if initialised {
        let cfg =
            MeshdConfig::load_or_default(data).map_err(|e| CliError::failed(e.to_string()))?;
        let node = Node::open(node_config(data, &cfg))
            .await
            .map_err(|e| CliError::failed(e.to_string()))?;
        let until = Instant::now() + REMOVED_WAIT;
        let removed = loop {
            if matches!(
                node.status(),
                NodeStatus::Removed { .. } | NodeStatus::RefusedAsRemoved { .. }
            ) {
                break true;
            }
            if Instant::now() >= until {
                break false;
            }
            sleep(Duration::from_millis(100)).await;
        };
        node.shutdown().await;
        if !removed {
            return Err(CliError::failed(
                "this node was not removed from its mesh; reset-identity only works on a removed node",
            ));
        }
    }
    if !yes
        && !confirm("Delete this node's identity? It cannot rejoin its mesh without a new invite.")
    {
        return Err(CliError::failed("not confirmed; nothing was reset"));
    }
    meshd::wipe_identity(data).map_err(|e| failed("reset failed", e))?;
    println!("Identity reset. Run \"meshd init\" or \"meshd join\" to start over.");
    Ok(())
}
