use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use mesh_host::config::GatewaySection;
use mesh_host::{Daemon, HostConfig, TestOptions};
use mesh_proto::NodeId;
use mesh_ui::{
    AdminTransport, DaemonStateDto, InProcessTransport, JoinRefusalDto, NoNode, Platform, Ui,
    UiErrorCode, accent_hue, hint_for,
};
use tempfile::TempDir;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_config() -> HostConfig {
    HostConfig {
        relay: "disabled".into(),
        discovery: false,
        gateway: GatewaySection {
            listen: SocketAddr::from(([127, 0, 0, 1], free_port())),
            ports: BTreeMap::new(),
        },
        ..HostConfig::default()
    }
}

async fn start() -> (Daemon, TempDir, Ui) {
    let dir = TempDir::new().unwrap();
    let daemon = Daemon::start_with(dir.path().to_path_buf(), test_config(), TestOptions::fast())
        .await
        .expect("daemon");
    let t: Arc<dyn AdminTransport> = Arc::new(InProcessTransport(daemon.in_process_admin()));
    let ui = Ui::new(Arc::new(move || t.clone()), Platform::DesktopWindows);
    (daemon, dir, ui)
}

fn hex_decode(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn status_uninitialised_then_init() {
    let (_d, _dir, ui) = start().await;
    assert_eq!(
        ui.status().await.unwrap().state,
        DaemonStateDto::Uninitialised
    );
    let s = ui.init("desk".into()).await.unwrap();
    assert_eq!(s.state, DaemonStateDto::Running);
    assert_eq!(s.name.as_deref(), Some("desk"));
    let id = s.node_id.unwrap();
    assert_eq!(id.len(), 64);
    assert!(
        id.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    );
    if let Some(url) = s.gateway_url {
        assert!(url.starts_with("http://"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn members_has_self_with_accent_hue() {
    let (_d, _dir, ui) = start().await;
    let s = ui.init("desk".into()).await.unwrap();
    let members = ui.members().await.unwrap();
    assert_eq!(members.len(), 1);
    let m = &members[0];
    assert!(m.is_self);
    assert_eq!(Some(&m.id), s.node_id.as_ref());
    assert_eq!(m.accent_hue, accent_hue(&NodeId(hex_decode(&m.id))));
    assert!(m.accent_hue < 360);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_info_updates_name_and_tags() {
    let (_d, _dir, ui) = start().await;
    ui.init("desk".into()).await.unwrap();
    ui.set_info(Some("d2".into()), Some(vec!["lab".into()]))
        .await
        .unwrap();
    let s = ui.status().await.unwrap();
    assert_eq!(s.name.as_deref(), Some("d2"));
    assert_eq!(s.tags, vec!["lab".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn config_round_trip() {
    let (_d, _dir, ui) = start().await;
    ui.config_set("log_level".into(), "debug".into())
        .await
        .unwrap();
    let c = ui.config_get().await.unwrap();
    assert!(
        c.iter().any(|e| e.key == "log_level" && e.value == "debug"),
        "{c:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_state_maps_with_code() {
    let (_d, _dir, ui) = start().await;
    let e = ui.members().await.unwrap_err();
    assert_eq!(e.code, UiErrorCode::WrongState);
}

#[tokio::test]
async fn no_node_transport_is_not_running_with_hint() {
    let t: Arc<dyn AdminTransport> = Arc::new(NoNode);
    let ui = Ui::new(Arc::new(move || t.clone()), Platform::Android);
    let e = ui.status().await.unwrap_err();
    assert_eq!(e.code, UiErrorCode::NotRunning);
    assert_eq!(e.hint.as_deref(), Some("Start the node."));
}

#[test]
fn hints_match_spec() {
    use JoinRefusalDto as R;
    let j = UiErrorCode::JoinRefused;
    assert_eq!(
        hint_for(j, Some(R::Expired)),
        Some("Ask the other device for a new invite.")
    );
    assert_eq!(
        hint_for(j, Some(R::Used)),
        Some("Each invite works once. Ask for a new one.")
    );
    assert_eq!(
        hint_for(j, Some(R::InviterUnavailable)),
        Some("Keep Meshnode open on the inviting device and try again.")
    );
    assert_eq!(
        hint_for(UiErrorCode::LastHost, None),
        Some("Add another host device before removing this one.")
    );
    assert_eq!(
        hint_for(UiErrorCode::DaemonUnreachable, None),
        Some("Start Meshnode, or install it as a service.")
    );
    assert_eq!(
        hint_for(UiErrorCode::NotRunning, None),
        Some("Start the node.")
    );
}

#[test]
fn accent_hue_stable() {
    let mut a = [0u8; 32];
    a[0] = 0x01;
    a[1] = 0x2c;
    assert_eq!(accent_hue(&NodeId(a)), 300);
    assert_eq!(accent_hue(&NodeId([0xff; 32])), 15);
}
