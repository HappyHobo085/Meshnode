//! Fetches one page from a mesh service through a client node.
//!
//! ```text
//! fetch join <dir> <ticket> <service> <path>   # join with a client invite, then fetch
//! fetch open <dir> <service> <path>            # reuse the client joined in <dir>
//! ```
//!
//! Prints the status line, the headers and the body; exits non-zero on an error.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bytes::Bytes;
use mesh_client::{ClientError, ClientOptions, MeshClient};
use tokio::time::{Instant, sleep};

const USAGE: &str =
    "usage:\n  fetch join <dir> <ticket> <service> <path>\n  fetch open <dir> <service> <path>";
/// How long to wait for the service to show up in a fresh replica.
const WAIT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = ClientOptions {
        name: "fetch".into(),
        ..ClientOptions::default()
    };
    let (client, service, path) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["join", dir, ticket, service, path] => (
            MeshClient::join(&PathBuf::from(dir), ticket, opts).await,
            service,
            path,
        ),
        ["open", dir, service, path] => (
            MeshClient::open(&PathBuf::from(dir), opts).await,
            service,
            path,
        ),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot start the client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = fetch(&client, service, path).await;
    client.shutdown().await;
    code
}

async fn fetch(client: &MeshClient, service: &str, path: &str) -> ExitCode {
    // A client that just started may not have the service's manifest or a live host yet.
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let listed = client.services().iter().any(|s| s.name == service);
        if listed && client.status().online_hosts > 0 {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    let req = match http::Request::get(path).body(Bytes::new()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bad path {path:?}: {e}");
            return ExitCode::from(2);
        }
    };
    match client.request(service, req).await {
        Ok(resp) => {
            println!("{:?} {}", resp.version(), resp.status());
            for (name, value) in resp.headers() {
                println!("{name}: {}", String::from_utf8_lossy(value.as_bytes()));
            }
            println!();
            println!("{}", String::from_utf8_lossy(resp.body()));
            ExitCode::SUCCESS
        }
        Err(ClientError::NoHost) => {
            eprintln!("no host could serve {service} (is it deployed, and a host online?)");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("request failed: {e}");
            ExitCode::FAILURE
        }
    }
}
