# Sub-spec 6: Tauri management UI (`apps/ui`, `mesh-ui`, `apps/desktop`)

> Master spec: `docs/superpowers/specs/2026-11-08-meshnode-design.md` (§9 "Tauri 2 UI", §14 row 6, success
> criterion 2). Previous sub-spec: `2026-11-10-android-shell-design.md`.

## 1. Intent

**What the owner asked for:** continue the master plan with sub-project 6, the full management UI on desktop and
Android, including QR invite/join (master §14 row 6). Decisions taken in brainstorming:

- **Frontend stack:** Svelte 5 + Vite + TypeScript, built to static files under the existing strict CSP.
- **Polish:** a polished product feel — a distinctive visual identity, restrained motion, light/dark, phone and
  desktop layouts — not a bare admin form.
- **Desktop without a daemon:** offer both **Install as system service** (elevated, behind a UAC prompt the owner
  approves) and **Run without installing** (`meshd run` as the user).
- **Approach A:** a shared Rust crate `mesh-ui` holds the typed Tauri commands once, over an `AdminTransport`
  trait (desktop: the admin pipe/socket; Android: in-process). TS types are generated from Rust with `ts-rs`.

**Assumptions (designer, not owner):**
- The admin API (sub-spec 4) already covers every screen; new admin fields are added only where a screen needs
  data the views lack. No new admin requests and no daemon event push (the UI polls).
- Installers and signing stay in sub-spec 8; the desktop app is built with `cargo tauri build --no-bundle` or run
  with `cargo tauri dev`.
- The elevated service install is never run by tests or by the implementer; it is in the owner's manual check.

### 1.1 Done criteria
1. All tests in §9 pass on Windows; the CI jobs in §9.5 are green on Linux.
2. `svelte-check` reports 0 errors; the generated TS types are current (CI drift check).
3. The Android APK and the desktop app both build and load the same `apps/ui/dist`.
4. The owner's manual check (§9.6) is run on Windows and the phone; step 2 is success criterion 2 (pairing within
   30 s, on Wi-Fi and on mobile data).
5. The master spec is back-ported (§10).

## 2. Layout and crates

```
apps/ui/                        Svelte 5 + Vite + TS; one frontend, built to apps/ui/dist
  package.json, package-lock.json
  src/lib/api.ts                typed wrappers over invoke()/Channel; the only module that imports @tauri-apps/api
  src/lib/generated/            ts-rs output (checked in; CI regenerates and diffs)
  src/lib/stores.ts             status / members / services stores and the poller (§6.4)
  src/lib/design/               tokens, components (Card, Sheet, Confirm, Chip, Monogram, Ring, Toast, Banner)
  src/routes/                   FirstRun, Home, AddDevice, Join, Services, ServiceDetail, Deploy, Settings, Logs,
                                NoDaemon (desktop), NodeStopped (Android)
  static/fonts/                 Inter, JetBrains Mono (woff2, SIL OFL licences checked in)
crates/mesh-ui/                 shared command layer; depends on mesh-admin (requests and views), qrcode, ts-rs;
                                no tauri dependency, so its tests need no webview
crates/mesh-ui-tauri/           (as built) the #[tauri::command] wrappers and Channel plumbing, shared by both
                                shells; its own Cargo workspace (outside the root one)
apps/desktop/src-tauri/         new Tauri 2 app, its own Cargo workspace and Cargo.lock (like apps/android)
apps/android/src-tauri/         existing app; frontendDist moves to ../../ui/dist; the placeholder src/ is deleted
apps/android/.../app/           Kotlin: a small Tauri plugin `FolderPicker` (SAF tree) (§7.2)
```

- `mesh-ui` is a member of the root workspace (desktop CI builds and tests it without webview packages). The
  `#[tauri::command]` wrappers and the `Channel` plumbing live in `crates/mesh-ui-tauri`, used by the two app
  workspaces (as built; not a `tauri` feature of `mesh-ui`); the logic below them is plain async Rust tested in
  the root workspace.
- Both Tauri configs: `build.beforeBuildCommand = "npm --prefix ../ui run build"`, `beforeDevCommand` runs
  Vite's dev server (`npm --prefix ../ui run dev`) (as built: Tauri runs the hooks from the project root, the
  parent of `src-tauri`, so the path is `../ui`, not `../../ui`), `frontendDist = "../../ui/dist"` (relative to
  `src-tauri`). The CSP stays as in sub-spec 5 (no remote fonts, scripts
  or images; `img-src 'self' data:`).

