//! `<data>/meshd.toml` (spec §6.1): load, save, and `config get` / `config set`.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use mesh_core::RelayChoice;
use mesh_proto::{MemberInfo, valid_service_name};
use serde::{Deserialize, Serialize};

use crate::power::PowerPolicy;

/// The config file's name inside the data directory.
pub const CONFIG_FILE: &str = "meshd.toml";

/// Every key `config get` lists and `config set` accepts, in listing order.
pub const KEYS: [&str; 9] = [
    "name",
    "tags",
    "relay",
    "discovery",
    "log_level",
    "gateway.listen",
    "gateway.ports",
    "services.max_memory_mb",
    "admin.windows_user_sid",
];

const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];
const PORTS: std::ops::RangeInclusive<u16> = 1024..=65535;
const MEMORY_MB: std::ops::RangeInclusive<u32> = 64..=65536;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown config key {0:?}")]
    UnknownKey(String),
    #[error("invalid value for {key}: {message}")]
    Invalid { key: String, message: String },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The longest `[android] periodic_wake_s`: 10 min, well inside the 15-min alarm.
const MAX_PERIODIC_WAKE_S: u32 = 600;

fn invalid(key: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        message: message.into(),
    }
}

/// The contents of `meshd.toml`. Missing fields take their defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    /// This node's member name (set by `init` / `join`).
    pub name: String,
    pub tags: Vec<String>,
    /// `"default"`, `"disabled"` or an `https://` relay URL.
    pub relay: String,
    pub discovery: bool,
    /// `trace` … `error`; `RUST_LOG` overrides it.
    pub log_level: String,
    pub gateway: GatewaySection,
    pub services: ServicesSection,
    pub admin: AdminSection,
    /// Android only (spec §5.3); left out of the file while it has the defaults.
    #[serde(skip_serializing_if = "AndroidConfig::is_default")]
    pub android: AndroidConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewaySection {
    pub listen: SocketAddr,
    /// Mapped port to the service it serves.
    #[serde(with = "port_map")]
    pub ports: BTreeMap<u16, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServicesSection {
    /// The WASM memory budget; unset is half of physical RAM, capped at 4096.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminSection {
    /// Windows: a user SID also allowed on the admin pipe (set by `service install`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_user_sid: Option<String>,
}

/// `[android]`: the power policy and the optional wake lock on each 15-min alarm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AndroidConfig {
    pub power: PowerPolicy,
    /// Seconds to hold a wake lock on each wall-clock alarm (`Balanced`); 0 is off.
    pub periodic_wake_s: u32,
}

impl AndroidConfig {
    fn is_default(&self) -> bool {
        *self == AndroidConfig::default()
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            name: String::new(),
            tags: Vec::new(),
            relay: "default".into(),
            discovery: true,
            log_level: "info".into(),
            gateway: GatewaySection::default(),
            services: ServicesSection::default(),
            admin: AdminSection::default(),
            android: AndroidConfig::default(),
        }
    }
}

impl Default for GatewaySection {
    fn default() -> Self {
        GatewaySection {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7777),
            ports: BTreeMap::from([(7801, "sync".to_string())]),
        }
    }
}

/// TOML keys are strings: the port map is stored as `{ "7801" = "sync" }`.
mod port_map {
    use std::collections::BTreeMap;

    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(m: &BTreeMap<u16, String>, s: S) -> Result<S::Ok, S::Error> {
        let m: BTreeMap<String, &String> = m.iter().map(|(p, v)| (p.to_string(), v)).collect();
        m.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<u16, String>, D::Error> {
        BTreeMap::<String, String>::deserialize(d)?
            .into_iter()
            .map(|(p, v)| {
                let p = p
                    .parse()
                    .map_err(|_| D::Error::custom(format!("invalid port {p:?}")))?;
                Ok((p, v))
            })
            .collect()
    }
}

impl HostConfig {
    /// Reads `<dir>/meshd.toml`, or returns the defaults when it does not exist. The
    /// result is validated.
    pub fn load_or_default(dir: &Path) -> Result<HostConfig, ConfigError> {
        let path = dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HostConfig::default());
            }
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let cfg: HostConfig = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Writes `<dir>/meshd.toml` atomically (temp file, fsync, rename).
    pub fn save(&self, dir: &Path) -> Result<(), ConfigError> {
        let path = dir.join(CONFIG_FILE);
        let text = toml::to_string(self).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        mesh_core::local::atomic_write(&path, text.as_bytes()).map_err(|e| ConfigError::Io {
            source: match e {
                mesh_core::NodeError::Io(e) => e,
                other => std::io::Error::other(other.to_string()),
            },
            path,
        })
    }

