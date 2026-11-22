# End-to-end results (master spec §1)

Filled in by the owner while running `docs/e2e-checklist.md`. One copy of the tables per run; add newer runs above older ones. Only the owner marks a step passed.

## Run: 2026-11-22 (partial, build agent, desktop + Windows VM; no phone)

Run by the build agent at the owner's request, on this desktop and the Windows 11 test VM `meshwin` (libvirt) standing in for the laptop. The phone was not connected, and the owner kept the reboot and power-off steps, so every phone step, C1 and C5 are **not run**. "Pass" below means the agent saw the pass condition; the owner still has to confirm. The owner's own node was stopped for the run and restarted afterwards.

### Build

| Item | Value |
|---|---|
| Commit | `be42c25` (main) |
| Release version (`scripts/check-version.sh`) | 0.1.0 |
| Older build used for the upgrade check | 0.0.1 from the same commit (`scripts/set-version.sh 0.0.1`; rpm in a scratch worktree, MSI in the VM's copy) |
| APK certificate SHA-256 digest, 0.0.1 build | not built (no phone; no release keystore) |
| APK certificate SHA-256 digest, release build | not built |
| MSI files | `Meshnode_0.0.1_x64_en-US.msi`, `Meshnode_0.1.0_x64_en-US.msi` (built in the VM) |

### Devices

| Device | Model | OS and version | Network(s) used |
|---|---|---|---|
| Desktop (rpm) | this PC | Nobara Linux 44 KDE, kernel 7.3.4, glibc 2.43 | home LAN |
| Phone (APK) | Galaxy S23 | not used | |
| Laptop (MSI, client) | VM `meshwin` | Windows 11 Pro 10.0.26100 | passt user-mode NAT through the desktop |

### A. Install and upgrade in place

| Step | Pass/fail | Notes |
|---|---|---|
| A1 Desktop rpm install 0.0.1 | pass | enabled + active, `meshd 0.0.1`, system user, `service install` refused with the package message; `meshd init --name desktop` from the CLI (app not opened); `site` 200 |
| A2 Laptop MSI install 0.0.1, admin without elevation | partial | silent install (`msiexec /qn`) over SSH as admin user `mesh`; service `installed, running`; SID matches `whoami /user`; `service install` refused; joined as client, listed online. **Not checked:** the app opening and connecting without elevation (GUI) |
| A3 Phone APK install 0.0.1, joined | not run | no phone |
| A4 Upgrade to release: desktop | pass | `meshd 0.1.0`, enabled + active, `NRestarts=0`, same members, `site` kept |
| A4 Upgrade to release: phone | not run | no phone |
| A4 Upgrade to release: laptop | pass | `meshd 0.1.0`, service running, SID unchanged, one Meshnode product (0.1.0), same members |
| A4 MSI downgrade refused | pass | exit 1603, log: "A newer version of Meshnode is already installed."; 0.1.0 still installed and running |
| A5 Phone fresh start | not run | no phone |

### Criteria

| Criterion | Pass/fail | Measurement | Notes |
|---|---|---|---|
| C1 Desktop starts at boot without login | not run | | owner's step (reboot) |
| C1 Phone back after reboot (after unlock) | not run | seconds: | no phone |
| C1 Phone back after swipe-away | not run | seconds: | no phone |
| C2 Pairing (phone on mobile data) | not run | seconds: | no phone |
| C3 Hosting on both (site, site2, sync; both gateways) | partial | | `sync` built from Aegis `sync-server/wasm` at Aegis `61bca1e`, listed `64 MB / 30000 ms / 8388608 B`. Desktop: `site` and `127.0.0.1:7801/healthz` 200. Laptop (client, forwarding to the desktop): `site.mesh.localhost:7777` 200 and `127.0.0.1:7801/healthz` 200. Not run: `site2` from the phone and the phone's gateway |
| C4 Real use: Aegis sync and airplane-mode convergence | not run | convergence seconds: | needs Aegis on the phone |
| C5 Desktop off: phone serves locally and to the laptop | not run | | needs the phone; owner's step (power-off) |
| C6 Removal refused within 60 s | adapted pass | seconds: about 1 | removed the **laptop** instead of the phone: its `meshd status` showed `removed` on the first poll (1 s); the desktop lists it `offline (not effective)` |
| C7 Battery, 24 h screen off on Wi-Fi | not run | see below | no phone |

### Battery (C7)

| Start time | Start % | End time | End % | Hours | Drain per 24 h | Desktop write applied after (s) | App mAh (optional) |
|---|---|---|---|---|---|---|---|
| not run | | | | | | | |

### Z. Uninstall and purge

| Step | Pass/fail | Notes |
|---|---|---|
| Z1 Desktop rpm remove, reinstall (same node), purge | pass | after `rpm -e` the unit is gone and data + user stay; reinstall: enabled + active, same `Node:` id; purge removed package, data and user (`userdel` kept the group as documented; `groupdel` removed it) |
| Z2 Phone uninstall | not run | no phone |
| Z3 Laptop MSI uninstall, data kept, purge | pass | `msiexec /x`: "Removal completed successfully"; service gone; files gone; no product left; `%ProgramData%\meshnode` purged. Seeing the folder **before** the purge was lost to a script quoting error, so "data kept" is inferred only from the purge succeeding |

### Notes

1. Windows: .NET's `Invoke-WebRequest` cannot resolve `*.mesh.localhost` (Windows' resolver has no `*.localhost` rule); `curl.exe --resolve` and browsers work. Not a Meshnode bug, but a CLI user on Windows will hit it.
2. Aegis's working tree had uncommitted changes in `sync-server/core` (a new `DeviceFacts.joined` field) that break the WASM service's build. The run built from committed `61bca1e` through `git archive`.
3. Remaining for the owner: the phone half (A3, A4 phone, A5, C1–C2, C4–C5, C7, C6 on the phone, Z2, with a release keystore), the desktop reboot (C1), the app GUI checks (A1 app, A2 no elevation), and `docs/manual-checks/sub-spec-8b.md`.

