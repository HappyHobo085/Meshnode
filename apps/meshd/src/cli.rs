//! The command line (spec §6.2), as clap derive types.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use mesh_admin::{MAX_LOG_LINES, MAX_PROBE_COUNT, MAX_PROBE_TIMEOUT_MS, MIN_PROBE_TIMEOUT_MS};

#[derive(Parser, Debug)]
#[command(
    name = "meshd",
    version,
    about = "Meshnode daemon and command line",
    after_help = "Exit codes: 0 success, 1 error reported by the daemon, 2 usage error, \
                  3 the daemon is not running."
)]
pub struct Cli {
    /// The data directory (default: MESHD_DATA, else the operating system's default).
    #[arg(long, global = true, value_name = "DIR")]
    pub data: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the daemon in the foreground.
    Run {
        /// Started by the Windows service manager.
        #[arg(long)]
        service: bool,
    },
    /// Found a new mesh with this node.
    Init {
        #[arg(long)]
        name: String,
    },
    /// Join the mesh an invite ticket belongs to.
    Join {
        ticket: String,
        #[arg(long)]
        name: String,
    },
    /// Delete this node's identity after it was removed from its mesh.
    ResetIdentity {
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Declare this node up to date although it has not synced with a host for over 80
    /// days (for a host left on its own). Deletions that other hosts already cleaned up
    /// may come back.
    DeclareFresh {
        /// Confirm the risk (required).
        #[arg(long)]
        yes: bool,
    },
    /// Show the daemon's state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List the mesh's members.
    Members {
        #[arg(long)]
        json: bool,
    },
    /// Print an invite ticket and its QR code.
    Invite {
        /// Invite a client (default: a host).
        #[arg(long)]
        client: bool,
        /// How long the ticket stays valid: 30s, 15m, 2h, 1d or plain seconds.
        #[arg(long, default_value = "15m", value_parser = parse_ttl)]
        ttl: u32,
    },
    /// Remove a member (a unique prefix of its id).
    Remove { id_prefix: String },
    /// List the deployed services.
    Services {
        #[arg(long)]
        json: bool,
    },
    /// Deploy a service.
    #[command(subcommand)]
    Deploy(Deploy),
    /// Change where a service runs.
    SetPlacement {
        name: String,
        #[command(flatten)]
        placement: PlacementArgs,
    },
    /// Replace a WASM service's allowed outbound `host:port` list (none clears it).
    SetOutbound {
        name: String,
        #[arg(value_delimiter = ',')]
        outbound: Vec<String>,
    },
    /// Change a service's limits (the ones not given keep their value).
    SetLimits {
        name: String,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Remove a service (its data stays until purged).
    Undeploy { name: String },
    /// Delete a service's data everywhere.
    Purge {
        name: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Read or change meshd.toml.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Show the daemon's log.
    Logs {
        /// How many of the last lines to show.
        #[arg(short = 'n', long, default_value_t = 100,
              value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_LOG_LINES)))]
        lines: u32,
        /// Keep showing new lines.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Measure the round-trip time to a member (a unique prefix of its id) with `_ping`
    /// requests sent straight to it.
    Probe {
        node: String,
        /// How many samples to take.
        #[arg(long, default_value_t = 20,
              value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_PROBE_COUNT)))]
        count: u32,
        /// Time between samples (such as 100ms, 5s or 1m).
        #[arg(long, default_value = "5s", value_parser = parse_interval_ms)]
        interval: u32,
        /// How long one sample may take, connecting included (1s to 2m).
        #[arg(long, default_value = "30s", value_parser = parse_probe_timeout_ms)]
        timeout: u32,
    },
    /// Install or control meshd as an OS service (needs root / Administrator, except
    /// status).
    #[command(subcommand)]
    Service(ServiceCmd),
}

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    /// Install the service for the data directory (--data) and start it.
    Install,
    /// Stop and remove the service; the data directory is kept unless --purge-data.
    Uninstall {
        /// Also delete the data directory.
        #[arg(long)]
        purge_data: bool,
    },
    /// Start the installed service.
    Start,
    /// Stop the installed service.
    Stop,
    /// Show whether the service is installed and running.
    Status {
        /// One JSON line: {"status": "running|stopped|starting|stopping|not-installed",
        /// "dataDir": the installed service's data directory, or null}.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Deploy {
    /// Deploy a WASM component.
    Wasm {
        name: String,
        file: PathBuf,
        #[command(flatten)]
        placement: PlacementArgs,
        /// Hosts and ports the service may connect to, as host:port,..
        #[arg(long, value_delimiter = ',')]
        outbound: Vec<String>,
        #[command(flatten)]
        limits: LimitArgs,
    },
    /// Deploy a directory as a static site.
    Static {
        name: String,
        dir: PathBuf,
        #[command(flatten)]
        placement: PlacementArgs,
        #[command(flatten)]
        limits: LimitArgs,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print one key, or all of them.
    Get {
        key: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Change a key.
    Set { key: String, value: String },
}

/// Where a service runs: on hosts with these tags, on these nodes, or (neither) on all hosts.
#[derive(Args, Debug)]
pub struct PlacementArgs {
    /// Hosts with any of these tags, as a,b
    #[arg(long, value_delimiter = ',', conflicts_with = "nodes")]
    pub tags: Option<Vec<String>>,
    /// These nodes (id prefixes), as id,..
    #[arg(long, value_delimiter = ',', conflicts_with = "tags")]
    pub nodes: Option<Vec<String>>,
}

#[derive(Args, Debug)]
pub struct LimitArgs {
    #[arg(long)]
    pub memory_mb: Option<u32>,
    #[arg(long)]
    pub timeout_ms: Option<u64>,
    #[arg(long)]
    pub max_body: Option<u64>,
}

/// `30s`, `15m`, `2h`, `1d` or a plain number of seconds.
pub fn parse_ttl(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration such as 30s, 15m, 2h or 1d"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => {
            return Err(format!(
                "{s:?} is not a duration such as 30s, 15m, 2h or 1d"
            ));
        }
    };
    let secs = n.saturating_mul(mult);
    if secs == 0 {
        return Err("the ttl must be positive".into());
    }
    u32::try_from(secs).map_err(|_| "the ttl is too long".into())
}

