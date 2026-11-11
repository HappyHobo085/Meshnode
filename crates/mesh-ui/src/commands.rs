//! The request/response commands, shared by the desktop and Android shells.

use mesh_admin::{AdminRequest, AdminResponse};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::dto::*;
use crate::error::{UiError, UiErrorCode};
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
}

fn unexpected(what: &str, got: AdminResponse) -> UiError {
    UiError::internal(format!("unexpected reply to {what}: {got:?}"))
}

impl Ui {
    pub fn new(transport: TransportFn, platform: Platform) -> Ui {
        Ui {
            transport,
            platform,
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

    pub async fn join(&self, ticket: String, name: String) -> Result<StatusDto, UiError> {
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
}
