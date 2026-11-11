//! What each command does: talk to the daemon (one connection per request, never held
//! across a prompt), or run the local fallbacks.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use mesh_admin::{
    AdminClient, AdminError, AdminRequest, AdminResponse, DaemonState, DeployOpts, FileMeta,
    MemberInfoView, ServiceInfoView, endpoint_for,
};
use mesh_proto::{InviteTicket, Limits, NodeId, Placement, Role};
use serde::Serialize;

use crate::cli::{Cli, Command, ConfigCmd, Deploy, LimitArgs, PlacementArgs, ServiceCmd};
use crate::output::{say, say_raw};
use crate::{local, output};

/// A failed command: the message for stderr and the process exit code (1 daemon-reported
/// or other failure, 2 usage, 3 daemon not running).
#[derive(Debug)]
pub struct CliError {
    pub code: u8,
    pub message: String,
}

impl CliError {
    pub fn failed(message: impl Into<String>) -> Self {
        CliError {
            code: 1,
            message: message.into(),
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        CliError {
            code: 2,
            message: message.into(),
        }
    }
}

impl From<AdminError> for CliError {
    fn from(e: AdminError) -> Self {
        match e {
            AdminError::NotRunning => CliError {
                code: 3,
                message: e.to_string(),
            },
            // The daemon's own words, verbatim.
            AdminError::Daemon { code, message } => error_answer(code, message),
            e => CliError::failed(e.to_string()),
        }
    }
}

fn error_answer(code: mesh_admin::ErrorCode, message: String) -> CliError {
    CliError::failed(if message.is_empty() {
        code.to_string()
    } else {
        message
    })
}

pub async fn run(cli: Cli) -> Result<(), CliError> {
    // An explicitly chosen data directory (`--data`, else `MESHD_DATA`), if any.
    let requested = cli.data.clone().or_else(|| {
        std::env::var_os(meshd::paths::DATA_ENV)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
    });
    let data = cli.data.unwrap_or_else(meshd::paths::default_data_dir);
    match cli.command {
        Command::Run { service } => crate::run_daemon(&data, service).await,
        Command::Init { name } => init(&data, name, None).await,
        Command::Join { ticket, name } => {
            InviteTicket::from_text(&ticket)
                .map_err(|e| CliError::failed(format!("invalid invite ticket: {e}")))?;
            init(&data, name, Some(ticket)).await
        }
        Command::ResetIdentity { yes } => reset_identity(&data, yes).await,
        Command::Status { json } => {
            let r = call(&data, AdminRequest::Status).await?;
            let AdminResponse::Status(s) = r else {
                return Err(unexpected(&r));
            };
            print_view(json, &s, output::status_text);
            Ok(())
        }
        Command::Members { json } => {
            let members = members(&data).await?;
            print_view(json, &members, |m| output::members_text(m));
            Ok(())
        }
        Command::Services { json } => {
            let services = services(&data).await?;
            print_view(json, &services, |s| output::services_text(s));
            Ok(())
        }
        Command::Invite { client, ttl } => invite(&data, client, ttl).await,
        Command::Remove { id_prefix } => {
            let id =
                resolve_member(&members(&data).await?, &id_prefix).map_err(CliError::failed)?;
            call(&data, AdminRequest::Remove { id }).await?;
            say!("Removed {}.", id.short());
            Ok(())
        }
        Command::Probe {
            node,
            count,
            interval,
            timeout,
        } => probe(&data, node, count, interval, timeout).await,
        Command::Deploy(d) => deploy(&data, d).await,
        Command::SetPlacement { name, placement } => {
            let placement = placement_of(&data, &placement).await?;
            show_manifest(call(&data, AdminRequest::SetPlacement { name, placement }).await?)
        }
        Command::SetOutbound { name, outbound } => {
            show_manifest(call(&data, AdminRequest::SetOutbound { name, outbound }).await?)
        }
        Command::SetLimits { name, limits } => {
            if limits.memory_mb.is_none()
                && limits.timeout_ms.is_none()
                && limits.max_body.is_none()
            {
                return Err(CliError::usage(
                    "give at least one of --memory-mb, --timeout-ms, --max-body",
                ));
            }
            let current = services(&data)
                .await?
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| CliError::failed(format!("no service named {name:?}")))?
                .limits;
            let limits = limits_of(&limits, current);
            show_manifest(call(&data, AdminRequest::SetLimits { name, limits }).await?)
        }
        Command::Undeploy { name } => {
            call(&data, AdminRequest::Undeploy { name: name.clone() }).await?;
            say!("Undeployed {name}.");
            Ok(())
        }
        Command::Purge { name, yes } => {
            // Fail on an unreachable daemon before asking anything; the connection is not
            // held across the prompt.
            drop(connect(&data).await?);
            if !yes
                && !confirm(&format!(
                    "Delete all data of {name:?} on every node? This cannot be undone."
                ))
            {
                return Err(CliError::failed("not confirmed; nothing was purged"));
            }
            call(&data, AdminRequest::Purge { name: name.clone() }).await?;
            say!("Purged {name}.");
            Ok(())
        }
        Command::Config(ConfigCmd::Get { key, json }) => {
            let r = call(&data, AdminRequest::ConfigGet { key }).await?;
            let AdminResponse::Config(entries) = r else {
                return Err(unexpected(&r));
            };
            print_view(json, &entries, |e| {
                e.iter().map(|(k, v)| format!("{k} = {v}\n")).collect()
            });
            Ok(())
        }
        Command::Config(ConfigCmd::Set { key, value }) => {
            let r = call(&data, AdminRequest::ConfigSet { key, value }).await?;
            let AdminResponse::ConfigSet { restart_required } = r else {
                return Err(unexpected(&r));
            };
            if restart_required {
                say!("Saved. Restart meshd for this to take effect.");
            } else {
                say!("Saved.");
            }
            Ok(())
        }
        Command::Logs { lines, follow } => logs(&data, lines, follow).await,
        Command::Service(cmd) => tokio::task::spawn_blocking(move || service(requested, cmd))
            .await
            .map_err(|e| CliError::failed(e.to_string()))?,
    }
}

