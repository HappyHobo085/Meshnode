# Manual check: sub-spec 4 (gateway, admin API, meshd, OS services)

Run by the owner (sub-spec 4 §8.2). Until this is done, master spec §14 row 4 stays "implemented, manual check pending".

Platforms: Windows now. Linux (systemd) runs later in a VM: pending. macOS: untested (the launchd plist and install code are only built in CI).

## Pending owner action

Run `cargo test -p meshd --test service -- --ignored` elevated (Administrator / root) on a disposable VM or machine. These tests install and remove a real OS service and write to system locations, so do not run them on a machine that matters. Result: not run yet.

## Steps

1. Windows, elevated: `meshd service install`; `meshd init --name pc`; `meshd status` shows Running.
2. Second node (another Windows PC now, or a Linux VM later) with the service installed: `meshd join <ticket from "meshd invite">`.
3. `meshd deploy static site <dir>` on one node; open `http://site.mesh.localhost:7777` in a browser on the other.
4. Deploy the `counter` fixture as `counter`; call it through both gateways; `meshd config set gateway.ports 7801=counter`, restart, and `http://127.0.0.1:7801` serves it.
5. Reboot one node: the service returns without anyone logging in, and the mesh reconnects (`meshd members`).
6. `meshd remove <other>`; `meshd service uninstall` on both.

## Results

| Date | OS | Machines | Pass/fail (per step 1-6) | Notes |
|---|---|---|---|---|
| | Windows | | | |
| | Linux (systemd, VM) | | pending | |
| | macOS | | untested | |

## Automated coverage (for reference)

`apps/meshd/tests/e2e.rs` (`two_daemons_serve_and_remove`) runs two real `meshd run` processes on temporary data directories and loopback ports: init, invite, join, deploy a static site, fetch it through the other node's gateway, remove the member, stop both. It never touches OS services. It does not replace the steps above.
