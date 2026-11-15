# mesh-client

The Meshnode SDK for apps. A `MeshClient` is a **client-role** member of a mesh: it keeps
an identity in a data directory, replicates only the membership namespace (`_mesh`:
members, service manifests, placement), and sends HTTP requests to the services that the
mesh's hosts run.

It never runs services, never serves `httpfwd`, never opens a gateway and never fetches
blobs, and it does not link the WASM runtime (`cargo tree -p mesh-client -e normal` has
no `wasmtime`; `tests/no_runtime_deps.rs` checks this).

## Rust

```toml
[dependencies]
mesh-client = { path = "../crates/mesh-client" }
bytes = "1"
http = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use mesh_client::{ClientError, ClientEvent, ClientOptions, MeshClient, Role};

async fn run(dir: &Path, ticket: Option<&str>) -> Result<(), ClientError> {
    let opts = ClientOptions {
        name: "my-app".into(),
        ..ClientOptions::default() // platform = this OS, request_timeout = 30 s
    };
    // First run: join with a client invite (`meshd invite --client`, or the UI).
    // Later runs: open the same directory.
    let client = match ticket {
        Some(t) => MeshClient::join(dir, t, opts).await?,
        None => MeshClient::open(dir, opts).await?,
    };

    let mut events = client.subscribe();
    tokio::spawn(async move {
        while let Ok(e) = events.recv().await {
            if e == ClientEvent::Removed {
                eprintln!("removed from the mesh");
            }
        }
    });

    let req = http::Request::get("/").body(Bytes::new()).unwrap();
    let resp = client.request("blog", req).await?;
    println!("{} {}", resp.status(), String::from_utf8_lossy(resp.body()));

    println!("{:?}", client.status());   // mesh, id, online hosts, first sync, removed
    println!("{:?}", client.members());  // id, name, role, platform, online, rtt
    println!("{:?}", client.services()); // name, kind, hosts

    let ticket = client.invite(Role::Client, Duration::from_secs(600)).await?;
    println!("invite another app: {ticket}");

    client.shutdown().await;
    Ok(())
}
```

### Behaviour

- **One client per data directory.** `client.lock` in the directory is held while a
  client has it open; a second `join`/`open` fails with `AlreadyOpen`.
- **`join`** accepts only a client invite (`InvalidTicket` otherwise, before anything is
  written), refuses a directory that already holds `node.key` (`AlreadyJoined`), and
  returns once joined: the first sync finishes in the background
  (`status().first_sync_done`), so `services()` may be empty for a moment.
- **`open`** needs a directory joined earlier (`NotJoined`).
- **`request(service, req)`** forwards the method, path and query, headers (hop-by-hop
  ones dropped) and body to the nearest online host that runs the service, trying the
  next one on failure, up to three hosts. The host's response is returned **as it is**,
  `4xx`/`5xx` included. Answered locally instead: `400` if the path does not start with
  `/`, `413` for a body over 16 MiB (`431`/`414` for headers or a path too large to
  forward). Errors: `NoHost` (no online host runs it, it is not deployed, or no host
  could be connected to), `Timeout` (a host was reached but did not answer in time, or
  the whole call took longer than `request_timeout`), `Removed`.
- **Removal.** Once removed, the client sends `ClientEvent::Removed` once, and every
  `request`, `invite` and `remove` fails with `Removed`.
- **Events:** `MembersChanged`, `ServicesChanged`, `Removed`, `Online` (some host is
  reachable again), `Offline` (none is).

### Example

```sh
cargo run -p mesh-client --example fetch -- join ./client-data <ticket> blog /
cargo run -p mesh-client --example fetch -- open ./client-data blog /about.html
```

## Kotlin

See Task 7 (`mesh-client-ffi`).
