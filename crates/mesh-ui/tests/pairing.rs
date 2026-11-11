use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mesh_host::config::GatewaySection;
use mesh_host::{Daemon, HostConfig, TestOptions};
use mesh_ui::{
    AdminTransport, InProcessTransport, JoinRefusalDto, Platform, RoleDto, Ui, UiErrorCode, qr_svg,
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

struct Node {
    _daemon: Daemon,
    _dir: TempDir,
    ui: Ui,
}

async fn start() -> Node {
    let dir = TempDir::new().unwrap();
    let daemon = Daemon::start_with(dir.path().to_path_buf(), test_config(), TestOptions::fast())
        .await
        .expect("daemon");
    let t: Arc<dyn AdminTransport> = Arc::new(InProcessTransport(daemon.in_process_admin()));
    let ui = Ui::new(Arc::new(move || t.clone()), Platform::DesktopWindows);
    Node {
        _daemon: daemon,
        _dir: dir,
        ui,
    }
}

async fn founded() -> Node {
    let n = start().await;
    n.ui.init("a".into()).await.unwrap();
    n
}

async fn wait_paired(nodes: &[&Node]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut ok = true;
        for n in nodes {
            ok &= match n.ui.members().await {
                Ok(m) => {
                    m.iter().filter(|m| m.effective).count() == nodes.len()
                        && m.iter().all(|m| m.online)
                }
                Err(_) => false,
            };
        }
        if ok {
            return;
        }
        assert!(Instant::now() < deadline, "nodes did not pair in 30 s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_then_join_pairs_two_nodes() {
    let a = founded().await;
    let b = start().await;
    let inv = a.ui.invite(RoleDto::Host, 900).await.unwrap();
    b.ui.join(inv.text, "phone".into()).await.unwrap();
    wait_paired(&[&a, &b]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_qr_decodes_to_ticket() {
    let a = founded().await;
    let inv = a.ui.invite(RoleDto::Client, 300).await.unwrap();
    let tree = resvg::usvg::Tree::from_str(&inv.qr_svg, &resvg::usvg::Options::default()).unwrap();
    let modules = tree.size().width();
    let px = (modules * 8.0).ceil() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(px, px).unwrap();
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(8.0, 8.0),
        &mut pixmap.as_mut(),
    );
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(px as usize, px as usize, |x, y| {
        let p = pixmap.pixel(x as u32, y as u32).unwrap();
        ((p.red() as u32 * 3 + p.green() as u32 * 6 + p.blue() as u32) / 10) as u8
    });
    let grids = img.detect_grids();
    assert_eq!(grids.len(), 1);
    let (_, content) = grids[0].decode().unwrap();
    assert_eq!(content, inv.text);
}

#[test]
fn qr_svg_is_inert() {
    let svg = qr_svg("mesh1abcdefghijklmnop");
    assert!(svg.starts_with("<svg"));
    assert!(!svg.contains("<script"));
    assert!(!svg.contains("href"));
    assert!(!svg.contains(" on"));
    assert_eq!(svg.matches("<path").count(), 1);
    assert!(svg.contains("viewBox"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_rejects_other_ttls() {
    let a = founded().await;
    let e = a.ui.invite(RoleDto::Host, 120).await.unwrap_err();
    assert_eq!(e.code, UiErrorCode::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_expires_at_matches_ttl() {
    let a = founded().await;
    let inv = a.ui.invite(RoleDto::Host, 900).await.unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let want = now + 900_000;
    assert!(
        (inv.expires_at_ms as i64 - want).abs() < 2_000,
        "{} vs {want}",
        inv.expires_at_ms
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn join_trims_ticket_whitespace() {
    let a = founded().await;
    let b = start().await;
    let inv = a.ui.invite(RoleDto::Host, 900).await.unwrap();
    b.ui.join(format!("  \n{}\n ", inv.text), "b".into())
        .await
        .unwrap();
    wait_paired(&[&a, &b]).await;
    // A ticket wrapped by mail, chat or a terminal: whitespace inside it is dropped too.
    let c = start().await;
    let inv = a.ui.invite(RoleDto::Host, 900).await.unwrap();
    let (head, tail) = inv.text.split_at(inv.text.len() / 3);
    let (mid, tail) = tail.split_at(tail.len() / 2);
    c.ui.join(format!("{head}\r\n{mid} \t{tail}"), "c".into())
        .await
        .unwrap();
    wait_paired(&[&a, &b, &c]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn join_non_ticket_is_invalid_argument_with_hint() {
    let b = start().await;
    let e =
        b.ui.join("https://example.com".into(), "x".into())
            .await
            .unwrap_err();
    assert_eq!(e.code, UiErrorCode::InvalidArgument);
    assert_eq!(e.message, "That isn't a Meshnode invite");
    assert_eq!(
        e.hint.as_deref(),
        Some("Scan or paste the invite shown on the other device.")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn join_used_ticket_is_refused_with_reason() {
    let a = founded().await;
    let b = start().await;
    let c = start().await;
    let inv = a.ui.invite(RoleDto::Host, 900).await.unwrap();
    b.ui.join(inv.text.clone(), "b".into()).await.unwrap();
    let e = c.ui.join(inv.text, "c".into()).await.unwrap_err();
    assert_eq!(e.code, UiErrorCode::JoinRefused);
    assert_eq!(e.reason, Some(JoinRefusalDto::Used));
    assert_eq!(
        e.hint.as_deref(),
        Some("Each invite works once. Ask for a new one.")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_member() {
    let a = founded().await;
    let b = start().await;
    let inv = a.ui.invite(RoleDto::Host, 900).await.unwrap();
    let bs = b.ui.join(inv.text, "b".into()).await.unwrap();
    wait_paired(&[&a, &b]).await;
    let b_id = bs.node_id.unwrap();
    a.ui.remove(b_id.clone()).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let m = a.ui.members().await.unwrap();
        let bm = m.iter().find(|m| m.id == b_id).expect("b listed");
        if !bm.effective {
            break;
        }
        assert!(Instant::now() < deadline, "b still effective after 10 s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
