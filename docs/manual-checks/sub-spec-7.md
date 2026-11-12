# Manual check: sub-spec 7 (Aegis sync port)

Run by the owner on Windows 11 and the Galaxy S23 (spec `2026-11-11-aegis-sync-design.md` §8.6). Until this is done, master spec §14 row 7 stays "implemented, manual check pending". Steps 4 and 5 are success criterion 4 of the master spec (Aegis desktop and Aegis Android sync bookmarks through their own device's gateway and converge after airplane mode).

Nothing here has been run on a device or with a real Aegis app by the build agent. It never installed services, never touched a phone, and never pushed either repo. The new CI jobs (`aegis-sync` in Meshnode, the `sync-server` and `msrv` changes in Aegis) have **not run anywhere yet**: they only run once the branches are pushed. Look at them the first time you push, and treat a red job as a finding.

The mesh from `docs/manual-checks/sub-spec-6.md` (a PC and the S23 paired, both nodes running) is the starting point. This check deploys a service and registers test devices, so do not use a mesh you cannot afford to disturb.

## Build

### The Meshnode service (on the PC)

```
rustup target add wasm32-wasip2
set CARGO_TARGET_DIR=%LOCALAPPDATA%\meshnode-target
bash scripts/build-services.sh
```

The script runs a locked release build of `services/aegis-sync` for `wasm32-wasip2`. The file is `aegis_sync.wasm` under `<target dir>/wasm32-wasip2/release/`. With `CARGO_TARGET_DIR` set as above that is `%LOCALAPPDATA%\meshnode-target\wasm32-wasip2\release\aegis_sync.wasm`; without it, `services\aegis-sync\target\wasm32-wasip2\release\aegis_sync.wasm` (`D:` is FAT32, so prefer the first). The script's last line prints the path. Deploy with

```
meshd deploy wasm sync <path to aegis_sync.wasm>
```

or in the desktop app (Services, Deploy, choose the `.wasm`, name it `sync`). The name must be `sync`: the default port mapping `7801 -> sync` is what makes `http://127.0.0.1:7801` work. Default limits: 64 MB memory, 30 s per request, 8 MiB body.

The Meshnode desktop app, `meshd` and the Android app are built as in `docs/manual-checks/sub-spec-6.md`. The phone must run the Meshnode APK (package `net.meshnode.app`) so that Aegis can find it.

### Aegis (desktop and APK)

The Aegis work is on branch `feat/sync-core` in `D:/AI_Apps/Aegis` (HEAD `71e78da` when this was written). **It is not pushed**, so there is no CI build and no release: the desktop app and the APK must be built from that branch on this PC. Check out `feat/sync-core` first. The commands are Aegis's own (`D:/AI_Apps/Aegis/AGENTS.md`):

```
cd D:\AI_Apps\Aegis
npm install
npm run tauri:build
```

Keep `CARGO_TARGET_DIR` off the FAT32 `D:` drive, for example `set CARGO_TARGET_DIR=%LOCALAPPDATA%\aegis-target`. This is a release build; use it for the timing check.

Android (from `src-tauri/AGENTS.md`, build section, gotchas 8 and 9, and its debug-build warning):

- `npm run android:build -- --target aarch64` builds an arm64-only **debug** APK. It needs **JDK 21** (Gradle 8.14.3 and AGP 8.11.0 fail under JDK 25, and the JDK 17 on this PC may be too old too, so check before you start). Set `JAVA_HOME` to a JDK 21 (for example Android Studio's JBR).
- A debug build installs as **`com.aegis.browser.debug`**, side by side with any release `com.aegis.browser`. Launch the `.debug` one, and confirm `adb shell dumpsys package com.aegis.browser.debug` shows a `lastUpdateTime` that moved after the install; otherwise you may be testing an old binary.
- A release APK needs `keystore.properties`, or `AEGIS_ALLOW_DEBUG_SIGNED_RELEASE=1` for a debug-key sideload.
- Install with `adb install -r <apk>` (not run by the agent).

## Where things are in Aegis

- **Settings, Sync tab** (Settings modal, group Network, tab **Sync**). With sync not yet set up, the top section **Sync server** has a **Server URL** field.
- **The Meshnode suggestion.** If Aegis finds Meshnode on this device and the URL field does not already hold `http://127.0.0.1:7801`, a box under the URL field reads "Meshnode was found on this device. Its local gateway can be your sync server at `http://127.0.0.1:7801`." with a button **Use Meshnode on this device**. It never applies itself.
  - Desktop detection looks for the **default** admin pipe `\\.\pipe\meshnode-admin` only. Meshnode must be running with its default data directory (a service, or the app's Run without installing). A daemon on a custom data directory is not detected: type the URL by hand.
  - Android detection looks for package `net.meshnode.app` (declared in the manifest `<queries>`).
- **Registration** then follows the normal Aegis flow (Set up sync, or Restore from a recovery phrase). There is no registration code on the mesh service (open registration in v1).
- **Devices** section (once sync is on): this account's devices. A device that is waiting shows "waiting for approval". On the **owner** only, a waiting device has **Approve** (and a reject button). Any device has **Remove** (with a confirmation: it "revokes it permanently", and the device cannot sync again, even with the recovery phrase). Approve and Remove are how steps 3 and 6 are done.
- A device waiting for approval shows "This device is waiting for approval" in the Sync status section; after approval press **Sync now** there.

## Steps (spec §8.6)

Record the date, the device, the network and the result of each in the table.

1. **Build and deploy `aegis-sync`; it runs on both nodes.** Build as above and deploy from the desktop app or `meshd deploy wasm sync <file>`. Pass: Services lists `sync` on the PC and on the phone within 30 s; `http://127.0.0.1:7801/healthz` answers 200 in the PC browser, and the same URL answers 200 in the phone browser.
2. **Aegis desktop: accept the suggestion; register (owner).** Start the Aegis build from `feat/sync-core`. Settings, Sync: the Meshnode box shows. Press **Use Meshnode on this device**, then Set up sync (a passphrase is optional) and register. Pass: the URL field holds `http://127.0.0.1:7801`, sync status is OK, and the Devices list shows this device as "(this device) (owner)". If the box does not show, check that Meshnode runs on its default data directory, and report it.
3. **Aegis on the S23: accept the suggestion; register; approve on the desktop.** On the phone install the Aegis debug APK; Settings, Sync: press **Use Meshnode on this device**, then restore the **same account** from the recovery phrase of step 2. Pass: the phone shows "waiting for approval"; on the desktop, Settings, Sync, Devices lists the phone as waiting and **Approve** is available. After you press Approve and then **Sync now** on the phone, the phone is an approved member within 60 s. Record the seconds.
4. **Sync bookmarks both ways.** Add a bookmark on the desktop and a different one on the phone; Sync now on each. Pass: each bookmark shows on the other device within 60 s. Delete one on each side and check that the deletes arrive too.
5. **Airplane mode (success criterion 4).** Turn on airplane mode on the phone (the Meshnode node and Aegis keep running). Edit on both: add 3 or more bookmarks on each, edit one existing bookmark on both, delete one on each. Turn airplane mode off, wait for the Meshnode phone to rejoin (Home shows the PC online), then Sync now on both. Pass: both devices show the **same** bookmark set, with the same winner for the bookmark edited on both. Record the time from reconnect to convergence.
6. **Revoke the phone from the desktop.** On the desktop, Settings, Sync, Devices: **Remove** the phone and confirm. Pass: the phone is refused within 60 s of the change reaching its node (its next Sync now shows a rejected or revoked state and no further updates arrive), and the desktop keeps syncing. Keep the phone online so the change replicates. Record the seconds.

## Extra checks

- **Large first sync, timing.** Use release builds (the Aegis `tauri:build` output, and a release `meshd`). Put several thousand bookmarks into a fresh Aegis account (a bookmarks HTML import works), then enable sync against `http://127.0.0.1:7801` and time the first full push. Pass: it finishes without a request error and no single request approaches the 30 s service limit. For reference, a 1 000-record push took 7 to 8 s in a **debug** build in the automated test, so a release build should be clearly faster. If any request nears 30 s, record the record count and the time as a finding. A GET page is capped at 6 MiB on the mesh (32 MiB on the native server), so a very large namespace takes several page requests.
- **Android plain HTTP.** Aegis Android does **not** get a cleartext waiver. It was dropped on purpose: Aegis sync uses Rust sockets, and `sync::validated_base` already allows loopback http, so Android's HTTP stack is not involved. Step 3 therefore has to work without one. **If the phone cannot reach `http://127.0.0.1:7801` from Aegis** (while the phone browser can), record it as a finding with the exact error text from the Sync status line. The fallback is to re-add the `network_security_config.xml` cleartext-to-`127.0.0.1` waiver and its manifest attribute (Aegis commit `7311626` added them; `fefa6d0` removed them).
- **Open registration warning.** The node log (`%LOCALAPPDATA%\Meshnode\meshd.log`, or the Logs view) shows an open-registration warning from the `sync` service **once per node per day**, not once per request.
- **Per-node replay protection (documented, accepted).** Replay nonces are kept per node, not mesh-wide: a signed request captured on the loopback of one node could in principle be replayed once on another node. This is accepted for v1 because the gateways are loopback only. Do not file it as a bug.

## Known behaviour to expect (not failures)

- **Owner.** Devices that registered without having to ask to join rank first (by registered time, then id); approved joiners rank by approval time. Approvals are stamped later than every earlier approval in the account, so approving a device does not change the owner. If the owner signs out, the mesh hands the role to the earliest-approved joiner, whereas the native server hands it to the earliest-registered device. This divergence is known.
- **Refusals that only the mesh service makes:** 413 for a key over 512 bytes, an order over 64 bytes, a record `hlc.node` over 48 bytes, or namespace plus uuid over about 442 bytes; 400 for a NUL character in the namespace, uuid or nonce. Real Aegis clients are well inside these limits.
- **Retention.** The service prunes old tombstones only in the namespace being pushed. Each pruned record leaves a permanent empty row of about 100 to 300 bytes that the mesh GC never removes.

## Results

| Date | Device | Network | Step or check | Result | Notes |
|---|---|---|---|---|---|
| | PC + S23 | | 1 build and deploy; runs on both nodes | | |
| | PC | | 2 Aegis desktop: suggestion, register (owner) | | |
| | S23 | | 3 Aegis phone: suggestion, register, approve | | seconds to approved: |
| | PC + S23 | | 4 bookmarks both ways | | |
| | PC + S23 | | 5 airplane mode, converge | | seconds from reconnect: |
| | PC + S23 | | 6 revoke the phone | | seconds to refusal: |
| | PC (release builds) | localhost | large first sync | | records / seconds: |
| | S23 | localhost | Android plain HTTP to 127.0.0.1:7801 | | |
| | PC | | open-registration warning, once per day | | |
| | GitHub | | new CI job, Meshnode `aegis-sync` (after push) | | |
| | GitHub | | new CI jobs, Aegis `sync-server` and `msrv` (after push) | | |

## Automated coverage (for reference)

Aegis: the 82 existing sync-server tests plus the core's unit tests and the contract vectors (run against the native server). One test, `save_snapshot`'s directory fsync, fails on Windows on `main` as well; it passes on Linux. Meshnode: the same vectors against the WASM service, two-node divergence, split first registration, replay, revocation, `kv/local` (not replicated, TTL, caps, purge, GC), and a 5 000-record paging test on a real store.