## 3. `mesh-ui` (the shared command layer)

### 3.1 Transport

```rust
#[async_trait]
pub trait AdminTransport: Send + Sync + 'static {
    /// One request, one final response (fresh connection per call on desktop).
    async fn request(&self, req: AdminRequest) -> Result<AdminResponse, UiError>;
    /// Streams: LogsTail { follow } and Probe. Lines arrive on `tx`; ends with Ok or Error.
    async fn stream(&self, req: AdminRequest, tx: mpsc::Sender<String>) -> Result<(), UiError>;
    /// Static upload from a file source (§3.4), reporting bytes sent.
    async fn deploy_static(&self, opts: DeployOpts, src: Box<dyn FileSource>,
                           progress: mpsc::Sender<u64>) -> Result<ServiceManifest, UiError>;
}
```

- **Desktop:** `PipeTransport { endpoint: Endpoint }` over `mesh_admin::AdminClient` (connect per call).
- **Android:** `InProcessTransport` over the host's `InProcessAdmin`. `mesh-ffi` gains a Rust-only (not UniFFI)
  `pub fn in_process_admin() -> Option<InProcessAdmin>` returning the running host's admin (`None` when the node
  is stopped → `UiError::NotRunning`). No postcard round trip for the webview.
- **Tests:** the same `InProcessTransport` over a test `Daemon` (`mesh-host` test-hooks).

### 3.2 Commands

`Commands<T: AdminTransport>` holds the transport; each method returns `Result<Dto, UiError>`:

| Command | Admin request(s) | Returns |
|---|---|---|
| `status` | `Status` | `StatusDto` (state, node id, name, role, tags, members total/online, services, gateway URL, ports) |
| `members` | `Members` | `Vec<MemberDto>` (adds `accent_hue`, derived from the node id, §6.1) |
| `services` | `Services` | `Vec<ServiceDto>` |
| `init(name)` / `join(ticket, name)` | `Init` / `Join` | `StatusDto` (re-read after success) |
| `invite(role, ttl_s)` | `Invite` | `InviteDto { text, qr_svg, expires_at_ms }` |
| `remove(id)`, `set_info(name, tags)`, `set_tags_of(id, tags)` | as named | `()` |
| `deploy_wasm(path, opts)` | `DeployWasm` | `ServiceDto` |
| `deploy_static(source, opts, progress)` | `DeployStatic` | `ServiceDto` |
| `undeploy(name)`, `purge(name)`, `set_placement`, `set_outbound`, `set_limits` | as named | `()` |
| `config_get()` / `config_set(key, value)` | `ConfigGet` / `ConfigSet` | `Vec<(key, value)>` / `{ restart_required }` |
| `logs_follow(lines, channel)` | `LogsTail { follow: true }` | streams lines; ends when the channel drops |
| `probe(node, count, interval_ms, timeout_ms, channel)` | `Probe` | streams `ProbeSample { seq, ms?, connect_ms?, error? }` (parsed from the admin's lines) |
| `reset_identity()` | `ResetIdentity` | `StatusDto` |
| `platform()` | — | `"desktop-windows" \| "desktop-linux" \| "desktop-macos" \| "android"` (set by the shell) |

- **QR:** `invite` renders the ticket text with the `qrcode` crate to a compact SVG string (error-correction M,
  quiet zone 4, `currentColor` modules so it themes). The page inserts it as an SVG element built from the
  string's path data — never `innerHTML` of untrusted input (the SVG is ours, but the rule is uniform).
- **Streams:** (as built) every stream message is a `StreamEvent` (`Item { value }`, `End`, `Error { error }`).
  Each stream runs on its own task; the Tauri `Channel::send` failing (page gone) or an explicit
  `cancel(stream_id)` ends the task and drops the admin connection. At most 4 concurrent streams per window.
- **DTOs** derive `serde::Serialize` and `ts_rs::TS` and are UI-shaped (ids as hex strings, times as ms numbers,
  enums as tagged unions). Admin view types are not exposed to TS directly.

### 3.3 Errors

```rust
#[derive(Serialize, TS)]
pub struct UiError { pub code: UiErrorCode, pub message: String, pub hint: Option<String> }
```

`UiErrorCode` = the admin `ErrorCode` variants (with `JoinRefused(reason)` flattened to `JoinRefused` plus a
`reason` field) plus `DaemonUnreachable`, `DaemonStarting`, `NotRunning` (Android node stopped), `Cancelled`,
`PickerCancelled`, `Io`. `hint` is English text written once in Rust, e.g.:

| Code | Hint |
|---|---|
| `JoinRefused(Expired)` | "Ask the other device for a new invite." |
| `JoinRefused(Used)` | "Each invite works once. Ask for a new one." |
| `JoinRefused(InviterUnavailable)` | "Keep Meshnode open on the inviting device and try again." |
| `LastHost` | "Add another host device before removing this one." |
| `DaemonUnreachable` | "Start Meshnode, or install it as a service." |
| `NotRunning` | "Start the node." |

### 3.4 File sources (deploy)

```rust
pub trait FileSource: Send {
    /// Every file, relative `/`-separated paths (mesh-admin's static path rules), sizes, in a stable order.
    fn list(&mut self) -> Result<Vec<FileMeta>, UiError>;
    fn open(&mut self, path: &str) -> Result<Box<dyn Read + Send>, UiError>;
}
```

- `DirSource` (desktop): walks a folder; skips symlinks and hidden OS files (`.DS_Store`, `Thumbs.db`,
  `desktop.ini`); refuses an empty folder and any path that fails mesh-admin's rules (with the offending path in
  the message); caps at the admin's existing upload limits.
