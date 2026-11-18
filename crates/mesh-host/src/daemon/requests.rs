//! Answers to admin requests: the [`AdminBackend`] impl of [`Daemon`].

use std::io;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mesh_admin::{
    AdminBackend, AdminRequest, AdminResponse, DaemonState, ErrorCode, LocalStateView,
    MemberInfoView, RequestIo, ServiceInfoView, ServiceKindView, StatusInfo, UploadError,
};
use mesh_core::{MemberStatus, Node, NodeError, NodeHandle};
use mesh_proto::{InviteTicket, NodeId, RefuseReason, ServiceKind, ServiceManifest};
use mesh_services::{LocalStatus, ServiceStatus, Services, ServicesError};
use tokio::time::Instant;
use tracing::{info, warn};

use super::{
    Backend, Daemon, DaemonError, How, KEPT_ON_RESET, Running, SHUTDOWN_LIMIT, Stopped, lock,
};
use crate::config::{ConfigError, HostConfig};
use crate::paths::{logs_dir, tmp_dir};

#[async_trait]
impl AdminBackend for Daemon {
    fn state(&self) -> DaemonState {
        Backend::state_of(&self.inner)
    }

    async fn handle(&self, req: AdminRequest, io: &mut RequestIo<'_>) -> AdminResponse {
        use AdminRequest as R;
        let inner = &*self.inner;
        match req {
            R::Status => {
                // Lock order: `run`, then `cfg`.
                let run = inner.run.read().await;
                let cfg = inner.cfg.lock().await;
                AdminResponse::Status(self.status_info(run.as_ref(), Some(&cfg)))
            }
            R::Init { name } => self.init(name, None).await,
            R::Join { ticket, name } => match InviteTicket::from_text(&ticket) {
                Ok(t) => self.init(name, Some(t)).await,
                Err(e) => AdminResponse::error(ErrorCode::InvalidArgument, e.to_string()),
            },
            R::ResetIdentity { confirm: true } => self.reset_identity().await,
            R::ResetIdentity { confirm: false } => AdminResponse::error(
                ErrorCode::InvalidArgument,
                "reset-identity must be confirmed",
            ),
            R::ConfigGet { key } => {
                let cfg = inner.cfg.lock().await;
                match key {
                    None => AdminResponse::Config(cfg.entries()),
                    Some(k) => match cfg.get(&k) {
                        Some(v) => AdminResponse::Config(vec![(k, v)]),
                        None => AdminResponse::error(
                            ErrorCode::InvalidArgument,
                            ConfigError::UnknownKey(k).to_string(),
                        ),
                    },
                }
            }
            R::ConfigSet { key, value } => self.config_set(&key, &value).await,
            R::Probe {
                node,
                count,
                interval_ms,
                timeout_ms,
            } => {
                // Only the node handle is kept: the run lock is not held for the probe.
                let (handle, peer) = {
                    let run = match inner.running_or(inner.run.read().await) {
                        Ok(r) => r,
                        Err(e) => return e.into(),
                    };
                    match resolve_member(&run.node, &node) {
                        Ok(peer) => (run.node.handle(), peer),
                        Err(e) => return e.into(),
                    }
                };
                let limit = Duration::from_millis(timeout_ms.into());
                let every = Duration::from_millis(interval_ms.into());
                probe(&handle, peer, count, every, limit, io).await
            }
            R::LogsTail { lines, follow } => {
                crate::logging::tail(&logs_dir(&inner.data_dir), lines, follow, io).await
            }
            other => {
                let run = match inner.running_or(inner.run.read().await) {
                    Ok(r) => r,
                    Err(e) => return e.into(),
                };
                self.node_request(&run, other, io)
                    .await
                    .unwrap_or_else(Into::into)
            }
        }
    }
}

/// An error answer: its code and message.
pub(super) struct Fail(pub(super) ErrorCode, pub(super) String);

impl From<Fail> for AdminResponse {
    fn from(Fail(code, message): Fail) -> Self {
        AdminResponse::Error { code, message }
    }
}

type Answer = Result<AdminResponse, Fail>;