/// `meshd service …` (spec §6.4): no daemon involved; may wait for the service manager.
/// `requested` is an explicitly chosen data directory: install uses it (else the OS
/// default), uninstall checks it against the installed one.
fn service(requested: Option<PathBuf>, cmd: ServiceCmd) -> Result<(), CliError> {
    use meshd::service;
    let failed = |e: service::ServiceError| CliError::failed(e.to_string());
    match cmd {
        ServiceCmd::Install => {
            let data = requested.unwrap_or_else(service::default_data_dir);
            service::install(&data).map_err(failed)?;
            say!(
                "Installed and started the meshnode service (data directory {}).",
                data.display()
            );
            #[cfg(target_os = "linux")]
            if std::env::var("SUDO_USER").is_ok_and(|u| !u.is_empty() && u != "root") {
                say!("Log in again to use meshd's commands without sudo (group meshnode).");
            }
        }
        ServiceCmd::Uninstall { purge_data } => {
            match service::uninstall(requested.as_deref(), purge_data).map_err(failed)? {
                Some(purged) => say!(
                    "Removed the meshnode service and deleted {}.",
                    purged.display()
                ),
                None => say!("Removed the meshnode service; its data directory was kept."),
            }
        }
        ServiceCmd::Start => {
            service::start().map_err(failed)?;
            say!("Started the meshnode service.");
        }
        ServiceCmd::Stop => {
            service::stop().map_err(failed)?;
            say!("Stopped the meshnode service.");
        }
        ServiceCmd::Status => {
            let status = service::status().map_err(failed)?;
            say!("meshnode service: {status}");
        }
    }
    Ok(())
}

// --- talking to the daemon ---------------------------------------------------------------

async fn connect(data: &Path) -> Result<AdminClient, CliError> {
    Ok(AdminClient::connect(&endpoint_for(data)).await?)
}

/// One request on its own connection; an `Error` answer becomes a failure with the
/// daemon's message.
async fn call(data: &Path, req: AdminRequest) -> Result<AdminResponse, CliError> {
    let mut client = connect(data).await?;
    let resp = client.request(req).await?;
    ok_answer(resp)
}

fn ok_answer(resp: AdminResponse) -> Result<AdminResponse, CliError> {
    match resp {
        AdminResponse::Error { code, message } => Err(error_answer(code, message)),
        r => Ok(r),
    }
}

fn unexpected(r: &AdminResponse) -> CliError {
    CliError::failed(format!("unexpected answer from the daemon: {r:?}"))
}

async fn members(data: &Path) -> Result<Vec<MemberInfoView>, CliError> {
    match call(data, AdminRequest::Members).await? {
        AdminResponse::Members(m) => Ok(m),
        r => Err(unexpected(&r)),
    }
}

async fn services(data: &Path) -> Result<Vec<ServiceInfoView>, CliError> {
    match call(data, AdminRequest::Services).await? {
        AdminResponse::Services(s) => Ok(s),
        r => Err(unexpected(&r)),
    }
}

