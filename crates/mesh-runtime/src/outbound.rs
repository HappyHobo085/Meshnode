//! The outbound HTTP allowlist (sub-spec 3 §3.3): a service may only call the `host:port`
//! destinations in its manifest.

use http::Uri;
use http::uri::Scheme;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

/// The `wasi:http` hooks of one request.
pub(crate) struct Outbound {
    service: String,
    allowed: Vec<String>,
}

impl Outbound {
    pub(crate) fn new(service: &str, allowed: Vec<String>) -> Self {
        Outbound {
            service: service.to_owned(),
            allowed,
        }
    }

    /// Whether `uri`'s `host:port` equals an allowlist entry, ignoring case. The port
    /// defaults to 443 for `https` and 80 for `http`; a URI without a host is never allowed.
    fn allows(&self, uri: &Uri) -> bool {
        let Some(dest) = destination(uri) else {
            return false;
        };
        self.allowed.iter().any(|a| a.eq_ignore_ascii_case(&dest))
    }
}

fn destination(uri: &Uri) -> Option<String> {
    let host = uri.host().filter(|h| !h.is_empty())?;
    let default_port = if uri.scheme() == Some(&Scheme::HTTP) {
        80
    } else {
        443
    };
    Some(format!("{host}:{}", uri.port_u16().unwrap_or(default_port)))
}

impl WasiHttpHooks for Outbound {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), Error>> + Send>,
                    ),
                    Error,
                >,
            > + Send,
    > {
        if !self.allows(request.uri()) {
            let dest = destination(request.uri()).unwrap_or_else(|| request.uri().to_string());
            tracing::warn!(service = %self.service, %dest, "outbound request denied");
            return Box::new(async { Err(Error::HttpRequestDenied) });
        }
        Box::new(async move {
            use http_body_util::BodyExt;

            let _ = fut;
            let (res, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
            Ok((
                res.map(BodyExt::boxed_unsync),
                Box::new(io) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allows(list: &[&str], uri: &str) -> bool {
        let allowed = list.iter().map(|s| s.to_string()).collect();
        Outbound::new("t", allowed).allows(&uri.parse().unwrap())
    }

    #[test]
    fn default_ports_and_case() {
        assert!(allows(
            &["api.example.com:443"],
            "https://API.example.com/x"
        ));
        assert!(allows(&["api.example.com:80"], "http://api.example.com/x"));
        assert!(!allows(
            &["api.example.com:443"],
            "http://api.example.com/x"
        ));
        assert!(allows(&["a.b:8080"], "http://a.b:8080/"));
        assert!(!allows(&["a.b"], "https://a.b/"));
        assert!(!allows(&[], "https://a.b/"));
    }

    #[test]
    fn no_host_is_denied() {
        assert!(!allows(&[":443", ":80", ""], "/just/a/path"));
    }
}