    /// Checks every field against the rules of `config set`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for key in KEYS {
            let value = self.get(key).unwrap_or_default();
            self.clone().set(key, &value)?;
        }
        // Not a `config set` key. A longer hold would outlast the 15-min alarm's re-arm.
        if self.android.periodic_wake_s > MAX_PERIODIC_WAKE_S {
            return Err(invalid(
                "android.periodic_wake_s",
                format!("expected 0 to {MAX_PERIODIC_WAKE_S} (seconds)"),
            ));
        }
        Ok(())
    }

    /// Every key and its value as `config set` takes it.
    pub fn entries(&self) -> Vec<(String, String)> {
        KEYS.iter()
            .filter_map(|k| Some((k.to_string(), self.get(k)?)))
            .collect()
    }

    /// One key's value as `config set` takes it; `None` for an unknown key.
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "name" => self.name.clone(),
            "tags" => self.tags.join(","),
            "relay" => self.relay.clone(),
            "discovery" => self.discovery.to_string(),
            "log_level" => self.log_level.clone(),
            "gateway.listen" => self.gateway.listen.to_string(),
            "gateway.ports" => self
                .gateway
                .ports
                .iter()
                .map(|(p, s)| format!("{p}={s}"))
                .collect::<Vec<_>>()
                .join(","),
            "services.max_memory_mb" => self
                .services
                .max_memory_mb
                .map(|m| m.to_string())
                .unwrap_or_default(),
            "admin.windows_user_sid" => self.admin.windows_user_sid.clone().unwrap_or_default(),
            _ => return None,
        })
    }

    /// Validates and applies one setting. Returns whether it takes effect only after a
    /// restart (`gateway.*`, `relay`, `discovery`, `log_level`, `services.*`, `admin.*`);
    /// `name` and `tags` the daemon applies at once.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool, ConfigError> {
        let value = value.trim();
        match key {
            "name" => {
                member_info(value, &self.tags).map_err(|m| invalid(key, m))?;
                self.name = value.to_string();
                Ok(false)
            }
            "tags" => {
                let tags = parse_list(value);
                member_info(&self.name, &tags).map_err(|m| invalid(key, m))?;
                self.tags = tags;
                Ok(false)
            }
            "relay" => {
                if !(value == "default" || value == "disabled" || value.starts_with("https://")) {
                    return Err(invalid(
                        key,
                        "expected default, disabled or an https:// URL",
                    ));
                }
                self.relay = value.to_string();
                Ok(true)
            }
            "discovery" => {
                self.discovery = value
                    .parse()
                    .map_err(|_| invalid(key, "expected true or false"))?;
                Ok(true)
            }
            "log_level" => {
                let level = value.to_ascii_lowercase();
                if !LOG_LEVELS.contains(&level.as_str()) {
                    return Err(invalid(key, "expected trace, debug, info, warn or error"));
                }
                self.log_level = level;
                Ok(true)
            }
            "gateway.listen" => {
                let addr: SocketAddr = value
                    .parse()
                    .map_err(|_| invalid(key, "expected a loopback address and port"))?;
                if !addr.ip().is_loopback() {
                    return Err(invalid(
                        key,
                        format!("{} is not a loopback address", addr.ip()),
                    ));
                }
                check_port(key, addr.port())?;
                if self.gateway.ports.contains_key(&addr.port()) {
                    return Err(invalid(key, "the port is already a mapped port"));
                }
                self.gateway.listen = addr;
                Ok(true)
            }
            "gateway.ports" => {
                let mut ports = BTreeMap::new();
                for entry in parse_list(value) {
                    let (port, svc) = entry
                        .split_once('=')
                        .ok_or_else(|| invalid(key, "expected port=service,…"))?;
                    let port: u16 = port
                        .trim()
                        .parse()
                        .map_err(|_| invalid(key, format!("invalid port {port:?}")))?;
                    check_port(key, port)?;
                    let svc = svc.trim();
                    if !valid_service_name(svc) {
                        return Err(invalid(key, format!("invalid service name {svc:?}")));
                    }
                    if port == self.gateway.listen.port() {
                        return Err(invalid(key, format!("{port} is the main gateway port")));
                    }
                    if ports.insert(port, svc.to_string()).is_some() {
                        return Err(invalid(key, format!("port {port} is mapped twice")));
                    }
                }
                self.gateway.ports = ports;
                Ok(true)
            }
            "services.max_memory_mb" => {
                self.services.max_memory_mb = if value.is_empty() {
                    None
                } else {
                    let mb: u32 = value
                        .parse()
                        .ok()
                        .filter(|m| MEMORY_MB.contains(m))
                        .ok_or_else(|| invalid(key, "expected 64 to 65536 (MiB)"))?;
                    Some(mb)
                };
                Ok(true)
            }
            "admin.windows_user_sid" => {
                self.admin.windows_user_sid = if value.is_empty() {
                    None
                } else if ["S-1-5-21-", "S-1-12-1-"]
                    .iter()
                    .any(|p| value.starts_with(p))
                    && value[9..]
                        .split('-')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                {
                    Some(value.to_string())
                } else {
                    return Err(invalid(
                        key,
                        "expected a user account SID such as S-1-5-21-… (not a group or well-known SID)",
                    ));
                };
                Ok(true)
            }
            _ => Err(ConfigError::UnknownKey(key.to_string())),
        }
    }

    /// The relay setting for the node.
    pub fn relay_choice(&self) -> RelayChoice {
        match self.relay.as_str() {
            "default" => RelayChoice::Default,
            "disabled" => RelayChoice::Disabled,
            url => RelayChoice::Custom(url.to_string()),
        }
    }
}