/// `--json`: the view as JSON; otherwise its text form.
fn print_view<T: Serialize + ?Sized>(json: bool, view: &T, text: impl FnOnce(&T) -> String) {
    if json {
        match serde_json::to_string_pretty(view) {
            Ok(s) => say!("{s}"),
            Err(e) => eprintln!("cannot render JSON: {e}"),
        }
    } else {
        say_raw!("{}", text(view));
    }
}

fn show_manifest(r: AdminResponse) -> Result<(), CliError> {
    match r {
        AdminResponse::Manifest(m) => {
            say!(
                "{} is now version {} ({}).",
                m.name,
                m.version,
                output::placement_text(&m.placement)
            );
            Ok(())
        }
        r => Err(unexpected(&r)),
    }
}

// --- init, join, reset-identity ------------------------------------------------------------

async fn init(data: &Path, name: String, ticket: Option<String>) -> Result<(), CliError> {
    match AdminClient::connect(&endpoint_for(data)).await {
        Ok(mut client) => {
            let req = match ticket {
                Some(ticket) => AdminRequest::Join { ticket, name },
                None => AdminRequest::Init { name },
            };
            ok_answer(client.request(req).await?)?;
            say!("Done.");
            Ok(())
        }
        Err(AdminError::NotRunning) => match ticket {
            Some(ticket) => local::join(data, &ticket, &name).await,
            None => local::init(data, &name).await,
        },
        Err(e) => Err(e.into()),
    }
}

async fn reset_identity(data: &Path, yes: bool) -> Result<(), CliError> {
    let through_daemon = match AdminClient::connect(&endpoint_for(data)).await {
        Ok(client) => {
            let state = client.daemon_state();
            if !matches!(state, DaemonState::Removed | DaemonState::Uninitialised) {
                return Err(CliError::failed("this node was not removed from its mesh"));
            }
            true
        }
        Err(AdminError::NotRunning) => false,
        Err(e) => return Err(e.into()),
    };
    // The connection above is closed: it is not held across the prompt.
    if !through_daemon {
        return local::reset_identity(data, yes).await;
    }
    if !yes
        && !confirm("Delete this node's identity? It cannot rejoin its mesh without a new invite.")
    {
        return Err(CliError::failed("not confirmed; nothing was reset"));
    }
    call(data, AdminRequest::ResetIdentity { confirm: true }).await?;
    say!("Identity reset. Run \"meshd init\" or \"meshd join\" to start over.");
    Ok(())
}

/// Asks on the terminal; anything but y / yes (or no terminal input) is no.
pub fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(n) if n > 0 => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        _ => false,
    }
}

// --- invite --------------------------------------------------------------------------------

async fn invite(data: &Path, client: bool, ttl: u32) -> Result<(), CliError> {
    let role = if client { Role::Client } else { Role::Host };
    let r = call(
        data,
        AdminRequest::Invite {
            role,
            ttl_secs: ttl,
        },
    )
    .await?;
    let AdminResponse::Ticket { text } = r else {
        return Err(unexpected(&r));
    };
    say!("{text}");
    match output::qr_text(&text) {
        Some(qr) => say!("\n{qr}"),
        None => eprintln!("(the ticket is too long for a QR code)"),
    }
    Ok(())
}

// --- members and nodes by prefix -------------------------------------------------------------

/// The one member whose id (lowercase hex) starts with `prefix`.
pub fn resolve_member(members: &[MemberInfoView], prefix: &str) -> Result<NodeId, String> {
    let p = prefix.trim().to_ascii_lowercase();
    if p.is_empty() {
        return Err("give a member id or a prefix of one (see \"meshd members\")".into());
    }
    let hits: Vec<&MemberInfoView> = members
        .iter()
        .filter(|m| output::node_hex(&m.id).starts_with(&p))
        .collect();
    match hits.as_slice() {
        [] => Err(format!("no member has an id starting with {prefix:?}")),
        [one] => Ok(one.id),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|m| format!("{} ({})", m.id.short(), m.name))
                .collect();
            Err(format!(
                "{prefix:?} is ambiguous, it matches {}; give more characters",
                list.join(", ")
            ))
        }
    }
}

