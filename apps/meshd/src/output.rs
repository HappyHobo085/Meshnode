//! Plain-text output: aligned tables and the views' one-line forms.

use std::fmt::{self, Write as _};
use std::io::{self, Write as _};

use mesh_admin::{LocalStateView, MemberInfoView, ServiceInfoView, ServiceKindView, StatusInfo};
use mesh_proto::{NodeId, Placement, Role};

/// Writes to stdout; a closed pipe (`meshd members | head -0`) ends the process quietly
/// instead of panicking.
pub fn write_stdout(args: fmt::Arguments<'_>, newline: bool) {
    let mut out = io::stdout().lock();
    let mut r = out.write_fmt(args);
    if r.is_ok() && newline {
        r = out.write_all(
            b"
",
        );
    }
    if let Err(e) = r.and_then(|()| out.flush()) {
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("cannot write output: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
}

/// `println!` that survives a closed pipe.
macro_rules! say {
    ($($arg:tt)*) => { $crate::output::write_stdout(format_args!($($arg)*), true) };
}

/// `print!` that survives a closed pipe.
macro_rules! say_raw {
    ($($arg:tt)*) => { $crate::output::write_stdout(format_args!($($arg)*), false) };
}

pub(crate) use {say, say_raw};

/// Full lowercase hex of a node id (what `remove` and `--nodes` match prefixes of).
pub fn node_hex(id: &NodeId) -> String {
    hex::encode(id.0)
}

/// Rows under a header, columns padded to their widest cell and separated by two spaces.
/// The last column is not padded.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        for (i, cell) in cells.enumerate() {
            if i + 1 == cols {
                out.push_str(cell);
            } else {
                let _ = write!(out, "{cell:<w$}  ", w = widths[i]);
            }
        }
        out.push('\n');
    };
    line(&mut header.iter().copied());
    for row in rows {
        line(&mut row.iter().map(String::as_str));
    }
    out
}

fn dash(s: String) -> String {
    if s.is_empty() { "-".into() } else { s }
}

fn role(r: Role) -> &'static str {
    match r {
        Role::Host => "host",
        Role::Client => "client",
    }
}

pub fn status_text(s: &StatusInfo) -> String {
    let mut o = String::new();
    let mut kv = |k: &str, v: String| {
        let _ = writeln!(o, "{k:<9} {v}");
    };
    kv("State:", s.state.to_string());
    kv("Version:", s.daemon_version.clone());
    kv("Data:", s.data_dir.clone());
    if let Some(n) = &s.name {
        kv("Name:", n.clone());
    }
    if let Some(id) = &s.node_id {
        kv("Node:", node_hex(id));
    }
    if let Some(id) = &s.mesh_id {
        kv("Mesh:", hex::encode(id.0));
    }
    if let Some(r) = s.role {
        kv("Role:", role(r).into());
    }
    if s.node_id.is_some() {
        kv("Tags:", dash(s.tags.join(",")));
        kv(
            "Members:",
            format!("{} ({} online)", s.members_total, s.members_online),
        );
        kv("Services:", s.services.to_string());
        kv("Gateway:", s.gateway.clone().unwrap_or_else(|| "-".into()));
        for (port, svc) in &s.ports {
            kv("Port:", format!("{port} -> {svc}"));
        }
    }
    o
}

