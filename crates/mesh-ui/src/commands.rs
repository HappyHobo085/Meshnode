//! The request/response commands, shared by the desktop and Android shells.

use mesh_admin::{AdminRequest, AdminResponse, MAX_UPLOAD_FILE};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use ts_rs::TS;

use crate::dto::*;
use crate::error::{UiError, UiErrorCode};
use crate::source::FileSource;
use crate::streams::Streams;
use crate::transport::TransportFn;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    DesktopWindows,
    DesktopLinux,
    DesktopMacos,
    Android,
}

pub struct Ui {
    transport: TransportFn,
    platform: Platform,
    streams: Streams,
}

fn unexpected(what: &str, got: AdminResponse) -> UiError {
    UiError::internal(format!("unexpected reply to {what}: {got:?}"))
}

impl Ui {
    pub fn new(transport: TransportFn, platform: Platform) -> Ui {
        Ui {
            transport,
            platform,
            streams: Streams::new(),
        }
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    async fn call(&self, req: AdminRequest) -> Result<AdminResponse, UiError> {
        let transport = (self.transport)();
        match transport.request(req).await? {
            AdminResponse::Error { code, message } => Err((code, message).into()),
            other => Ok(other),
        }
    }

    async fn call_ok(&self, what: &str, req: AdminRequest) -> Result<(), UiError> {
        match self.call(req).await? {
            AdminResponse::Ok => Ok(()),
            other => Err(unexpected(what, other)),
        }
    }

    pub async fn status(&self) -> Result<StatusDto, UiError> {
        match self.call(AdminRequest::Status).await? {
            AdminResponse::Status(s) => Ok(s.into()),
            other => Err(unexpected("status", other)),
        }
    }

    pub async fn members(&self) -> Result<Vec<MemberDto>, UiError> {
        match self.call(AdminRequest::Members).await? {
            AdminResponse::Members(m) => Ok(m.into_iter().map(Into::into).collect()),
            other => Err(unexpected("members", other)),
        }
    }

    pub async fn services(&self) -> Result<Vec<ServiceDto>, UiError> {
        match self.call(AdminRequest::Services).await? {
            AdminResponse::Services(s) => Ok(s.into_iter().map(Into::into).collect()),
            other => Err(unexpected("services", other)),
        }
    }

    pub async fn init(&self, name: String) -> Result<StatusDto, UiError> {
        self.call_ok("init", AdminRequest::Init { name }).await?;
        self.status().await
    }

    pub async fn invite(&self, role: RoleDto, ttl_s: u32) -> Result<InviteDto, UiError> {
        if !matches!(ttl_s, 300 | 900 | 3600) {
            return Err(UiError::invalid(
                "an invite lasts 5 minutes, 15 minutes or 1 hour",
            ));
        }
        let req = AdminRequest::Invite {
            role: role.into(),
            ttl_secs: ttl_s,
        };
        match self.call(req).await? {
            AdminResponse::Ticket { text } => {
                let ticket = mesh_proto::InviteTicket::from_text(&text)
                    .map_err(|e| UiError::internal(format!("daemon made a bad invite: {e}")))?;
                Ok(InviteDto {
                    qr_svg: crate::qr::qr_svg(&text),
                    text,
                    expires_at_ms: ticket.expires_ms,
                })
            }
            other => Err(unexpected("invite", other)),
        }
    }

    pub async fn join(&self, ticket: String, name: String) -> Result<StatusDto, UiError> {
        // The ticket alphabet has no whitespace: a ticket wrapped by mail, chat or a terminal
        // is joined back together.
        let ticket: String = ticket.split_whitespace().collect();
        if mesh_proto::InviteTicket::from_text(&ticket).is_err() {
            return Err(UiError {
                code: UiErrorCode::InvalidArgument,
                message: "That isn't a Meshnode invite".into(),
                hint: Some("Scan or paste the invite shown on the other device.".into()),
                reason: None,
            });
        }
        self.call_ok("join", AdminRequest::Join { ticket, name })
            .await?;
        self.status().await
    }

    pub async fn remove(&self, id_hex: String) -> Result<(), UiError> {
        let id = parse_node_id(&id_hex)?;
        self.call_ok("remove", AdminRequest::Remove { id }).await
    }

    pub async fn set_info(
        &self,
        name: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<(), UiError> {
        self.call_ok("set_info", AdminRequest::SetInfo { name, tags })
            .await
    }

    pub async fn set_tags_of(&self, id_hex: String, tags: Vec<String>) -> Result<(), UiError> {
        let id = parse_node_id(&id_hex)?;
        self.call_ok("set_tags_of", AdminRequest::SetTagsOf { id, tags })
            .await
    }

    pub async fn deploy_wasm(
        &self,
        name: String,
        wasm: Vec<u8>,
        opts: DeployOptsDto,
    ) -> Result<ServiceDto, UiError> {
        let opts = opts.into_opts()?;
        match self
            .call(AdminRequest::DeployWasm {
                name: name.clone(),
                wasm,
                opts,
            })
            .await?
        {
            AdminResponse::Manifest(_) | AdminResponse::Ok => {}
            other => return Err(unexpected("deploy_wasm", other)),
        }
        self.service_named(&name).await
    }

    /// Deploys the file at `path` as a wasm service (at most 64 MiB).
    pub async fn deploy_wasm_file(
        &self,
        name: String,
        path: std::path::PathBuf,
        opts: DeployOptsDto,
    ) -> Result<ServiceDto, UiError> {
        let io =
            |e: std::io::Error| UiError::new(UiErrorCode::Io, format!("{}: {e}", path.display()));
        let len = std::fs::metadata(&path).map_err(io)?.len();
        if len > MAX_UPLOAD_FILE {
            return Err(UiError::invalid(format!(
                "{} is larger than 64 MiB",
                path.display()
            )));
        }
        let wasm = std::fs::read(&path).map_err(io)?;
        self.deploy_wasm(name, wasm, opts).await
    }

    /// Deploys a static site from `src`, sending cumulative bytes read to `progress`.
    pub async fn deploy_static(
        &self,
        name: String,
        opts: DeployOptsDto,
        src: Box<dyn FileSource>,
        progress: mpsc::Sender<u64>,
    ) -> Result<ServiceDto, UiError> {
        let opts = opts.into_opts()?;
        let transport = (self.transport)();
        match transport
            .deploy_static(name.clone(), opts, src, progress)
            .await?
        {
            AdminResponse::Error { code, message } => return Err((code, message).into()),
            AdminResponse::Manifest(_) | AdminResponse::Ok => {}
            other => return Err(unexpected("deploy_static", other)),
        }
        self.service_named(&name).await
    }

    pub(crate) async fn service_named(&self, name: &str) -> Result<ServiceDto, UiError> {
        self.services()
            .await?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| UiError::new(UiErrorCode::NotFound, format!("service {name} not found")))
    }

    pub async fn undeploy(&self, name: String) -> Result<(), UiError> {
        self.call_ok("undeploy", AdminRequest::Undeploy { name })
            .await
    }

    pub async fn purge(&self, name: String) -> Result<(), UiError> {
        self.call_ok("purge", AdminRequest::Purge { name }).await
    }

    pub async fn set_placement(
        &self,
        name: String,
        placement: PlacementDto,
    ) -> Result<(), UiError> {
        let placement = placement.into_placement()?;
        self.call_ok(
            "set_placement",
            AdminRequest::SetPlacement { name, placement },
        )
        .await
    }

    pub async fn set_outbound(&self, name: String, outbound: Vec<String>) -> Result<(), UiError> {
        self.call_ok("set_outbound", AdminRequest::SetOutbound { name, outbound })
            .await
    }

    pub async fn set_limits(&self, name: String, limits: LimitsDto) -> Result<(), UiError> {
        self.call_ok(
            "set_limits",
            AdminRequest::SetLimits {
                name,
                limits: limits.into(),
            },
        )
        .await
    }

    pub async fn config_get(&self) -> Result<Vec<ConfigEntry>, UiError> {
        match self.call(AdminRequest::ConfigGet { key: None }).await? {
            AdminResponse::Config(c) => Ok(c
                .into_iter()
                .map(|(key, value)| ConfigEntry { key, value })
                .collect()),
            other => Err(unexpected("config_get", other)),
        }
    }

    pub async fn config_set(&self, key: String, value: String) -> Result<ConfigSetDto, UiError> {
        match self.call(AdminRequest::ConfigSet { key, value }).await? {
            AdminResponse::ConfigSet { restart_required } => Ok(ConfigSetDto { restart_required }),
            other => Err(unexpected("config_set", other)),
        }
    }

    pub async fn reset_identity(&self) -> Result<StatusDto, UiError> {
        self.call_ok(
            "reset_identity",
            AdminRequest::ResetIdentity { confirm: true },
        )
        .await?;
        self.status().await
    }

    /// Follows the daemon log: the last `lines` lines, then new ones, until `tx` closes.
    pub async fn logs_follow(&self, lines: u32, tx: mpsc::Sender<String>) -> Result<(), UiError> {
        let _slot = self.streams.begin()?;
        let transport = (self.transport)();
        transport
            .stream(
                AdminRequest::LogsTail {
                    lines,
                    follow: true,
                },
                tx,
            )
            .await
    }

    /// Pings a member `count` times; each sample goes to `tx` as it arrives.
    pub async fn probe(
        &self,
        node_prefix: String,
        count: u32,
        interval_ms: u32,
        timeout_ms: u32,
        tx: mpsc::Sender<ProbeSampleDto>,
    ) -> Result<(), UiError> {
        let _slot = self.streams.begin()?;
        let transport = (self.transport)();
        let req = AdminRequest::Probe {
            node: node_prefix,
            count,
            interval_ms,
            timeout_ms,
        };
        let (line_tx, mut line_rx) = mpsc::channel::<String>(16);
        let forward = async {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    line = line_rx.recv() => match line {
                        None => break,
                        Some(line) => {
                            if let Some(sample) = parse_probe_line(&line)
                                && tx.send(sample).await.is_err()
                            {
                                break;
                            }
                        }
                    },
                }
            }
            // Closing the line channel stops the transport.
            drop(line_rx);
        };
        let (result, ()) = tokio::join!(transport.stream(req, line_tx), forward);
        result
    }
}