async fn placement_of(data: &Path, args: &PlacementArgs) -> Result<Placement, CliError> {
    if let Some(tags) = &args.tags {
        return Ok(Placement::Tags(tags.clone()));
    }
    if let Some(nodes) = &args.nodes {
        let members = members(data).await?;
        let ids = nodes
            .iter()
            .map(|n| resolve_member(&members, n))
            .collect::<Result<Vec<_>, _>>()
            .map_err(CliError::failed)?;
        return Ok(Placement::Nodes(ids));
    }
    Ok(Placement::AllHosts)
}

fn limits_of(args: &LimitArgs, base: Limits) -> Limits {
    Limits {
        memory_mb: args.memory_mb.unwrap_or(base.memory_mb),
        request_timeout_ms: args.timeout_ms.unwrap_or(base.request_timeout_ms),
        max_body_bytes: args.max_body.unwrap_or(base.max_body_bytes),
    }
}

// --- deploy --------------------------------------------------------------------------------

async fn deploy(data: &Path, d: Deploy) -> Result<(), CliError> {
    match d {
        Deploy::Wasm {
            name,
            file,
            placement,
            outbound,
            limits,
        } => {
            let wasm = std::fs::read(&file)
                .map_err(|e| CliError::failed(format!("cannot read {}: {e}", file.display())))?;
            let opts = DeployOpts {
                limits: limits_of(&limits, Limits::default()),
                placement: placement_of(data, &placement).await?,
                outbound,
            };
            let r = call(data, AdminRequest::DeployWasm { name, wasm, opts }).await?;
            show_deployed(r)
        }
        Deploy::Static {
            name,
            dir,
            placement,
            limits,
        } => {
            let files = collect_files(&dir, &mut |msg| eprintln!("warning: {msg}"))?;
            if files.is_empty() {
                return Err(CliError::failed(format!(
                    "{} has no files to deploy",
                    dir.display()
                )));
            }
            let opts = DeployOpts {
                limits: limits_of(&limits, Limits::default()),
                placement: placement_of(data, &placement).await?,
                outbound: Vec::new(),
            };
            let mut client = connect(data).await?;
            let r = client.deploy_static(name, opts, files).await?;
            show_deployed(ok_answer(r)?)
        }
    }
}

fn show_deployed(r: AdminResponse) -> Result<(), CliError> {
    match r {
        AdminResponse::Manifest(m) => {
            say!(
                "Deployed {} version {} ({}).",
                m.name,
                m.version,
                output::placement_text(&m.placement)
            );
            Ok(())
        }
        r => Err(unexpected(&r)),
    }
}