/// `100ms`, `5s`, `1m` or a plain number of seconds, as milliseconds.
pub fn parse_interval_ms(s: &str) -> Result<u32, String> {
    let bad = || format!("{s:?} is not a duration such as 100ms, 5s or 1m");
    let t = s.trim();
    let (digits, unit) = match t.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => t.split_at(i),
        None => (t, "s"),
    };
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let mult = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        _ => return Err(bad()),
    };
    u32::try_from(n.saturating_mul(mult)).map_err(|_| "the interval is too long".into())
}

/// A sample timeout in milliseconds, within what the daemon accepts.
fn parse_probe_timeout_ms(s: &str) -> Result<u32, String> {
    let ms = parse_interval_ms(s)?;
    if (MIN_PROBE_TIMEOUT_MS..=MAX_PROBE_TIMEOUT_MS).contains(&ms) {
        Ok(ms)
    } else {
        Err("the timeout must be between 1s and 2m".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_timeout_range() {
        assert_eq!(parse_probe_timeout_ms("1s"), Ok(1000));
        assert_eq!(parse_probe_timeout_ms("2m"), Ok(120_000));
        assert!(parse_probe_timeout_ms("500ms").is_err());
        assert!(parse_probe_timeout_ms("3m").is_err());
    }

    #[test]
    fn interval_forms() {
        assert_eq!(parse_interval_ms("100ms"), Ok(100));
        assert_eq!(parse_interval_ms("5s"), Ok(5000));
        assert_eq!(parse_interval_ms("2"), Ok(2000));
        assert_eq!(parse_interval_ms("1m"), Ok(60_000));
        for bad in ["", "ms", "5x", "-1", "99999999999m"] {
            assert!(parse_interval_ms(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ttl_forms() {
        assert_eq!(parse_ttl("15m"), Ok(900));
        assert_eq!(parse_ttl("30"), Ok(30));
        assert_eq!(parse_ttl("2h"), Ok(7200));
        assert_eq!(parse_ttl("1d"), Ok(86_400));
        for bad in ["", "m", "0s", "5x", "-1", "99999999999d"] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn clap_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
