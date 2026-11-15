# Manual check: sub-spec 8a (`mesh-client` SDK and partial replication)

Run by the owner on Windows 11 and the Galaxy S23 (spec `2026-11-12-mesh-client-design.md` §7). Until this is done, master spec §14 row 8a stays "implemented, manual check pending". Step 4 is success criterion 5 of the master spec on real devices (a client keeps working with the desktop node off, served by the phone). The in-process version of it is the automated test `criterion5_client_via_remaining_host`.

The build agent ran none of this. It never ran a client against a real mesh, never touched a phone, never loaded the AAR in an Android app, and never pushed. The two new CI jobs (`mesh-client` and `mesh-client-ffi-android`) have **not run anywhere yet**; they run only once the branch is pushed. Check them the first time you push, and treat a red job as a finding.

Start from the mesh of `docs/manual-checks/sub-spec-6.md`: the PC and the S23 paired as **hosts**, both nodes running. This check deploys a static site and adds, then removes, a client member. The client's node id is removed for good at the end, so do not use a mesh you cannot afford to disturb.

## Build

On the PC, from the repository on branch `feat/mesh-client`:

```
set CARGO_TARGET_DIR=%LOCALAPPDATA%\meshnode-target
cargo build --release -p mesh-client --example fetch
```

The program is `%LOCALAPPDATA%\meshnode-target\release\examples\fetch.exe` (`D:` is FAT32, so keep the target dir on `C:`). It needs no installation. `meshd` and the Android app are built as in `docs/manual-checks/sub-spec-6.md`; this branch changes the sync protocol (`Hello` gained `scope` and `freshness`), so **both nodes must run builds from this branch**. A node on an older build cannot sync with them.

`fetch` has two forms:

```
fetch join <dir> <ticket> <service> <path>   # join with a client invite, then fetch
fetch open <dir> <service> <path>            # reuse the client joined in <dir>
```

It waits up to 30 s for the service to appear in the client's replica and for a host to be online, sends one `GET`, prints the status line, headers and body, and exits non-zero on an error.

**Where the client runs.** Preferably a second machine (copy `fetch.exe` there; it has no other files) or a second Windows user profile. The PC itself also works: the client is its own node and opens no ports of its own, so it does not conflict with the PC's `meshd`. Use a directory nobody else uses, for example `%USERPROFILE%\mesh-client-check`. Never point it at a `meshd` data directory: `open` refuses one (`NotJoined`), and `join` refuses one that is in a mesh (`AlreadyJoined`).

## Steps (spec §7)

Record the date, the machine, the network and the result of each in the table.

1. **Deploy a site placed on both hosts.** Make a directory with an `index.html` containing a recognisable line, then on the PC run `meshd deploy static site <dir>` (no `--nodes` or `--tags`: the default placement is every host). Pass: `meshd services` lists `site`, and `http://site.mesh.localhost:7777` shows the page in the PC browser and in the phone browser.
2. **Join a client and fetch the site.** On the PC: `meshd invite --client`. Copy the ticket (it is valid for 15 minutes; `--ttl 1h` for longer). On the client machine:
   ```
   fetch.exe join %USERPROFILE%\mesh-client-check <ticket> site /
   ```
   Pass: it prints `HTTP/1.1 200 OK` (the status line), the headers and the page. `meshd members` on the PC lists a new member with role `client`, named `fetch`. Record the seconds from start to output.
3. **Open again.** `fetch.exe open %USERPROFILE%\mesh-client-check site /` Pass: the same page, the same member id (no new member in `meshd members`). Also try a path that does not exist (`site /nope.html`): the site's own `404` is printed as a response, not as an error.
4. **Criterion 5: desktop node off, served by the phone.** Stop the PC's node: `meshd service stop` (needs Administrator), or quit the desktop app's Run without installing. If the client runs on the PC, that is fine: the client is not part of `meshd`. Keep the phone's Meshnode running and online (Wi-Fi or mobile data). Then run `fetch.exe open %USERPROFILE%\mesh-client-check site /`. Pass: the page is printed, served by the phone. Record the seconds. Start the PC's node again afterwards.
   - If it prints `no host could serve site`, first check that the phone's app shows the site under Services and the node as running. Then retry once (a host the client tried first may have been listed online while it was going down). A second failure is a finding; note the phone's network.
5. **The client's store holds only `_mesh`.** There is no command that lists a client's namespaces, so this step compares sizes; the exact namespace check is automated (see the end).
   - The client directory holds `node.key`, `mesh.json`, `role.json`, `client.lock` and `store.redb`. Pass: `role.json` says `"joined_as": "client"`.
   - Put some service data in the mesh: the `sync` service from `docs/manual-checks/sub-spec-7.md` with an Aegis account works (its `meshd services` `DATA` column shows its size). Or deploy any WASM service and write to it.
   - Run `fetch.exe open %USERPROFILE%\mesh-client-check sync /healthz` (any request will do: it brings the client up to date), then compare the size of the client's `store.redb` with the host's `store.redb` (the installed service's data directory is the `dataDir` that `meshd service status --json` prints; for the app's Run without installing, its data directory). Pass: the client's file is far smaller than the host's and does not grow when the services' `DATA` grows (repeat after adding a few thousand bookmarks or records, as in sub-spec 7's large first sync). redb files grow in pages and never shrink, so compare growth, not exact numbers.
