# Sub-spec 4 — Gateway, admin API, `meshd`, desktop OS services

**Date:** 2026-11-10
**Status:** Draft, awaiting owner review
**Parent:** `2026-11-08-meshnode-design.md` (§8 client access, §9 admin API and CLI, §10.1 desktop daemon, §12, §14 row 4)
**Builds on:** `2026-11-09-node-core-design.md` (sub-spec 2) and `2026-11-09-runtime-services-design.md` (sub-spec 3)
**Delivers:** crates `mesh-gateway` (localhost HTTP gateway, `httpfwd` forwarding, port mappings, status page) and `mesh-admin` (admin protocol, server, client); the `apps/meshd` binary (CLI, daemon, OS services, logging); additions to `mesh-proto`, `mesh-core`, `mesh-runtime` and `mesh-services`.

---

## 1. Goal and success criteria

Make a node usable as an always-on desktop service: browsers and local apps reach any mesh service through the node's own localhost gateway (forwarded to another host when this node does not run it), and the owner manages the node through a `meshd` CLI talking to a daemon installed as an OS service.

Done when:
1. Every test in §8 passes.
2. `cargo test --workspace --all-features`, `clippy -D warnings`, `fmt --check` pass on Windows and Linux; `cargo build -p meshd` passes on macOS in CI; the Android job builds `mesh-gateway` and `mesh-admin`.
3. The Windows part of the manual check (§8.3) has been run and recorded in `docs/manual-checks/sub-spec-4.md`; the Linux (systemd, in a VM) part is recorded as pending.
4. The back-ports in §2 are made to the master spec.

New dependencies (workspace): `hyper` 1, `hyper-util` (server, tokio), `http-body-util`, `clap` 4 (derive), `toml`, `qrcode`, `tracing-appender`, `windows-service` (Windows), `windows-sys` features for named pipes, security descriptors and the Event Log (Windows).

## 2. Changes to the parent specs (made here, to be back-ported)

1. **Extra gated protocols in `mesh-core`.** `NodeConfig.extra_protocols: Vec<(Vec<u8>, Arc<dyn DynProtocolHandler>)>` (ALPN, handler) is registered on the node's `Router` behind the membership `Gate`. `NodeHandle::connect(peer, alpn) -> Result<Connection, NodeError>` dials an effective member. `mesh-core` stays HTTP-agnostic.
2. **The CLI talks to the running daemon.** Except `init`, `join`, `reset-identity` (through the daemon when it is reachable, otherwise directly on the data directory) and `service install|uninstall|start|stop|status` (the OS), every command needs a running daemon and otherwise prints `meshd is not running (start it with "meshd run" or "meshd service install")`. This narrows master §9's "or works on the data directory directly when no daemon is running".
3. **The daemon can run uninitialised.** `meshd run` without `mesh.json` serves only the admin API, accepting `Init` and `Join`; on success it starts the node in place.
4. **Logging.** A daily rolling file `<data>/logs/meshd.log.<date>` (7 kept) plus stdout; under systemd stdout reaches journald. On Windows only service start/stop and fatal startup errors go to the Event Log (source `Meshnode`).
5. **Bodies stay buffered** end to end (gateway, `httpfwd`, services), with caps.
6. **macOS is best-effort**: the launchd plist and install code are written and built in CI, not verified on a Mac here. Linux (systemd) is verified later in a VM.
7. **Resource limits carried over from sub-spec 3 (I5)**: a node-wide memory budget for WASM requests, static requests counted in flight, and a compact memory reservation on Android (§4.4).

## 3. `httpfwd` forwarding (`mesh-gateway`)

### 3.1 Wire format (`mesh-proto/src/httpfwd.rs`)

