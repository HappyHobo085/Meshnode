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
  written) and refuses a directory that already holds a joined node (`mesh.json`:
  `AlreadyJoined`). A failed or interrupted join leaves only `node.key`; the next `join`
  reuses it, so retrying with the same ticket completes a join whose reply was lost. It
  returns once joined: the first sync finishes in the background
  (`status().first_sync_done`), so `services()` may be empty for a moment.
- **`open`** needs a directory joined earlier as a client (`NotJoined` otherwise, a host's
  data directory included). A client re-invited as a host while running (role upgrade)
  is out of scope for now.
- **Stopping.** `shutdown()` releases the directory before it returns. Dropping a client
  stops it in the background, on the Tokio runtime it was started on (also when it is
  dropped on a thread without one); until it has let go of the directory, reopening it
  is refused with `AlreadyOpen`.
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

`crates/mesh-client-ffi` exposes the same client to Kotlin through UniFFI: package
`net.meshnode.client`, native library `mesh_client_ffi`. The bindings are committed in
`crates/mesh-client-ffi/kotlin/` (regenerate with `scripts/gen-client-bindings.sh`;
`--check` fails when they are out of date).

### Android

`scripts/build-client-aar.sh` builds `dist/mesh-client.aar` locally (not committed):
`libmesh_client_ffi.so` for `arm64-v8a` and `x86_64`, the compiled bindings and a minimal
manifest (`INTERNET`, `ACCESS_NETWORK_STATE`, minSdk 29). It needs `cargo-ndk`,
`ANDROID_NDK_HOME`, both Rust Android targets and a JDK 17. The app adds the runtime
dependencies itself:

```kotlin
dependencies {
    implementation(files("libs/mesh-client.aar"))
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
}
```

On Android, call `MeshClientAndroid.init(context)` once (e.g. in `Application.onCreate`)
before the first `join`/`open`: the networking code needs the application context.
Without it they fail with `MeshClientException.NotInitialised`. The desktop JVM needs
no set-up; it loads the host library through JNA (`jna.library.path`).

```kotlin
import net.meshnode.client.*

suspend fun run(dir: String, ticket: String?) {
    val opts = MeshClientOptions(name = "my-app") // platform = this OS, requestTimeoutMs = 30000
    val client = if (ticket != null) MeshClient.join(dir, ticket, opts) else MeshClient.open(dir, opts)
    try {
        client.setListener(object : MeshClientListener {
            override fun onEvent(event: MeshEvent) {
                // MembersChanged, ServicesChanged, Removed, Online, Offline.
                // Called on a library thread: return quickly.
            }
        })
        val resp = client.request(
            "blog",
            MeshRequest(method = "GET", path = "/", headers = listOf(), body = byteArrayOf()),
        )
        println("${resp.status} ${resp.body.decodeToString()}")
        println(client.status())   // mesh, id, onlineHosts, firstSyncDone, removed
        println(client.members())  // id (hex), name, role, platform, online, rttMs
        println(client.services()) // name, kind, hosts
        val invite = client.invite(MeshRole.CLIENT, 600uL)
    } catch (e: MeshClientException.NoHost) {
        // no host could serve it: try again later
    } finally {
        client.shutdown()
    }
}
```

- `join`, `open`, `request`, `invite`, `remove` and `shutdown` are `suspend` functions;
  they run on the library's own threads, so they can be called from any dispatcher.
  `status()`, `members()`, `services()` and `setListener` return at once.
- Errors are the sealed `MeshClientException`: the `ClientError` variants above
  (`InvalidTicket`, `AlreadyJoined`, `NotJoined`, `AlreadyOpen`, `Removed`, `NoHost`,
  `Timeout`, `Io`, `Internal`), plus `InvalidArgument` (a node id for `remove` that is
  not 64 hex digits), `Closed` (any call after or during `shutdown`) and
  `NotInitialised` (Android, see above). A request that cannot be built (bad method,
  header or path) gets a local `400` response instead.
- **Call `shutdown()`.** It ends calls in flight with `Closed`, stops delivering events
  and releases the directory before it returns. An object that is only closed
  (`close()`/`destroy()`) or garbage-collected stops in the background, and its
  directory stays locked until then (`AlreadyOpen` meanwhile).
- `crates/mesh-client-ffi/jvm-test` is the JVM test of the bindings
  (`crates/mesh-client-ffi/jvm-test/run.sh`): it loads the host library and checks that
  errors arrive as the right `MeshClientException`.
