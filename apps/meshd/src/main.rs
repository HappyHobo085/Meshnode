//! `meshd`: the Meshnode daemon and its command line (spec §6).

mod cli;
mod commands;
mod local;
mod output;

use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use meshd::{Daemon, MeshdConfig, logging};

use commands::CliError;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(commands::run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e.message);
            ExitCode::from(e.code)
        }
    }
}

/// `meshd run [--service]`: the daemon in the foreground until Ctrl-C or SIGTERM.
pub async fn run_daemon(data: &Path, service: bool) -> Result<(), CliError> {
    if service {
        return run_service(data).await;
    }
    run_foreground(data).await
}

/// `run --service` on Windows: started by the service control manager, it reports its
/// state to it and stops on a service stop instead of Ctrl-C (spec §6.4). systemd and
/// launchd run plain `run`, so elsewhere it is the same as `run`.
#[cfg(windows)]
async fn run_service(data: &Path) -> Result<(), CliError> {
    let runtime = tokio::runtime::Handle::current();
    let data = data.to_path_buf();
    tokio::task::spawn_blocking(move || meshd::service::windows::run_dispatcher(data, runtime))
        .await
        .map_err(|e| CliError::failed(format!("the service dispatcher failed: {e}")))?
        .map_err(|e| CliError::failed(e.to_string()))
}

#[cfg(not(windows))]
async fn run_service(data: &Path) -> Result<(), CliError> {
    run_foreground(data).await
}

async fn run_foreground(data: &Path) -> Result<(), CliError> {
    let cfg = MeshdConfig::load_or_default(data).map_err(|e| CliError::failed(e.to_string()))?;
    std::fs::create_dir_all(data)
        .map_err(|e| CliError::failed(format!("data directory {}: {e}", data.display())))?;
    let _log = logging::init(data, &cfg.log_level);
    let daemon = Daemon::start(data.to_path_buf(), cfg)
        .await
        .map_err(|e| CliError::failed(e.to_string()))?;
    daemon.run_until(shutdown_signal()).await;
    Ok(())
}

/// Ctrl-C, or SIGTERM on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
