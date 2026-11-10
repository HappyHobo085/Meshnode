# Manual check: sub-spec 3 (runtime and services)

Run by the owner on two desktops (spec §8.4). Until this is done, master spec §14 row 3 stays "implemented, manual check pending".

## Setup

Build once: `cargo build -p mesh-services --example meshdev`, and make sure the `counter` fixture has been built by mesh-runtime's test build, so that `target/fixtures/wasm32-wasip2/release/counter.wasm` exists. Run on each desktop:

```
meshdev <data-dir> init | join <ticket-text> | run
```

Desktop A runs `init` then `invite host`; desktop B runs `join <ticket-text>`. Both must list each other in `members` first. The service commands are:

- `deploy wasm <name> <file> [--tags a,b | --nodes id,..] [--outbound host:port,..]`
- `deploy static <name> <dir> [--tags a,b | --nodes id,..]`
- `services`, `undeploy <name>`, `purge <name>`
- `call <name> <METHOD> <path> [body]`: prints status, headers, then the body

Deploying a name again writes a new manifest version, so step 4 re-deploys with `--nodes`.

## Steps

1. On A: `deploy wasm counter <path to counter.wasm>`. On B, once `services` shows `counter ... local=ready`: `call counter GET /` returns `200` with body `1`.
2. Call `call counter GET /` on A and on B in turn. Both see one shared, increasing count (2, 3, ...).
3. On A: `deploy static site <dir with an index.html>`. On B: `call site GET /` returns `200` with the contents of `index.html`.
4. On A: `deploy wasm counter <path> --nodes <A id prefix>` (placement A only). On B (after `services` shows `local=not placed`): `call counter GET /` returns `503` with "not placed"; A still answers.
5. On A: `undeploy counter`; `call counter GET /` on both returns `404`. Then `purge counter`, redeploy `counter`, and `call counter GET /` returns `1` again.

## Results

| Date | Machines | Networks | Pass/fail (per step 1-5) | Notes |
|---|---|---|---|---|
| | | | | |

## Android build check

`cargo ndk -t arm64-v8a build -p mesh-services` (a build check only; nothing is run on a device).

| Date | Machine | Toolchain | Result |
|---|---|---|---|
| 2026-11-10 | Windows 11 dev machine | cargo-ndk 4.1.2, NDK 27.2.12479018 (`%LOCALAPPDATA%\Android\Sdk\ndk`; `ANDROID_NDK_HOME` pointed at it), Rust target `aarch64-linux-android` | builds, no errors (debug profile, 3m58s cold). `libmesh_services.rlib` is 88 MB (debug); the crate produces an rlib, not a `.so`, so there is no shared-library size to record yet. |

Local smoke test (2026-11-10, same machine, two processes, default relay and discovery): A `init`, `invite host`; B `join`. `deploy wasm counter` on A; on B `services` showed `counter v1 ... local=ready v1` and `call counter GET /` returned `200`, body `1`; then A got `2` and B `3`. `deploy static site <dir>` on A; `call site GET /` on B returned `200` with the `index.html` body, the `etag`, `cache-control: no-cache` and `content-type: text/html; charset=utf-8` headers; `call site GET /nope` returned `404`. Bad input (missing arguments, invalid method, missing option value, unknown member prefix, unknown command) printed a usage or error line without panicking. Both processes exited cleanly on `quit`. Steps 4 and 5 (placement, undeploy, purge) were not run in this smoke test. This does not replace the steps above.