impl Daemon {
    /// The answer to `Status`, from the running node (if any) and `meshd.toml` (if
    /// available: it is the fallback for this node's name and tags).
    pub(super) fn status_info(
        &self,
        run: Option<&Running>,
        cfg: Option<&HostConfig>,
    ) -> StatusInfo {
        let inner = &*self.inner;
        let mut s = StatusInfo {
            state: self.state(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            data_dir: inner.data_dir.display().to_string(),
            node_id: None,
            mesh_id: None,
            name: None,
            role: None,
            tags: Vec::new(),
            members_total: 0,
            members_online: 0,
            services: 0,
            gateway: None,
            ports: Vec::new(),
        };
        if let Some(run) = run {
            let me = run.node.node_id();
            let members = run.node.members();
            s.node_id = Some(me);
            s.mesh_id = Some(run.node.mesh());
            let own = members.iter().find(|m| m.id == me);
            s.role = own.map(|m| m.role);
            // Until the membership shows this node's info, `meshd.toml` has it.
            match own.and_then(|m| m.info.as_ref()) {
                Some(i) => (s.name, s.tags) = (Some(i.name.clone()), i.tags.clone()),
                None => {
                    if let Some(cfg) = cfg {
                        (s.name, s.tags) = (Some(cfg.name.clone()), cfg.tags.clone());
                    }
                }
            }
            s.members_total = members.len() as u32;
            s.members_online = members.iter().filter(|m| m.online).count() as u32;
            s.services = run.services.services().len() as u32;
            for (addr, svc) in lock(&inner.gateway_addrs).iter() {
                match svc {
                    None => s.gateway = Some(addr.to_string()),
                    Some(svc) => s.ports.push((addr.port(), svc.clone())),
                }
            }
        }
        s
    }

    /// `Init` (no ticket) or `Join`: starts the node in place and records the name.
    async fn init(&self, name: String, ticket: Option<InviteTicket>) -> AdminResponse {
        let inner = &*self.inner;
        let _t = inner.transition.lock().await;
        if inner.run.read().await.is_some() {
            return AdminResponse::error(
                ErrorCode::WrongState,
                "this node already belongs to a mesh",
            );
        }
        let mut cfg = inner.cfg.lock().await.clone();
        if let Err(e) = cfg.set("name", &name) {
            return AdminResponse::error(ErrorCode::InvalidArgument, e.to_string());
        }
        let how = ticket.map_or(How::Create, How::Join);
        let gateway = match inner.start_node(&cfg, how).await {
            Ok(g) => g,
            Err(DaemonError::Node(e)) => return node_error(e).into(),
            Err(e) => return AdminResponse::error(ErrorCode::Internal, e.to_string()),
        };
        if let Err(Fail(code, e)) = self.remember_info(Some(name), None).await {
            return AdminResponse::error(code, format!("the node runs, but {e}"));
        }
        match gateway {
            None => AdminResponse::Ok,
            Some(e) => AdminResponse::error(
                ErrorCode::Internal,
                format!("the node runs, but the gateway did not start: {e}"),
            ),
        }
    }

    /// Stops the node and deletes everything in the data directory but `meshd.toml` and
    /// the logs (the server allows this only when removed or uninitialised).
    async fn reset_identity(&self) -> AdminResponse {
        self.reset_identity_by(Instant::now() + SHUTDOWN_LIMIT)
            .await
    }

    pub(super) async fn reset_identity_by(&self, deadline: Instant) -> AdminResponse {
        let inner = &*self.inner;
        let _t = inner.transition.lock().await;
        if !matches!(
            self.state(),
            DaemonState::Removed | DaemonState::Uninitialised
        ) {
            return AdminResponse::error(ErrorCode::WrongState, "this node was not removed");
        }
        let stopped = inner.stop_node(deadline).await;
        if stopped == Stopped::Busy {
            // The node may still be running: nothing is deleted.
            return AdminResponse::error(
                ErrorCode::Internal,
                "reset refused: the node is busy and could not be stopped; nothing was deleted, try again",
            );
        }
        let released = stopped == Stopped::Clean;
        let dir = inner.data_dir.clone();
        let wiped = tokio::task::spawn_blocking(move || wipe(&dir))
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)));
        match wiped {
            Ok(()) => {
                info!("identity reset");
                AdminResponse::Ok
            }
            Err(e) if !released => AdminResponse::error(
                ErrorCode::Internal,
                format!(
                    "reset failed: {e}; the services were still in use. Restart meshd, then retry reset-identity"
                ),
            ),
            Err(e) => AdminResponse::error(ErrorCode::Internal, format!("reset failed: {e}")),
        }
    }

    async fn config_set(&self, key: &str, value: &str) -> AdminResponse {
        let inner = &*self.inner;
        // Lock order everywhere: `transition`, then `run`, then `cfg`. A name or tags change
        // waits for an Init or Join in progress, so the node started there takes it too.
        let _t = match key {
            "name" | "tags" => Some(inner.transition.lock().await),
            _ => None,
        };
        let run = inner.run.read().await;
        let mut cfg = inner.cfg.lock().await;
        let mut new = cfg.clone();
        let restart_required = match new.set(key, value) {
            Ok(r) => r,
            Err(e) => return AdminResponse::error(ErrorCode::InvalidArgument, e.to_string()),
        };
        if !restart_required && let Some(run) = run.as_ref() {
            // Both fields, from `meshd.toml`: a lagging membership snapshot must not
            // write back an older name or tags.
            if let Err(e) = run
                .node
                .set_info(Some(new.name.clone()), Some(new.tags.clone()))
                .await
            {
                return node_error(e).into();
            }
        }
        if let Err(e) = save_config(&new, &inner.data_dir).await {
            return AdminResponse::error(ErrorCode::Internal, e);
        }
        *cfg = new;
        AdminResponse::ConfigSet { restart_required }
    }

    /// A request that needs the running node.
    async fn node_request(
        &self,
        run: &Running,
        req: AdminRequest,
        io: &mut RequestIo<'_>,
    ) -> Answer {
        use AdminRequest as R;
        let (node, svcs) = (&run.node, &run.services);
        Ok(match req {
            R::Members => {
                let me = node.node_id();
                let mut views: Vec<_> = node
                    .members()
                    .into_iter()
                    .map(|m| member_view(m, me))
                    .collect();
                // Until the membership shows this node's info, `meshd.toml` has it.
                if let Some(v) = views
                    .iter_mut()
                    .find(|v| v.is_self && v.platform.is_empty())
                {
                    let cfg = self.inner.cfg.lock().await;
                    (v.name, v.tags) = (cfg.name.clone(), cfg.tags.clone());
                    v.platform = std::env::consts::OS.into();
                }
                AdminResponse::Members(views)
            }
            R::Services => {
                AdminResponse::Services(svcs.services().into_iter().map(service_view).collect())
            }
            R::Invite { role, ttl_secs } => {
                if ttl_secs == 0 {
                    return Err(Fail(
                        ErrorCode::InvalidArgument,
                        "the ttl must be positive".into(),
                    ));
                }
                let t = node
                    .invite(role, Duration::from_secs(ttl_secs.into()))
                    .await
                    .map_err(node_error)?;
                AdminResponse::Ticket { text: t.to_text() }
            }
            R::Remove { id } => {
                is_member(node, id)?;
                node.remove(id).await.map_err(node_error)?;
                AdminResponse::Ok
            }
            R::SetInfo { name, tags } => {
                // Both fields, the unchanged one from `meshd.toml`, so that a lagging
                // membership snapshot cannot write back an older value.
                // The cfg lock is held across the node call and the save, as in
                // `config_set`.
                let mut cfg = self.inner.cfg.lock().await;
                let mut new = cfg.clone();
                if let Some(n) = name {
                    new.name = n;
                }
                if let Some(t) = tags {
                    new.tags = t;
                }
                node.set_info(Some(new.name.clone()), Some(new.tags.clone()))
                    .await
                    .map_err(node_error)?;
                self.save_info(&mut cfg, new).await?;
                AdminResponse::Ok
            }
            R::SetTagsOf { id, tags } => {
                is_member(node, id)?;
                if id == node.node_id() {
                    // Through `set_info` with both fields (see `SetInfo`).
                    let mut cfg = self.inner.cfg.lock().await;
                    let mut new = cfg.clone();
                    new.tags = tags;
                    node.set_info(Some(new.name.clone()), Some(new.tags.clone()))
                        .await
                        .map_err(node_error)?;
                    self.save_info(&mut cfg, new).await?;
                } else {
                    node.set_tags_of(id, tags).await.map_err(node_error)?;
                }
                AdminResponse::Ok
            }
            R::DeclareFresh => {
                node.declare_fresh().await.map_err(node_error)?;
                AdminResponse::Ok
            }
            R::DeployWasm { name, wasm, opts } => manifest(
                svcs.deploy_wasm(&name, Bytes::from(wasm), services_opts(opts))
                    .await,
            )?,
            R::DeployStatic { name, opts, .. } => self.deploy_static(svcs, &name, opts, io).await?,
            R::Undeploy { name } => {
                svcs.undeploy(&name).await.map_err(services_error)?;
                AdminResponse::Ok
            }
            R::Purge { name } => {
                svcs.purge(&name).await.map_err(services_error)?;
                AdminResponse::Ok
            }
            R::SetPlacement { name, placement } => {
                manifest(svcs.set_placement(&name, placement).await)?
            }
            R::SetOutbound { name, outbound } => {
                manifest(svcs.set_outbound(&name, outbound).await)?
            }
            R::SetLimits { name, limits } => manifest(svcs.set_limits(&name, limits).await)?,
            R::Status
            | R::Init { .. }
            | R::Join { .. }
            | R::ResetIdentity { .. }
            | R::ConfigGet { .. }
            | R::ConfigSet { .. }
            | R::Probe { .. }
            | R::LogsTail { .. } => unreachable!("answered without the node"),
        })
    }

    /// Writes this node's own name and tags to `meshd.toml` after the node took them.
    async fn remember_info(
        &self,
        name: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<(), Fail> {
        let mut cfg = self.inner.cfg.lock().await;
        let mut new = cfg.clone();
        if let Some(n) = name {
            new.name = n;
        }
        if let Some(t) = tags {
            new.tags = t;
        }
        self.save_info(&mut cfg, new).await
    }

    /// Saves `new` to `meshd.toml` and installs it into the locked `cfg`.
    async fn save_info(&self, cfg: &mut HostConfig, new: HostConfig) -> Result<(), Fail> {
        save_config(&new, &self.inner.data_dir)
            .await
            .map_err(|e| Fail(ErrorCode::Internal, e))?;
        *cfg = new;
        Ok(())
    }

    /// Receives the uploaded files into a fresh directory under `<data>/tmp`, deploys
    /// them, and deletes the directory.
    async fn deploy_static(
        &self,
        svcs: &Services,
        name: &str,
        opts: mesh_admin::DeployOpts,
        io: &mut RequestIo<'_>,
    ) -> Answer {
        let dir =
            tmp_dir(&self.inner.data_dir).join(format!("upload-{:016x}", rand::random::<u64>()));
        let result = async {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| Fail(ErrorCode::Internal, e.to_string()))?;
            io.save_files(&dir).await.map_err(|e| match e {
                UploadError::Invalid(m) => Fail(ErrorCode::InvalidArgument, m),
                e => Fail(ErrorCode::Internal, e.to_string()),
            })?;
            manifest(svcs.deploy_static(name, &dir, services_opts(opts)).await)
        }
        .await;
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("cannot remove {}: {e}", dir.display());
        }
        result
    }
}

