//! The read-only status page and `/status.json` (spec §4.3).

use std::fmt::Write;
use std::net::SocketAddr;

use mesh_core::NodeHandle;
use mesh_proto::ServiceKind;
use mesh_services::{LocalStatus, Services};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct Snapshot {
    node: NodeInfo,
    members: Vec<MemberRow>,
    services: Vec<ServiceRow>,
    gateway: Vec<String>,
}

#[derive(Serialize)]
struct NodeInfo {
    id: String,
    short_id: String,
    name: String,
    status: String,
    mesh_id: String,
}

#[derive(Serialize)]
struct MemberRow {
    id: String,
    short_id: String,
    name: String,
    role: String,
    platform: String,
    tags: Vec<String>,
    online: bool,
    rtt_ms: Option<u64>,
    lag: u64,
}

#[derive(Serialize)]
struct ServiceRow {
    name: String,
    kind: &'static str,
    version: u64,
    local: String,
    placed_hosts: Vec<String>,
    data_bytes: u64,
}

impl Snapshot {
    /// `addrs` are the gateway's listeners (main first has no service; mapped have one).
    pub(crate) fn take(
        node: &NodeHandle,
        services: &Services,
        addrs: &[(SocketAddr, Option<String>)],
    ) -> Snapshot {
        let me = node.node_id();
        let members: Vec<MemberRow> = node
            .members()
            .into_iter()
            .map(|m| {
                let info = m.info.as_ref();
                MemberRow {
                    id: hex::encode(m.id.0),
                    short_id: m.id.short(),
                    name: info.map(|i| i.name.clone()).unwrap_or_default(),
                    role: format!("{:?}", m.role),
                    platform: info.map(|i| i.platform.clone()).unwrap_or_default(),
                    tags: info.map(|i| i.tags.clone()).unwrap_or_default(),
                    online: m.online,
                    rtt_ms: m.rtt_ms,
                    lag: m.lag,
                }
            })
            .collect();
        let name = node
            .members()
            .iter()
            .find(|m| m.id == me)
            .and_then(|m| m.info.as_ref().map(|i| i.name.clone()))
            .unwrap_or_default();
        let services: Vec<ServiceRow> = services
            .services()
            .into_iter()
            .map(|s| ServiceRow {
                name: s.manifest.name,
                kind: match s.manifest.kind {
                    ServiceKind::Wasm { .. } => "wasm",
                    ServiceKind::Static { .. } => "static",
                },
                version: s.manifest.version,
                local: match s.local {
                    LocalStatus::NotPlaced => "not placed".to_string(),
                    LocalStatus::Fetching => "fetching".to_string(),
                    LocalStatus::Ready { version } => format!("ready (v{version})"),
                    LocalStatus::Failed { error, .. } => format!("failed: {error}"),
                },
                placed_hosts: s.placed_hosts.iter().map(|h| h.short()).collect(),
                data_bytes: s.data_bytes,
            })
            .collect();
        let mut gateway = Vec::new();
        for (addr, svc) in addrs {
            match svc {
                None => gateway.push(format!("http://mesh.localhost:{}/", addr.port())),
                Some(svc) => gateway.push(format!("http://127.0.0.1:{}/ ({svc})", addr.port())),
            }
        }
        if let Some((addr, None)) = addrs.first() {
            for s in &services {
                gateway.push(format!("http://{}.mesh.localhost:{}/", s.name, addr.port()));
            }
        }
        Snapshot {
            node: NodeInfo {
                id: hex::encode(me.0),
                short_id: me.short(),
                name,
                status: format!("{:?}", node.status()),
                mesh_id: hex::encode(node.mesh().0),
            },
            members,
            services,
            gateway,
        }
    }

    pub(crate) fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    pub(crate) fn html(&self) -> String {
        let mut o = String::from(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Meshnode</title>\
             <style>body{font-family:sans-serif;margin:2rem}table{border-collapse:collapse}\
             td,th{border:1px solid #aaa;padding:.25rem .6rem;text-align:left}</style>\
             </head><body><h1>Meshnode</h1>",
        );
        let n = &self.node;
        let _ = write!(
            o,
            "<h2>This node</h2><p>{} <code>{}</code> &middot; status {} &middot; mesh <code>{}</code></p>",
            esc(&n.name),
            esc(&n.short_id),
            esc(&n.status),
            esc(&n.mesh_id)
        );
        o.push_str(
            "<h2>Members</h2><table><tr><th>Name</th><th>Id</th><th>Role</th><th>Platform</th>\
             <th>Tags</th><th>Online</th><th>RTT</th><th>Lag</th></tr>",
        );
        for m in &self.members {
            let _ = write!(
                o,
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&m.name),
                esc(&m.short_id),
                esc(&m.role),
                esc(&m.platform),
                esc(&m.tags.join(", ")),
                if m.online { "yes" } else { "no" },
                m.rtt_ms.map_or("-".to_string(), |r| format!("{r} ms")),
                m.lag
            );
        }
        o.push_str(
            "</table><h2>Services</h2><table><tr><th>Name</th><th>Kind</th><th>Version</th>\
             <th>Local</th><th>Placed on</th><th>Data</th></tr>",
        );
        for s in &self.services {
            let _ = write!(
                o,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} B</td></tr>",
                esc(&s.name),
                s.kind,
                s.version,
                esc(&s.local),
                esc(&s.placed_hosts.join(", ")),
                s.data_bytes
            );
        }
        o.push_str("</table><h2>Gateway</h2><ul>");
        for u in &self.gateway {
            let _ = write!(o, "<li>{}</li>", esc(u));
        }
        o.push_str("</ul></body></html>");
        o
    }
}

/// HTML-escapes text for element content and attribute values.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::esc;

    #[test]
    fn escapes() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