fn check_port(key: &str, port: u16) -> Result<(), ConfigError> {
    if PORTS.contains(&port) {
        Ok(())
    } else {
        Err(invalid(key, format!("port {port} is outside 1024-65535")))
    }
}

/// A comma-separated list; empty entries are dropped.
fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Checks a name and tags with the member-info limits.
fn member_info(name: &str, tags: &[String]) -> Result<(), String> {
    MemberInfo {
        name: name.to_string(),
        platform: String::new(),
        tags: tags.to_vec(),
    }
    .validate()
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example_parses_and_round_trips() {
        let text = r#"
name = "pc"
tags = ["desktop"]
relay = "default"
discovery = true
log_level = "info"

[gateway]
listen = "127.0.0.1:7777"
ports = { 7801 = "sync" }

[services]
max_memory_mb = 4096

[admin]
windows_user_sid = "S-1-5-21-1-2-3-1001"
"#;
        let cfg: HostConfig = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.gateway.ports.get(&7801).map(String::as_str),
            Some("sync")
        );
        assert_eq!(cfg.services.max_memory_mb, Some(4096));
        let back: HostConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn defaults_and_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HostConfig::load_or_default(dir.path()).unwrap();
        assert_eq!(cfg, HostConfig::default());
        cfg.validate().unwrap();
        let mut cfg = cfg;
        assert!(!cfg.set("name", "pc").unwrap());
        assert!(cfg.set("services.max_memory_mb", "128").unwrap());
        cfg.save(dir.path()).unwrap();
        cfg.save(dir.path()).unwrap(); // overwrites
        assert_eq!(HostConfig::load_or_default(dir.path()).unwrap(), cfg);
        std::fs::write(dir.path().join(CONFIG_FILE), "bogus = 1").unwrap();
        assert!(HostConfig::load_or_default(dir.path()).is_err());
    }

    #[test]
    fn get_set_round_trip() {
        let mut cfg = HostConfig::default();
        for (k, v) in [
            ("tags", "a,b"),
            ("relay", "https://relay.example"),
            ("discovery", "false"),
            ("log_level", "debug"),
            ("gateway.listen", "[::1]:8000"),
            ("gateway.ports", "7801=sync,9000=web"),
            ("services.max_memory_mb", "65536"),
            ("admin.windows_user_sid", "S-1-5-21-10-20-30-1001"),
        ] {
            cfg.set(k, v).unwrap();
            assert_eq!(cfg.get(k).as_deref(), Some(v), "{k}");
        }
        assert_eq!(
            cfg.relay_choice(),
            RelayChoice::Custom("https://relay.example".into())
        );
        cfg.validate().unwrap();
        for (k, v) in [
            ("services.max_memory_mb", "63"),
            ("admin.windows_user_sid", "S-1-x"),
            ("admin.windows_user_sid", "S-1-1-0"),
            ("admin.windows_user_sid", "S-1-5-11"),
            ("admin.windows_user_sid", "S-1-5-32-545"),
            ("admin.windows_user_sid", "S-1-5-21-"),
            ("gateway.ports", "9000"),
            ("gateway.ports", "8000=sync"),
            ("gateway.listen", "127.0.0.1:9000"),
            ("tags", &"x".repeat(40)),
            ("log_level", "loud"),
        ] {
            assert!(cfg.set(k, v).is_err(), "{k}={v}");
        }
        assert!(matches!(
            cfg.set("admin", ""),
            Err(ConfigError::UnknownKey(_))
        ));
    }

    #[test]
    fn periodic_wake_s_is_bounded() {
        let mut cfg = HostConfig::default();
        cfg.android.periodic_wake_s = 600;
        cfg.validate().unwrap();
        cfg.android.periodic_wake_s = 601;
        match cfg.validate() {
            Err(ConfigError::Invalid { key, .. }) => assert_eq!(key, "android.periodic_wake_s"),
            other => panic!("{other:?}"),
        }
    }
}