/// Walks `root` into upload entries: `/`-separated paths relative to it, sorted. Symbolic
/// links (to files or directories) are not followed or uploaded; each is reported through
/// `warn`.
pub fn collect_files(
    root: &Path,
    warn: &mut dyn FnMut(String),
) -> Result<Vec<(FileMeta, PathBuf)>, CliError> {
    let meta = std::fs::metadata(root)
        .map_err(|e| CliError::failed(format!("cannot read {}: {e}", root.display())))?;
    if !meta.is_dir() {
        return Err(CliError::failed(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let mut out = Vec::new();
    walk(root, "", &mut out, warn)?;
    out.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    Ok(out)
}

fn walk(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(FileMeta, PathBuf)>,
    warn: &mut dyn FnMut(String),
) -> Result<(), CliError> {
    let read_err =
        |p: &Path, e: io::Error| CliError::failed(format!("cannot read {}: {e}", p.display()));
    for entry in std::fs::read_dir(dir).map_err(|e| read_err(dir, e))? {
        let entry = entry.map_err(|e| read_err(dir, e))?;
        let path = entry.path();
        let Ok(name) = entry.file_name().into_string() else {
            warn(format!(
                "skipping {}{}: its name is not valid UTF-8",
                prefix,
                entry.file_name().to_string_lossy()
            ));
            continue;
        };
        let rel = format!("{prefix}{name}");
        // `file_type` does not follow symbolic links.
        let ty = entry.file_type().map_err(|e| read_err(&path, e))?;
        if ty.is_symlink() {
            warn(format!("skipping symlink {rel}"));
        } else if ty.is_dir() {
            walk(&path, &format!("{rel}/"), out, warn)?;
        } else if ty.is_file() {
            let size = entry.metadata().map_err(|e| read_err(&path, e))?.len();
            out.push((FileMeta { path: rel, size }, path));
        } else {
            warn(format!("skipping {rel}: not a regular file"));
        }
    }
    Ok(())
}

// --- logs ----------------------------------------------------------------------------------

async fn logs(data: &Path, lines: u32, follow: bool) -> Result<(), CliError> {
    let mut client = connect(data).await?;
    let stream = client.logs_tail(lines, follow);
    tokio::pin!(stream);
    let mut stop = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(Ok(batch)) => {
                    for l in batch {
                        say!("{l}");
                    }
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            _ = &mut stop => return Ok(()),
        }
    }
}

// --- probe ---------------------------------------------------------------------------------

async fn probe(
    data: &Path,
    node: String,
    count: u32,
    interval_ms: u32,
    timeout_ms: u32,
) -> Result<(), CliError> {
    let mut client = connect(data).await?;
    let stream = client.probe(node, count, interval_ms, timeout_ms);
    tokio::pin!(stream);
    let mut stop = std::pin::pin!(tokio::signal::ctrl_c());
    let (mut ms, mut seen) = (Vec::new(), 0usize);
    let mut total = count as usize;
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(Ok(batch)) => {
                    for l in batch {
                        say!("{l}");
                        seen += 1;
                        ms.extend(output::probe_sample(&l));
                    }
                }
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            _ = &mut stop => {
                total = seen;
                break;
            }
        }
    }
    say!("{}", output::probe_summary(&ms, total));
    if ms.is_empty() && total > 0 {
        return Err(CliError {
            code: 2,
            message: "every sample failed".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mesh_proto::Role;

    use super::*;

    fn member(first: u8, second: u8, name: &str) -> MemberInfoView {
        let mut id = [0u8; 32];
        id[0] = first;
        id[1] = second;
        MemberInfoView {
            id: NodeId(id),
            name: name.into(),
            role: Role::Host,
            platform: "windows".into(),
            tags: vec![],
            is_self: false,
            effective: true,
            online: true,
            rtt_ms: None,
            last_seen_ms: None,
            lag: 0,
        }
    }

    #[test]
    fn remove_ambiguous_prefix_error() {
        let members = [member(0xab, 0x01, "pc"), member(0xab, 0x02, "laptop")];
        let err = resolve_member(&members, "ab").unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("pc") && err.contains("laptop"), "{err}");
        // A longer prefix is unique, in any case.
        assert_eq!(resolve_member(&members, "AB02").unwrap(), members[1].id);
        assert!(
            resolve_member(&members, "cd")
                .unwrap_err()
                .contains("no member")
        );
        assert!(resolve_member(&members, "").is_err());
    }

    #[test]
    fn collect_files_uses_relative_slash_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "hello").unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        std::fs::write(dir.path().join("a").join("b").join("c.txt"), "xy").unwrap();
        let mut warnings = Vec::new();
        let files = collect_files(dir.path(), &mut |w| warnings.push(w)).unwrap();
        let listed: Vec<(&str, u64)> = files
            .iter()
            .map(|(m, _)| (m.path.as_str(), m.size))
            .collect();
        assert_eq!(listed, [("a/b/c.txt", 2), ("index.html", 5)]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn collect_files_skips_symlinks_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "x").unwrap();
        let link = dir.path().join("link.txt");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&real, &link);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link);
        if let Err(e) = made {
            eprintln!("SKIPPED: cannot create a symlink here ({e})");
            return;
        }
        let mut warnings = Vec::new();
        let files = collect_files(dir.path(), &mut |w| warnings.push(w)).unwrap();
        let listed: Vec<&str> = files.iter().map(|(m, _)| m.path.as_str()).collect();
        assert_eq!(listed, ["real.txt"]);
        assert_eq!(warnings, ["skipping symlink link.txt"]);
    }

    #[test]
    fn collect_files_skips_symlinked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("a.txt"), "a").unwrap();
        let link = dir.path().join("linkdir");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&real, &link);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link);
        if let Err(e) = made {
            eprintln!("SKIPPED: cannot create a symlink here ({e})");
            return;
        }
        let mut warnings = Vec::new();
        let files = collect_files(dir.path(), &mut |w| warnings.push(w)).unwrap();
        let listed: Vec<&str> = files.iter().map(|(m, _)| m.path.as_str()).collect();
        assert_eq!(listed, ["real/a.txt"]);
        assert_eq!(warnings, ["skipping symlink linkdir"]);
    }

    #[cfg(unix)]
    #[test]
    fn collect_files_skips_non_utf8_names_with_a_warning() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "x").unwrap();
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
        if std::fs::write(dir.path().join(bad), "x").is_err() {
            return;
        }
        let mut warnings = Vec::new();
        let files = collect_files(dir.path(), &mut |w| warnings.push(w)).unwrap();
        assert_eq!(files.len(), 1);
        assert!(warnings[0].contains("not valid UTF-8"), "{warnings:?}");
    }
}