ALPN `meshnode/httpfwd/1` (the mesh's own request-forwarding protocol between nodes, version 1; browsers and apps speak plain HTTP/1.1 to the gateway, §4). One bidirectional QUIC stream per request; frames use the existing codec (`u32` big-endian length, then the version byte and postcard).

```rust
pub struct ForwardRequest  { pub service: String, pub method: String, pub path_and_query: String,
                             pub headers: Vec<(String, Vec<u8>)>, pub body: Vec<u8> }
pub struct ForwardResponse { pub status: u16, pub headers: Vec<(String, Vec<u8>)>, pub body: Vec<u8> }
```

Validation on decode: `service` is a valid service name; `method` is a valid HTTP token; at most 128 headers and 64 KiB of header names and values in total; `path_and_query` ≤ 8 KiB and starts with `/`. Frame cap: 64 MiB + 64 KiB. Hop-by-hop headers (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `TE`, `Upgrade`, `Proxy-Authenticate`, `Proxy-Authorization`, `Proxy-Connection`) are stripped in both directions; `Host` passes through unchanged.

### 3.2 Serving side

Registered through `extra_protocols`, so only effective members get in. Each stream: read the request frame within 30 s (else the stream is reset), answer with `Services::handle(service, request)` — which serves **locally only**, so a forwarded request is never forwarded again — and write the response frame. At most 256 concurrent streams per connection; WASM and static work is bounded by the services' in-flight and memory limits.

### 3.3 Forwarding side

`Services::local_state(name) -> LocalState { Active, NotPlaced, NotReady, Frozen, NotDeployed }` (new in `mesh-services`). The gateway:

- `Active` → serve locally.
- `NotDeployed` → `404 Service not deployed`.
- `NotPlaced`, `NotReady`, `Frozen` → forward. A client-role node always forwards.

Candidates: online effective hosts in `placed_hosts(manifest)`, excluding this node, ordered by recent RTT (unknown RTT last). Up to **3 attempts**; each attempt times out after the manifest's `request_timeout_ms` + 5 s. Connections are cached per peer and dropped on error. The next candidate is tried after a connect or stream error, a timeout, or a `503` whose body is one of `Service not placed on this node`, `Service not yet replicated`, `Node re-bootstrapping`; any other response is returned as is. If no candidate exists or all attempts fail: `502 No host could serve <name>`, with each host's failure logged at `warn`. Forwards count against a per-node forwarding semaphore (64).

## 4. The localhost gateway (`mesh-gateway`)

### 4.1 Server

hyper 1 + hyper-util, HTTP/1.1. Listeners: `gateway.listen` (default `127.0.0.1:7777`) and one per port mapping (default `7801 → sync`). Addresses must be loopback (`127.0.0.0/8` or `::1`); anything else is rejected at startup. If the main listener fails to bind, startup fails with a clear error; a mapped port that fails to bind is logged and skipped. At most 256 open connections; 30 s to read request headers; 60 s keep-alive idle timeout. Request bodies are read up to 64 MiB; beyond that `413`.

### 4.2 Routing

The `Host` header is lower-cased and its port removed.

| Request | Handled as |
|---|---|
| main port, Host `<svc>.mesh.localhost` | service `<svc>` (§3.3) |
| main port, Host `mesh.localhost`, `localhost`, `127.0.0.1` or `[::1]` | status page (§4.3) |
| main port, any other Host | `404 Unknown host` |
| mapped port, Host `127.0.0.1`, `localhost` or `[::1]` | that port's service |
| mapped port, any other Host | `421 Misdirected Request` |

The last two rules are DNS-rebinding protection: a web page whose domain resolves to 127.0.0.1 must not reach a mapped service through the browser.

### 4.3 Status page

`GET /` returns a read-only HTML page: this node (short id, name, status, mesh id), a members table (role, platform, tags, online, RTT, lag), a services table (version, local state, placed hosts, data size) and the gateway URLs. Every value is HTML-escaped. `GET /status.json` returns the same data as JSON. No other paths (`404`), no state changes, no CORS headers.

### 4.4 Resource limits (from sub-spec 3, I5)

- **Memory budget.** `ServicesConfig.max_memory_mb` (default: half of physical RAM, capped at 4096). `mesh-runtime` holds a weighted semaphore counted in MiB; each WASM request reserves its `limits.memory_mb` before instantiation. Waiting counts against the request timeout; if it elapses: `503 Memory budget exhausted`. A request whose `memory_mb` exceeds the whole budget gets that `503` immediately.
- **Static requests count in flight.** A static-site request takes a slot from the runtime's in-flight semaphore (no memory reservation; its read is capped at 64 MiB).
- **Compact memory.** `RuntimeConfig.compact_memory: bool` (default `true` on Android, `false` elsewhere) configures wasmtime with a small memory reservation and a 64 KiB guard region so memories are movable instead of each reserving 4 GiB plus guard of address space. The exact `Config` calls are chosen in the plan from the wasmtime 50 API. Verified on the phone in sub-spec 5; here unit-tested by building the engine and serving a request.

## 5. Admin API (`mesh-admin`)

### 5.1 Protocol

Same frame codec; frames capped at 64 MiB. The client sends `Hello { version: 1 }`; the daemon answers `Hello { version: 1, state: DaemonState }` with `DaemonState = Uninitialised | Running | Removed | Rebootstrapping`. A version mismatch closes the connection with a readable error. Then requests and responses alternate on the connection.

```rust
pub enum AdminRequest {
    Status, Members, Services,
    Init { name: String },
    Join { ticket: String, name: String },
    ResetIdentity { confirm: bool },
    Invite { role: Role, ttl_secs: u32 },
    Remove { id: NodeId },
    SetInfo { name: Option<String>, tags: Option<Vec<String>> },
    SetTagsOf { id: NodeId, tags: Vec<String> },
    DeployWasm { name: String, wasm: Vec<u8>, opts: DeployOpts },
    DeployStatic { name: String, opts: DeployOpts, files: Vec<FileMeta> },   // then one FileData frame per file
    Undeploy { name: String }, Purge { name: String },
    SetPlacement { name: String, placement: Placement },
    SetOutbound { name: String, outbound: Vec<String> },
    SetLimits { name: String, limits: Limits },
    ConfigGet { key: Option<String> },
    ConfigSet { key: String, value: String },
    LogsTail { lines: u32, follow: bool },                                   // answered by a stream of LogLines frames
}
pub struct FileMeta { pub path: String /* relative, '/'-separated */, pub size: u64 }
pub struct FileData { pub bytes: Vec<u8> }
pub enum AdminResponse {
    Status(StatusInfo), Members(Vec<MemberInfoView>), Services(Vec<ServiceInfoView>),
    Ticket { text: String }, Manifest(ServiceManifest), Config(Vec<(String, String)>),
    ConfigSet { restart_required: bool }, LogLines(Vec<String>), Ok,
    Error { code: ErrorCode, message: String },
}
pub enum ErrorCode { NotRunning, WrongState, InvalidArgument, NotFound, JoinRefused(RefuseReason), LastHost, Busy, Internal }
```

- `Init`, `Join` are accepted only when `Uninitialised`. `ResetIdentity` only when `Removed` or `Uninitialised`, and only with `confirm: true`; it stops the node and wipes the data directory except `meshd.toml`.
- **Static upload.** The daemon may run as another user and cannot read the caller's files, so the client streams them: `DeployStatic` lists the files (relative `/`-separated paths validated like static-site paths: no `..`, `.`, empty segments, backslashes, NUL, absolute paths); each `FileData` frame carries one file (≤ 64 MiB); the total is capped at 1 GiB and must match the declared sizes. The daemon writes them to a fresh directory under `<data>/tmp/`, calls `deploy_static`, and deletes the directory.
- **`LogsTail`** returns the last `lines` lines (≤ 10 000) of today's log file, then, if `follow`, new lines as they are written until the client disconnects.
- At most 16 concurrent admin connections; requests on one connection are handled in order.

### 5.2 Transport and access

| OS | Endpoint | Access |
|---|---|---|
| Linux | `<data>/admin.sock` | owner `meshnode`, group `meshnode`, mode `0660`; `service install` adds the installing user to the group |
| macOS | `<data>/admin.sock` | group `admin`, mode `0660` (best-effort) |
| Windows | named pipe `\\.\pipe\meshnode-admin` for the default data directory, else `meshnode-admin-<first 8 hex of blake3(canonical data dir)>` | DACL: the daemon's account, Administrators, and the SID recorded in `meshd.toml` `[admin] windows_user_sid` |

Authentication is operating-system permissions only: whoever can open the endpoint is an administrator. When the daemon runs in the foreground as a normal user (development), the endpoint is restricted to that user.

## 6. `apps/meshd`

### 6.1 Data directory and config

Default data directory: Linux `/var/lib/meshnode`, Windows `%ProgramData%\meshnode`, macOS `/Library/Application Support/meshnode`; overridden by `--data` or `MESHD_DATA`.

`<data>/meshd.toml` (written by `init`/`join`, changed by `config set`):

```toml
name = "pc"
tags = ["desktop"]
relay = "default"          # "default" | "disabled" | "https://relay.example"
discovery = true
log_level = "info"

[gateway]
listen = "127.0.0.1:7777"
ports = { 7801 = "sync" }

[services]
max_memory_mb = 4096

[admin]
windows_user_sid = "S-1-5-21-…"   # Windows only, set by `service install`
```

`config set` validates the key and value (loopback addresses, service names, ports 1024–65535, memory 64–65536) and reports `restart_required` (true for `gateway.*`, `relay`, `discovery`, `services.max_memory_mb`; `name` and `tags` apply at once through `SetInfo`).

### 6.2 CLI

| Command | Needs daemon |
|---|---|
| `run [--service]` | — (is the daemon) |
| `init --name N`, `join <ticket> --name N`, `reset-identity [--yes]` | no (through the daemon if reachable) |
| `status`, `members`, `invite [--client] [--ttl 15m]`, `remove <id-prefix>` | yes |
| `services`, `deploy wasm <name> <file> [opts]`, `deploy static <name> <dir> [opts]`, `set-placement`, `set-outbound`, `set-limits`, `undeploy <name>`, `purge <name> [--yes]` | yes |
| `config get [key]`, `config set <key> <value>`, `logs [-n 100] [-f]` | yes |
| `service install [--data DIR] \| uninstall [--purge-data] \| start \| stop \| status` | no |

Deploy options: `--tags a,b` or `--nodes id,..` (not both), `--outbound host:port,..` (WASM only), `--memory-mb`, `--timeout-ms`, `--max-body`. `invite` prints the ticket text and a terminal QR code. `remove <id-prefix>` resolves a unique member prefix. Output is plain-text tables; `--json` on read commands. Exit code 0 on success, 1 on an error reported by the daemon, 2 on usage errors, 3 when the daemon is not running.

A local `join` (no daemon) starts the node, joins, waits up to 60 s for the first sync, and shuts down; the daemon later continues.

### 6.3 `run`

1. Load `meshd.toml` (create defaults if missing) and set up logging (§6.5).
2. If `mesh.json` exists: `Node::open` → `Services::start` (with `extra_protocols` carrying the `httpfwd` handler) → gateway → admin server. Otherwise start only the admin server (`Uninitialised`).
3. On Ctrl-C, SIGTERM or a Windows service stop: stop accepting admin connections → stop the gateway → `Services::shutdown` → `Node::shutdown`, all within 20 s.

### 6.4 OS services

Install and uninstall require root or Administrator and refuse with a clear message otherwise.

- **Linux.** Copy the binary to `/usr/local/bin/meshd`; create system user and group `meshnode` (`useradd --system`); add `$SUDO_USER` to the group; write `/etc/systemd/system/meshnode.service` (`User=meshnode`, `Group=meshnode`, `StateDirectory=meshnode`, `After=network-online.target`, `Wants=network-online.target`, `Restart=always`, `ExecStart=/usr/local/bin/meshd run --data /var/lib/meshnode`); `systemctl daemon-reload`; `systemctl enable --now meshnode`.
- **Windows.** Copy the binary to `%ProgramFiles%\Meshnode\meshd.exe`; register service `meshnode` (display name `Meshnode`, account `NT AUTHORITY\LocalService`, automatic delayed start, restart on failure three times after 5 s) with arguments `run --service --data %ProgramData%\meshnode`; create the data directory with a DACL for LocalService and Administrators; record the installing user's SID in `meshd.toml`; register the Event Log source `Meshnode`; start the service.
- **macOS (best-effort).** Copy the binary to `/usr/local/bin/meshd`; write `/Library/LaunchDaemons/net.meshnode.meshd.plist` (`RunAtLoad`, `KeepAlive`, runs as root in this version); `launchctl bootstrap system <plist>`.

Uninstall stops and removes the service and keeps the data directory unless `--purge-data`. `service status` reports installed / running / stopped.

### 6.5 Logging

`tracing` to stdout and to a daily rolling file `<data>/logs/meshd.log.<date>` (`tracing-appender`, 7 files kept). The level comes from `log_level`, overridden by `RUST_LOG`. On Windows, service start, stop and fatal startup errors are also written to the Event Log under source `Meshnode`.

## 7. Errors

| Situation | Behaviour |
|---|---|
| Daemon not running (CLI) | message from §2.2, exit code 3 |
| Main gateway port in use | `meshd run` fails with the address in the message |
| Mapped port in use | logged at `error`, other listeners keep serving |
| Admin endpoint already in use (another daemon on the same data dir) | `meshd run` fails: `another meshd is already running for <data>` |
| Install without privileges | `service install requires root` / `… requires Administrator` |
| No host can serve a service | `502 No host could serve <name>` |
| Memory budget exhausted | `503 Memory budget exhausted` |
| Admin version mismatch | `meshd and the running daemon are different versions (cli N, daemon M)` |

## 8. Testing

### 8.1 Automated

| Crate | Tests |
|---|---|
| `mesh-proto` | forward request/response round trip; header count, header bytes, path and frame limits; version byte |
| `mesh-core` | `extra_protocols_are_gated` (non-member refused on the extra ALPN); `node_handle_connect_reaches_member` |
| `mesh-runtime` / `mesh-services` | `memory_budget_waits_then_503`; `request_larger_than_budget_503`; `static_counts_in_flight`; `compact_memory_engine_serves`; `local_state_reports_each_state` |
| `mesh-gateway` (multi-node) | `host_routing_to_local_service`; `unknown_host_404`; `status_page_and_json` (escaped, read-only); `mapped_port_serves_service`; `mapped_port_rejects_foreign_host_421`; `forward_to_placed_host`; `forward_skips_unreachable_then_succeeds`; `forward_all_fail_502`; `forwarded_request_never_reforwarded`; `client_role_always_forwards`; `oversized_body_413`; `non_loopback_listen_rejected`; `hop_by_hop_headers_stripped` |
| `mesh-admin` | `hello_version_mismatch_closes`; every request against an in-process daemon over a real socket/pipe; `uninitialised_accepts_only_init_join`; `deploy_static_upload` (including traversal and size-mismatch rejection); `logs_tail_follow_streams`; `reset_identity_only_when_removed`; `error_codes_render`; `endpoint_in_use_refused` |
| `apps/meshd` | CLI parsing; `cli_reports_daemon_not_running` (exit 3); `init` and `join` locally and through the daemon; `config set` validation; systemd unit and launchd plist golden files; end-to-end with two `meshd run` child processes on temporary data dirs: deploy a static site on A, fetch it through B's gateway with an HTTP client, then remove B and see B refused. The Windows service install/uninstall test is `#[ignore]` (needs Administrator) |

CI: add a macOS job (`cargo build -p meshd`); the Android job builds `mesh-gateway` and `mesh-admin`.

### 8.2 Manual check (`docs/manual-checks/sub-spec-4.md`)

1. Windows, elevated: `meshd service install`; `meshd init --name pc`; `meshd status` shows Running.
2. Second node (another Windows PC now, or a Linux VM later) with the service installed: `meshd join <ticket from "meshd invite">`.
3. `meshd deploy static site <dir>` on one node; open `http://site.mesh.localhost:7777` in a browser on the other.
4. Deploy the `counter` fixture as `counter`; call it through both gateways; `meshd config set gateway.ports 7801=counter`, restart, and `http://127.0.0.1:7801` serves it.
5. Reboot one node: the service returns without anyone logging in, and the mesh reconnects (`meshd members`).
6. `meshd remove <other>`; `meshd service uninstall` on both.

Linux (systemd in a VM): pending. macOS: untested.

## 9. Out of scope here

The Tauri UI (sub-spec 6); the Android shell and the in-process admin channel (sub-spec 5); installers (MSI, deb/rpm, dmg — sub-spec 8); streaming bodies; public ingress (master §15).