6. **Remove the client; it is refused.** On the PC: `meshd remove <client id prefix>` (from `meshd members`), or in the desktop app Home, the client's card, **Remove device…**. Then run `fetch.exe open %USERPROFILE%\mesh-client-check site /`. Pass: no page is printed. The program says `cannot start the client: this client has been removed from the mesh` or `request failed: this client has been removed from the mesh` (it may first wait up to 30 s), and `meshd members` no longer shows the client as an effective member (it is gone from the list, or its STATE says `(not effective)`). The removal is permanent for that identity. To clean up, delete `%USERPROFILE%\mesh-client-check`.

## Extra checks

- **Second client at once.** While a `fetch.exe open` on the directory is waiting (run it with a service name that does not exist, so it waits 30 s), run a second `fetch.exe open` on the same directory. Pass: the second one fails at once with `cannot start the client: another client has this directory open`.
- **Wrong ticket.** `fetch.exe join <new empty dir> <a host ticket from "meshd invite"> site /` Pass: it fails with `not a valid client invite ticket`, and the directory is left without `mesh.json`.
- **Kotlin AAR in an app (optional; 8a ships no sample app).** If you have an Android app project at hand: build `dist/mesh-client.aar` with `bash scripts/build-client-aar.sh` (needs `cargo-ndk`, `ANDROID_NDK_HOME` set to NDK 27.2, JDK 17 as `JAVA_HOME`). Add it and its runtime dependencies as `crates/mesh-client/README.md` (Kotlin, Android) shows, call `MeshClientAndroid.init(applicationContext)` in `Application.onCreate`, then join with a client ticket and request `site /`. Pass: the page's status and body arrive; a **release** (minified) build works too, which checks the AAR's `proguard.txt`. Nothing of the AAR has ever run on a device, so any failure here is a finding: record the exception and the logcat lines around it.
- **Declare up to date (UI and CLI wording only).** On a healthy host, `meshd declare-fresh` without `--yes` prints the risk and changes nothing. In the desktop app, Settings shows **Declare up to date…**; open it and cancel. Do not confirm it on a host that is not actually alone and stale: it makes the node a fresh copy of the mesh's data again.

## Known behaviour to expect (not failures)

- **First sync in the background.** `join` returns once the node is a member. `fetch` then waits for the manifest and an online host (up to 30 s), so the first `join` takes a few seconds longer than later `open`s.
- **Timeouts.** A request that reaches a host that does not answer fails with `request failed: the request timed out` (after the service's request timeout plus 5 s per host, or 30 s for the whole call). Connecting to one host may take at most 5 s before the next is tried; on a poor mobile link, a slow first connect through the relay can therefore be abandoned (deferred finding F3; record it if you see `no host could serve` while the phone is clearly reachable).
- **A client re-invited as a host.** No product path does this in 8a: `fetch join` refuses a joined directory, and there is no promote command.
- **Host restarted on an old build.** A node on a build from before this branch cannot sync with these nodes (the sync `Hello` changed). Update both.

## Results

| Date | Machine | Network | Step or check | Result | Notes |
|---|---|---|---|---|---|
| | PC + S23 | | 1 site on both hosts | | |
| | client machine | | 2 join and fetch | | seconds to output: |
| | client machine | | 3 open again, 404 passed through | | |
| | client machine + S23 | | 4 PC node off, fetch via the phone | | seconds: ; phone network: |
| | client machine | | 5 client store holds only `_mesh` | | client / host `store.redb` sizes, before and after: |
| | PC + client machine | | 6 remove, then refused | | message printed: |
| | client machine | | second client on the same directory | | |
| | client machine | | host ticket refused | | |
| | Android app (optional) | | AAR: init, join, request, release build | | |
| | PC | | declare-fresh wording (not confirmed) | | |
| | GitHub | | new CI job `mesh-client` (after push) | | |
| | GitHub | | new CI job `mesh-client-ffi-android` (after push) | | |

## Automated coverage (for reference)

- **mesh-store:** scoped diffs (out-of-scope runs become `Covered`, compacted gaps, `limit`, the scan budget, paging to completion), and the refusal of out-of-scope writes.
- **mesh-core (`tests/partial.rs`, `tests/stale_partial.rs`):**
  - the client store holds only `_mesh` (namespace sizes checked), and the join catch-up is scoped;
  - a new host that synced first from the client still gets all data later from a Full host;
  - a contradictory scope is refused, and `Advertise` before `Hello` is a protocol error;
  - every staleness rule (C1c, C1d, C1e), the re-bootstrap epoch guards, and the role upgrade (online, while offline, resumed after a crash, gated on a Fresh Full peer).
- **mesh-client (`tests/client.rs`):**
  - join and request against a static site and a WASM fixture;
  - failover, `NoHost`, a timeout, and criterion 5 in-process;
  - removal, `open` and a second `open`, a host ticket and a used directory refused;
  - invite from a client, the services list following deploys, the local `413`;
  - join retry after a lost reply, and drop without shutdown, also off the runtime;
  - the no-runtime-dependencies check.
- **mesh-client-ffi:** the Rust tests of the wrapper (`tests/ffi.rs`: request round trip, listener, shutdown during a request, drop, error mapping), and the JVM test (`jvm-test/run.sh`: the library loads through JNA, and errors map to the right `MeshClientException`).
