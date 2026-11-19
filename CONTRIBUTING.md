# Contributing

## Release rule (versions and formats)

From sub-spec 8b §5.1; master spec §13 carries the same rule. Meshnode has no migrations and no
mixed-version compatibility: a node refuses data or a peer from an incompatible version, in plain
words, and never crash-loops on it.

1. **One product version.** `apps/meshd/Cargo.toml` is the source. Change it only with
   `scripts/set-version.sh <x.y.z>`, which also writes `crates/mesh-ffi/Cargo.toml` and
   `apps/{desktop,android}/src-tauri/{Cargo.toml,tauri.conf.json}`. `scripts/check-version.sh`
   fails when the six disagree. Android's versionCode is derived by Tauri as
   `major*1000000 + minor*1000 + patch` (limits in the `set-version.sh` header), so every release
   needs a higher version than the last one installed.
2. **On-disk formats.** Any change to how the store is written bumps
   `mesh_store::SCHEMA_VERSION` (`crates/mesh-store/src/tables.rs`). A store with a different
   schema, older or newer, is refused (`meshd run` exits 4; the reason goes to
   `<data>/start-error.txt`).
3. **Other state files** (`meshd.toml`, `role.json`, `mesh.json`, `invites.json`) carry no version
   field. Unreadable or unknown content is refused the same way, naming the file. `meshd.toml`
   refuses unknown keys, so a new key there is detected by older builds. The JSON files ignore
   unknown fields: a breaking change to one of them must change or remove an existing field
   (so older builds fail to parse it), or first add a version field that this release rule
   then governs.
4. **Wire protocols.** Any breaking change to a protocol bumps the `/N` of its ALPN:
   `meshnode/sync/1`, `meshnode/join/1`, `meshnode/httpfwd/1`. The signing domains
   `meshnode/op/1` and `meshnode/mesh/1` follow the same rule. The local admin protocol (CLI and
   desktop app to `meshd`) has no ALPN: any change to its wire types
   (`crates/mesh-admin/src/proto.rs`) bumps `mesh_admin::PROTOCOL_VERSION`, so a mismatched
   client gets "cli X, daemon Y" from the `Hello` exchange instead of a decode error. Admin
   frames are postcard, which is positional: adding a field, even with `#[serde(default)]`,
   changes the encoding and needs the bump. `Hello` and its `DaemonState` keep their encoding
   forever, since the version check runs on them.
5. **`meshnode/version` is frozen.** The version probe's ALPN has no `/N` and is never bumped.
   Its payload is JSON (`mesh_proto::PeerVersion`: product version, each protocol's `/N`, the
   store schema); later versions add fields and older readers ignore them. A peer whose `sync`
   version or store schema differs shows as "incompatible version (x)" in `meshd members` and
   the UI.
6. **No migrations for now.** An older or newer format is refused, never converted. Downgrades
   are not supported on any platform.
7. **Before a release**, run `docs/e2e-checklist.md` on the real devices and record it in
   `docs/e2e-results.md`.
