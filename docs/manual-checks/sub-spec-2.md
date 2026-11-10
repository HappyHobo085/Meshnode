# Manual check: sub-spec 2 (node core)

Run by the owner on two desktops on different networks (spec §11.3). Until this is done, master spec §14 row 2 stays "implemented, manual check pending".

## Setup

Build once: `cargo build -p mesh-services --example meshdev`. Run on each desktop:

```
meshdev <data-dir> init | join <ticket-text> | run
```

then use the REPL: `invite [host|client]`, `members`, `put <ns> <key> <value>`, `get <ns> <key>`, `del <ns> <key>`, `ls <ns> [prefix]`, `remove <id-prefix>`, `status`, `sync`, `blob add <path>`, `blob get <hash> <out>`, `blob getdir <hash>` (collections), `quit`. Node events (`PeerUp`, `PeerDown`, `RemovedSelf`, ...) are logged as they happen; set `RUST_LOG=meshdev=debug` for more. Use `run` to restart a node that already has a data dir.

## Steps

1. Desktop 1: `init`, then `invite`. Desktop 2, on a different network: `join <ticket-text>`. Both show each other online (`members`) within 30 s.
2. Disconnect Desktop 2's network. On both desktops write and delete different keys, and write the same key with different values.
3. Reconnect. Within 60 s both show the same `ls` results and the same winner for the shared key.
4. `blob add <path>` on one desktop, `blob get <hash> <out>` on the other; compare the files.
5. On Desktop 1, `remove <id-prefix>` for Desktop 2. Desktop 2 shows `RefusedAsRemoved` or `Removed` (`status`) within 60 s.

## Results

| Date | Machines | Networks | Pass/fail (per step 1-5) | Notes |
|---|---|---|---|---|
| | | | | |

## Android build check

`cargo ndk -t arm64-v8a build -p mesh-core` (a build check only; nothing is run on a device).

| Date | Machine | Toolchain | Result |
|---|---|---|---|
| 2026-11-09 | Windows 11 dev machine | cargo-ndk 4.1.2, NDK 27.2.12479018 (`%LOCALAPPDATA%\Android\Sdk\ndk`; `ANDROID_NDK_HOME` pointed at it), Rust target `aarch64-linux-android` | builds, no errors |

Local smoke test (2026-11-09, same machine, two processes, default relay and discovery): `init`, `invite`, `join`; `put` on a was read back on b; a write on b appeared in a's `ls`; bad input (unknown command, bad hash, missing arguments) printed a usage or error line without panicking. This does not replace the cross-network steps above.