/// Writes `meshd.toml` on a blocking thread.
pub(super) async fn save_config(cfg: &HostConfig, dir: &Path) -> Result<(), String> {
    let (cfg, dir) = (cfg.clone(), dir.to_path_buf());
    match tokio::task::spawn_blocking(move || cfg.save(&dir)).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Deletes every entry of `dir` except [`KEPT_ON_RESET`]. Retries briefly: on Windows a
/// file closed a moment ago may still be in use.
pub(super) fn wipe(dir: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        let r = (|| {
            for e in std::fs::read_dir(dir)? {
                let e = e?;
                if KEPT_ON_RESET.iter().any(|k| e.file_name() == **k) {
                    continue;
                }
                if e.file_type()?.is_dir() {
                    std::fs::remove_dir_all(e.path())?;
                } else {
                    std::fs::remove_file(e.path())?;
                }
            }
            Ok(())
        })();
        match r {
            Err(_) if attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

/// The one member whose id (lowercase hex) starts with `prefix`; the rule of
/// `meshd remove`.
fn resolve_member(node: &Node, prefix: &str) -> Result<NodeId, Fail> {
    let p = prefix.trim().to_ascii_lowercase();
    let bad = |m: String| Fail(ErrorCode::InvalidArgument, m);
    if p.is_empty() {
        return Err(bad(
            "give a member id or a prefix of one (see \"meshd members\")".into(),
        ));
    }
    let members = node.members();
    let hits: Vec<_> = members
        .iter()
        .filter(|m| hex::encode(m.id.0).starts_with(&p))
        .collect();
    match hits.as_slice() {
        [] => Err(Fail(
            ErrorCode::NotFound,
            format!("no member has an id starting with {prefix:?}"),
        )),
        [one] => Ok(one.id),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|m| {
                    let name = m.info.as_ref().map_or("", |i| i.name.as_str());
                    format!("{} ({name})", m.id.short())
                })
                .collect();
            Err(bad(format!(
                "{prefix:?} is ambiguous, it matches {}; give more characters",
                list.join(", ")
            )))
        }
    }
}

/// Answers `Probe`: `count` pings to `peer`, one line each, until the client leaves.
async fn probe(
    node: &NodeHandle,
    peer: NodeId,
    count: u32,
    every: Duration,
    limit: Duration,
    io: &mut RequestIo<'_>,
) -> AdminResponse {
    for seq in 1..=count {
        if seq > 1 {
            tokio::select! {
                _ = io.client_gone() => return AdminResponse::Ok,
                _ = tokio::time::sleep(every) => {}
            }
        }
        let sample = tokio::select! {
            _ = io.client_gone() => return AdminResponse::Ok,
            r = mesh_gateway::ping(node, peer, limit) => r,
        };
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let line = match sample {
            Ok(t) => format!(
                "seq={seq} ms={:.1} connect={:.1}",
                ms(t.total),
                ms(t.connect)
            ),
            Err(e) => format!("seq={seq} error={e}"),
        };
        if io.send_lines(vec![line]).await.is_err() {
            return AdminResponse::Ok;
        }
    }
    AdminResponse::Ok
}

fn is_member(node: &Node, id: NodeId) -> Result<(), Fail> {
    if node.members().iter().any(|m| m.id == id) {
        Ok(())
    } else {
        let message = format!("{} is not a member", hex::encode(id.0));
        Err(Fail(ErrorCode::NotFound, message))
    }
}

fn services_opts(o: mesh_admin::DeployOpts) -> mesh_services::DeployOpts {
    mesh_services::DeployOpts {
        limits: o.limits,
        placement: o.placement,
        outbound: o.outbound,
    }
}

fn manifest(r: Result<ServiceManifest, ServicesError>) -> Answer {
    r.map(AdminResponse::Manifest).map_err(services_error)
}

fn member_view(m: MemberStatus, me: NodeId) -> MemberInfoView {
    let (name, platform, tags) = m
        .info
        .map(|i| (i.name, i.platform, i.tags))
        .unwrap_or_default();
    MemberInfoView {
        id: m.id,
        name,
        role: m.role,
        platform,
        tags,
        is_self: m.id == me,
        effective: m.effective,
        online: m.online,
        rtt_ms: m.rtt_ms,
        last_seen_ms: m.last_seen_ms,
        lag: m.lag,
        version: m.version.map(|v| v.product).filter(|p| !p.is_empty()),
        incompatible: m.incompatible,
    }
}

fn service_view(s: ServiceStatus) -> ServiceInfoView {
    let m = s.manifest;
    ServiceInfoView {
        kind: match m.kind {
            ServiceKind::Wasm { .. } => ServiceKindView::Wasm,
            ServiceKind::Static { .. } => ServiceKindView::Static,
        },
        name: m.name,
        version: m.version,
        placement: m.placement,
        placed_hosts: s.placed_hosts,
        local: match s.local {
            LocalStatus::NotPlaced => LocalStateView::NotPlaced,
            LocalStatus::Fetching => LocalStateView::Fetching,
            LocalStatus::Ready { version } => LocalStateView::Ready { version },
            LocalStatus::Failed { error, .. } => LocalStateView::Failed { error },
        },
        limits: m.limits,
        outbound: m.outbound,
        data_bytes: s.data_bytes,
    }
}

fn node_error(e: NodeError) -> Fail {
    let code = match &e {
        NodeError::JoinRefused(r) => ErrorCode::JoinRefused(*r),
        NodeError::InviterUnreachable => ErrorCode::JoinRefused(RefuseReason::InviterUnavailable),
        NodeError::LastHost => ErrorCode::LastHost,
        NodeError::NotEffective
        | NodeError::Removed
        | NodeError::Rebootstrapping
        | NodeError::NeverSynced
        | NodeError::AlreadyInMesh
        | NodeError::NotInitialised
        | NodeError::WrongMesh => ErrorCode::WrongState,
        NodeError::Proto(_) => ErrorCode::InvalidArgument,
        NodeError::ShuttingDown => ErrorCode::NotRunning,
        _ => ErrorCode::Internal,
    };
    Fail(code, e.to_string())
}

fn services_error(e: ServicesError) -> Fail {
    match e {
        ServicesError::Node(e) => node_error(e),
        ServicesError::InvalidName
        | ServicesError::InvalidManifest(_)
        | ServicesError::InvalidComponent(_) => Fail(ErrorCode::InvalidArgument, e.to_string()),
        ServicesError::NotFound => Fail(ErrorCode::NotFound, e.to_string()),
        ServicesError::NotFullReplica(_) => Fail(ErrorCode::WrongState, e.to_string()),
        e => Fail(ErrorCode::Internal, e.to_string()),
    }
}