## Run: <date>

### Build

| Item | Value |
|---|---|
| Commit | |
| Release version (`scripts/check-version.sh`) | |
| Older build used for the upgrade check | |
| APK certificate SHA-256 digest, 0.0.1 build | |
| APK certificate SHA-256 digest, release build | |
| MSI files | |

### Devices

| Device | Model | OS and version | Network(s) used |
|---|---|---|---|
| Desktop (rpm) | | | |
| Phone (APK) | Galaxy S23 | | |
| Laptop (MSI, client) | | | |

### A. Install and upgrade in place

| Step | Pass/fail | Notes |
|---|---|---|
| A1 Desktop rpm install 0.0.1 | | |
| A2 Laptop MSI install 0.0.1, admin without elevation | | |
| A3 Phone APK install 0.0.1, joined | | |
| A4 Upgrade to release: desktop | | |
| A4 Upgrade to release: phone | | |
| A4 Upgrade to release: laptop | | |
| A4 MSI downgrade refused | | |
| A5 Phone fresh start | | |

### Criteria

| Criterion | Pass/fail | Measurement | Notes |
|---|---|---|---|
| C1 Desktop starts at boot without login | | | |
| C1 Phone back after reboot (after unlock) | | seconds: | |
| C1 Phone back after swipe-away | | seconds: | |
| C2 Pairing (phone on mobile data) | | seconds: | |
| C3 Hosting on both (site, site2, sync; both gateways) | | | |
| C4 Real use: Aegis sync and airplane-mode convergence | | convergence seconds: | |
| C5 Desktop off: phone serves locally and to the laptop | | | |
| C6 Removal refused within 60 s | | seconds: | |
| C7 Battery, 24 h screen off on Wi-Fi | | see below | |

### Battery (C7)

| Start time | Start % | End time | End % | Hours | Drain per 24 h | Desktop write applied after (s) | App mAh (optional) |
|---|---|---|---|---|---|---|---|
| | | | | | | | |

Target: at most 5 % per 24 h with the default power policy (Balanced).

### Z. Uninstall and purge

| Step | Pass/fail | Notes |
|---|---|---|
| Z1 Desktop rpm remove, reinstall (same node), purge | | |
| Z2 Phone uninstall | | |
| Z3 Laptop MSI uninstall, data kept, purge | | |

### Notes

Findings, failures and anything that needed a retry.
