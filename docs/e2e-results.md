# End-to-end results (master spec §1)

Filled in by the owner while running `docs/e2e-checklist.md`. One copy of the tables per run; add newer runs above older ones. Only the owner marks a step passed.

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