fn ms_ago(ms: u64) -> String {
    match ms / 1000 {
        0 => "now".into(),
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub fn members_text(members: &[MemberInfoView]) -> String {
    let rows: Vec<Vec<String>> = members
        .iter()
        .map(|m| {
            let mut state = if m.is_self {
                "self".to_string()
            } else if m.online {
                "online".to_string()
            } else {
                m.last_seen_ms
                    .map_or("offline".to_string(), |ms| format!("seen {}", ms_ago(ms)))
            };
            if !m.effective {
                state.push_str(" (not effective)");
            }
            vec![
                m.id.short(),
                dash(m.name.clone()),
                role(m.role).into(),
                dash(m.platform.clone()),
                dash(m.tags.join(",")),
                state,
                m.rtt_ms.map_or("-".into(), |r| format!("{r} ms")),
                m.lag.to_string(),
            ]
        })
        .collect();
    table(
        &[
            "ID", "NAME", "ROLE", "PLATFORM", "TAGS", "STATE", "RTT", "LAG",
        ],
        &rows,
    )
}

pub fn placement_text(p: &Placement) -> String {
    match p {
        Placement::AllHosts => "all hosts".into(),
        Placement::Tags(t) => format!("tags {}", t.join(",")),
        Placement::Nodes(n) => format!(
            "nodes {}",
            n.iter().map(NodeId::short).collect::<Vec<_>>().join(",")
        ),
    }
}

fn local_text(l: &LocalStateView) -> String {
    match l {
        LocalStateView::NotPlaced => "not placed".into(),
        LocalStateView::Fetching => "fetching".into(),
        LocalStateView::Ready { version } => format!("ready v{version}"),
        LocalStateView::Failed { error } => format!("failed: {error}"),
    }
}

pub fn services_text(services: &[ServiceInfoView]) -> String {
    let rows: Vec<Vec<String>> = services
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                match s.kind {
                    ServiceKindView::Wasm => "wasm",
                    ServiceKindView::Static => "static",
                }
                .into(),
                format!("v{}", s.version),
                placement_text(&s.placement),
                s.placed_hosts.len().to_string(),
                local_text(&s.local),
                format!(
                    "{} MB / {} ms / {} B",
                    s.limits.memory_mb, s.limits.request_timeout_ms, s.limits.max_body_bytes
                ),
                dash(s.outbound.join(",")),
                format_bytes(s.data_bytes),
            ]
        })
        .collect();
    table(
        &[
            "NAME",
            "KIND",
            "VERSION",
            "PLACEMENT",
            "HOSTS",
            "LOCAL",
            "LIMITS",
            "OUTBOUND",
            "DATA",
        ],
        &rows,
    )
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// A QR code as dense unicode blocks (two module rows per text row), or `None` when the
/// text does not fit in one.
pub fn qr_text(text: &str) -> Option<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
    )
}

/// The round-trip time of a probe line `seq=<n> ms=<rtt>`; `None` for an error line.
pub fn probe_sample(line: &str) -> Option<f64> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("ms="))
        .and_then(|v| v.parse().ok())
}

/// The nearest-rank `pct`th percentile of `sorted` (ascending, not empty).
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    let rank = (pct / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// `p50=<ms> p95=<ms> max=<ms> failed=<k>/<total>` over the successful samples `ms`.
pub fn probe_summary(ms: &[f64], total: usize) -> String {
    let failed = total.saturating_sub(ms.len());
    let mut sorted = ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return format!("p50=- p95=- max=- failed={failed}/{total}");
    }
    format!(
        "p50={:.1} p95={:.1} max={:.1} failed={failed}/{total}",
        percentile(&sorted, 50.0),
        percentile(&sorted, 95.0),
        sorted[sorted.len() - 1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        let ms: Vec<f64> = (1..=20).rev().map(f64::from).collect();
        // rank ceil(0.5*20)=10, ceil(0.95*20)=19.
        assert_eq!(
            probe_summary(&ms, 20),
            "p50=10.0 p95=19.0 max=20.0 failed=0/20"
        );
        assert_eq!(
            probe_summary(&[4.0], 3),
            "p50=4.0 p95=4.0 max=4.0 failed=2/3"
        );
        assert_eq!(probe_summary(&[], 2), "p50=- p95=- max=- failed=2/2");
        assert_eq!(probe_sample("seq=1 ms=12.5 connect=9.1"), Some(12.5));
        assert_eq!(probe_sample("seq=2 error=timed out after 30000 ms"), None);
    }

    #[test]
    fn table_aligns_columns() {
        let t = table(
            &["A", "BBB"],
            &[
                vec!["xxxx".into(), "1".into()],
                vec!["y".into(), "22".into()],
            ],
        );
        assert_eq!(t, "A     BBB\nxxxx  1\ny     22\n");
    }

    #[test]
    fn qr_renders_blocks() {
        let q = qr_text("mesh1abcdef").unwrap();
        assert!(q.contains('\u{2588}') || q.contains('\u{2580}') || q.contains('\u{2584}'));
        assert!(q.lines().count() > 5);
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
    }
}