- `SafSource` (Android): backed by the Kotlin `FolderPicker` plugin (§7.2), which lists the tree and opens
  documents by URI; same rules.
- `deploy_wasm` on desktop takes a path; on Android the file picker returns a content URI that the plugin copies
  to the app cache first, then the path is used.

## 4. Desktop shell (`apps/desktop/src-tauri`)

### 4.1 Finding the daemon

At start and whenever a request returns `DaemonUnreachable`, `DaemonLocator` probes, in order, and connects to the
first endpoint whose `Hello` answers within 1 s:

1. **System service:** the data directory read back from the installed service registration (`meshd service
   status` logic, reused as a library call from `meshd`'s service module, read-only), else the platform default
   (`%ProgramData%\meshnode`, `/var/lib/meshnode`, `/Library/Application Support/Meshnode`).
2. **Per-user daemon:** `%LOCALAPPDATA%\Meshnode` / `$XDG_DATA_HOME/meshnode` (fallback `~/.local/share/meshnode`)
   / `~/Library/Application Support/Meshnode`.

`connection()` tells the UI which one (`System service` / `Running as you` / `None`). If both answer, the
service wins and the UI shows a one-time notice that a second, separate node is running as the user.

### 4.2 Starting a daemon

- `locate_meshd()`: `meshd[.exe]` next to the app executable, else on `PATH`, else `None` (the UI then offers
  **Locate meshd…**: `locate_meshd_dialog` opens the file dialog in Rust, accepts only a file named `meshd`/`meshd.exe`, canonicalises it and remembers it in the app's config dir; the page never names the path). Relative `PATH` entries are skipped.
- **Run without installing:** spawn `meshd run --data <user dir>` detached (Windows: `DETACHED_PROCESS |
  CREATE_NO_WINDOW`; Unix: `setsid`), stdout/stderr to `<user dir>/meshd.log`. Poll the endpoint for up to 10 s
  (`DaemonStarting` meanwhile). The daemon outlives the UI.
- **Install as system service:** `install_command(meshd, data) -> (program, args)` is a pure function (tested).
  Windows runs it through `ShellExecuteExW` with the `runas` verb and waits for the process; a declined UAC prompt
  (`ERROR_CANCELLED`) maps to `Cancelled` ("Installation cancelled"). Linux uses `pkexec`; macOS uses
  `osascript … with administrator privileges`. Then poll the service endpoint for up to 30 s.
- Nothing in tests or CI calls the elevated path; tests cover `install_command` and the spawn of `meshd run`
  against a temp data directory.

### 4.3 Desktop-only commands
`connection`, `locate_meshd`, `locate_meshd_dialog`, `daemon_run_user`, `daemon_install`, `pick_folder`, `pick_file`
(`tauri-plugin-dialog`), `open_url` (the gateway URL in the default browser, `tauri-plugin-opener`, only
`http://*.localhost:*` and `http://127.0.0.1:*`).

## 5. Android shell changes (`apps/android`)

- The placeholder page and its commands (`status`, `join`) are replaced by the shared `mesh-ui` commands over
  `InProcessTransport`; `service_start` / `service_stop` stay; new `power_policy_get` / `power_policy_set`
  (through `mesh_ffi::set_power_policy`, which persists to meshd.toml, unchanged).
- **QR scan:** `tauri-plugin-barcode-scanner` (QR only). The camera permission is requested explicitly before
  scanning, when the user taps Scan (as built: an explicit permission request, not left to the scan call); a denial shows "Camera access is off — paste the invite instead" with the paste field.
- **When the node is stopped:** (as built) the UI uses `node_running()` to know the node is stopped and shows
  **Node stopped** with **Start** (service control). Admin calls, `status()` included, return `NotRunning`;
  `status()` does not answer while the node is stopped.
- Capabilities: only the commands and plugin permissions listed above; CSP unchanged.

## 6. Frontend

### 6.1 Visual identity
- **Theme:** "a calm network of your own devices". Dark: soft graphite surfaces; light: warm paper. Tokens on
  `:root` (colour, spacing 4-pt scale, radii, elevation, motion durations), switched by `prefers-color-scheme`
  with a manual override in Settings.
- **Device accent:** each node gets a hue from its id (`accent_hue = u16::from_be_bytes(id[0..2]) % 360`, from
  Rust), used for its monogram (two letters of its name) and card edge, identical on every screen and platform.
- **Type:** Inter (UI), JetBrains Mono (ids, tickets, logs); bundled woff2.
- **Motion:** a gentle pulse on online dots; cards slide in when a member appears; the invite's ring countdown;
  sheet and toast transitions ≤ 200 ms. Everything off under `prefers-reduced-motion`.
- **Layout:** desktop — left sidebar, content max 1100 px, works down to an 800 px window; phone — bottom tabs
  (Home, Services, Logs, Settings), a floating **Add device** button on Home, safe-area insets respected,
  touch targets ≥ 44 px.
- **Accessibility:** WCAG AA contrast in both themes, visible focus rings, every control keyboard-reachable on
  desktop, status conveyed by text not only colour.

### 6.2 Screens
- **First run** (state `Uninitialised`): two cards — *Create a new mesh* / *Join a mesh*; both ask for a device
  name prefilled from the hostname or phone model.
- **Home:** hero strip — mesh status ("3 of 4 devices online"), the gateway URL (tap to copy; desktop also opens
  it), mapped ports. Device cards: monogram, name, platform icon, tags, online state, RTT, "last seen 2 min ago",
  replication lag; this device pinned first ("This device").
- **Add device:** role (host/client) and expiry (default 15 min; 5/15/60) → large QR, ticket text with copy, ring
  countdown; on a new member id appearing in `members`, a success state ("Pixel joined") with its card animating
  in; on expiry, "Invite expired" with **New invite**.
- **Join:** phone — full-screen scanner with "Paste instead"; desktop — paste box. Errors show the reason and the
  hint.
- **Services:** list (kind, version, the hosts running it as monograms, local state, data size). **Service
  detail** sheet: placement (tag chips), outbound allowlist editor, limits, **Undeploy**, **Purge**. **Deploy**
  sheet: choose a `.wasm` file or a site folder → name → placement → progress bar → done.
- **Settings:** name and tags, gateway port mappings, relay URL, theme, Android power policy (three cards
  explaining the battery trade-off), the latency probe (pick a device → live chart of samples, then p50/p95/max),
  and a **Danger zone** with Reset identity. Removing another device is on that device's card menu on Home.
- **Logs:** live tail in mono, level filter chips, pause/resume, search; virtualised list holding the last 10 000
  lines.
- **No daemon** (desktop) and **Node stopped** (Android) as in §4 and §5.

### 6.3 Confirmations and errors
- Remove, Purge, Reset identity and Undeploy use a confirmation sheet where the user types the device or service
  name, and that states the honest limit ("a removed device can never rejoin with the same identity"; "purge
  hides all of this service's data on every device").
- Errors: an inline message with the `hint` near the control that failed; a top **banner** when the daemon is
  unreachable or the node is stopped (with the desktop/Android actions); toasts only for success.

### 6.4 State and polling
- Stores `status`, `members`, `services`. One poller refreshes them every 2 s while `document.visibilityState ===
  "visible"`, pauses when hidden (also covers the Android app in the background), and refreshes immediately
  after any mutating command. Requests never overlap (a slow poll skips the next tick).
- Add device detects the new member through this poll; no admin events.

## 7. Platform specifics

### 7.1 Desktop dialogs
`tauri-plugin-dialog` for file/folder pick and **Locate meshd…**.

### 7.2 Android `FolderPicker` plugin (Kotlin)
- `pickTree()` → `ACTION_OPEN_DOCUMENT_TREE`, returns a tree URI (with a persisted read grant for the deploy only).
- `list(treeUri)` → walks `DocumentsContract` children, returning `(relativePath, size, documentUri)`; the
  relative-path mapping is a pure Kotlin function (JVM-tested).
- `read(documentUri, offset, len)` → bytes, used by `SafSource::open`.
- `pickFile(mime)` → copies the document to the app cache and returns the path (for `.wasm`).

## 8. Errors (summary)

| Case | UI behaviour |
|---|---|
| Desktop daemon unreachable | Banner + **No daemon** screen (Run without installing / Install as service / Locate meshd) |
| UAC declined | "Installation cancelled", no state change |
| Daemon started but not answering in time | "Meshnode didn't start" with a link to `meshd.log` |
| Android node stopped | **Node stopped** with Start |
| Join refused | reason + hint on the Join screen |
| Invite expired before scan | Add device shows "Invite expired" + New invite |
| Deploy: empty folder / bad path / too large | inline, naming the file |
| Camera denied | paste fallback |
| Stream connection lost | the log/probe view shows "Disconnected — Resume" |

## 9. Testing

### 9.1 `mesh-ui` (Rust, root workspace)
Against a test `Daemon` through `InProcessTransport`:
- every command's mapping, including init → status, invite → join between two in-process nodes, remove;
- each `UiErrorCode`, each join-refusal reason and its hint;
- `invite`'s SVG decodes back to the ticket text (`rqrr` dev-dependency on a rasterised SVG via `resvg`);
- `logs_follow` stops and drops its connection when the receiver is dropped; the 4-stream cap;
- `probe` line parsing (sample, error, malformed);
- `DirSource`: path rules, empty folder, symlink skipped, hidden OS files skipped, stable order;
- `accent_hue` is stable and in 0..360.

### 9.2 Desktop shell (`apps/desktop/src-tauri`)
- `DaemonLocator` order and the both-running notice, with fake endpoints (temp-dir sockets / pipe names);
- `install_command` per platform (pure);
- spawning `meshd run` against a temp data dir answers within 10 s, then is shut down by the test.

### 9.3 Frontend (`apps/ui`)
Vitest + @testing-library/svelte with `api.ts` mocked:
- the poller pauses when hidden, skips overlapping ticks, refreshes after a mutation;
- invite countdown and the new-member success switch; expiry state;
- typed-name confirmation enables only on an exact match;
- error + hint rendering; daemon-unreachable banner;
- the log view keeps 10 000 lines and stays responsive (render < 50 ms per batch in jsdom).
`svelte-check` with 0 errors; ESLint + Prettier clean.

### 9.4 Android
- JVM test for the `FolderPicker` relative-path mapping.
- Existing Kotlin tests and lint still pass.

### 9.5 CI
- `ui`: `npm ci`, lint, `svelte-check`, vitest, build.
- `ts-types`: regenerate ts-rs output, fail on diff.
- `desktop-ui`: Windows and Ubuntu (with webkit2gtk-4.1 packages): clippy `-D warnings` and tests for
  `apps/desktop/src-tauri`.
- `android-apk`: builds `apps/ui` first.

### 9.6 Manual check (`docs/manual-checks/sub-spec-6.md`, owner, Windows + phone)
1. Fresh PC: the UI offers **Run without installing**; Home appears.
2. Desktop shows an invite QR; the phone scans it; both list each other online within **30 s** — once on Wi-Fi,
   once on mobile data (success criterion 2).
3. Deploy a static site from the phone (folder picker); open it from both gateways.
4. Change placement and outbound; undeploy; purge — with confirmations.
5. Logs: live tail, pause, filter.
6. Latency probe from the phone to the PC.
7. Change the power policy; restart the app; it persists.
8. Remove the phone from the desktop; both UIs reflect it within 60 s.
9. Light/dark, reduced motion, narrow phone width, desktop window at 800 px.
10. **Install as system service** via the UAC prompt (on a VM or spare machine if preferred).

## 10. Back-ports to the master spec
- §3 crate table: add `crates/mesh-ui` and `apps/desktop`; `apps/ui` is the Svelte frontend.
- §9 UI: the screens and decisions above (polling, no-daemon actions, per-user daemon directory).
- §14 row 6 status.

## 11. Out of scope
Installers and signing (sub-spec 8); Aegis (sub-spec 7); localisation; an iOS shell; daemon event push;
macOS service installation beyond what `meshd service install` already does; public access (§15).
