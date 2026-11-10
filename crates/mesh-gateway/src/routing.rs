//! Host-based routing (spec §4.2).

use mesh_proto::valid_service_name;

const SUFFIX: &str = ".mesh.localhost";

/// What a request's Host selects.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Service(String),
    Status,
    /// Main port, unrecognised Host: `404`.
    Unknown,
    /// Mapped port, foreign Host: `421`.
    Misdirected,
}

/// The Host lower-cased with its port removed (`[::1]:7777` gives `[::1]`); `None` if the
/// port is not numeric or the bracketed form is malformed.
pub(crate) fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().to_ascii_lowercase();
    if let Some(rest) = h.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        return match tail {
            "" => Some(format!("[{addr}]")),
            t if t.strip_prefix(':').is_some_and(is_port) => Some(format!("[{addr}]")),
            _ => None,
        };
    }
    match h.rsplit_once(':') {
        None => Some(h),
        Some((name, port)) if is_port(port) && !name.contains(':') => Some(name.to_string()),
        Some(_) => None,
    }
}

fn is_port(s: &str) -> bool {
    s.is_empty() || (s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit()))
}

fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Routing on the main port. `host` is the raw Host value.
pub(crate) fn route_main(host: &str) -> Target {
    let Some(host) = normalize_host(host) else {
        return Target::Unknown;
    };
    if host == "mesh.localhost" || is_local(&host) {
        return Target::Status;
    }
    match host.strip_suffix(SUFFIX) {
        Some(svc) if valid_service_name(svc) => Target::Service(svc.to_string()),
        _ => Target::Unknown,
    }
}

/// Routing on a port mapped to `service`: only the loopback names reach it, so a page on
/// a domain that resolves to 127.0.0.1 cannot (DNS-rebinding protection).
pub(crate) fn route_mapped(host: &str, service: &str) -> Target {
    match normalize_host(host) {
        Some(h) if is_local(&h) => Target::Service(service.to_string()),
        _ => Target::Misdirected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize_host("A.B:80").unwrap(), "a.b");
        assert_eq!(normalize_host("[::1]:7777").unwrap(), "[::1]");
        assert_eq!(normalize_host("[::1]").unwrap(), "[::1]");
        assert_eq!(normalize_host("::1"), None);
        assert_eq!(normalize_host("a:b"), None);
        assert_eq!(normalize_host("[::1]x"), None);
    }

    #[test]
    fn main_routes() {
        assert_eq!(route_main("x.mesh.localhost"), Target::Service("x".into()));
        assert_eq!(route_main("mesh.localhost:1"), Target::Status);
        assert_eq!(route_main("Foo_Bar.mesh.localhost"), Target::Unknown);
        assert_eq!(route_main("a..mesh.localhost"), Target::Unknown);
    }
}
