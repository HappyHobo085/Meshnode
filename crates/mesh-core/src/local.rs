//! Node-local files in the data directory: `node.key`, `mesh.json`, `role.json`,
//! `invites.json`, `rebootstrap.bin` and `expired-edits.bin`.
//!
//! `node.key` is the raw 32-byte signing seed, readable by its owner only (spec §3.1): on Unix
//! it is created with mode 0600; on Windows it gets a protected DACL (nothing inherited from
//! the data directory) whose only ACE grants the current user full access. The permissions
//! are set on the temp file before the seed is written, and the file is moved into place
//! atomically.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use mesh_proto::{MeshGenesis, NodeAddrBytes, NodeId, Role, SignedOp};
use mesh_store::SyncItem;
use serde::{Deserialize, Serialize};

use crate::error::NodeError;

const KEY_FILE: &str = "node.key";
const GENESIS_FILE: &str = "mesh.json";
const ROLE_FILE: &str = "role.json";
const INVITES_FILE: &str = "invites.json";
const REBOOTSTRAP_FILE: &str = "rebootstrap.bin";
const EXPIRED_FILE: &str = "expired-edits.bin";

/// Renames `from` over `to` (or, without `replace`, only if `to` does not exist: then it
/// fails with `AlreadyExists`) so that the rename survives a power loss once this returns.
fn durable_rename(from: &Path, to: &Path, replace: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::winfs::durable_rename(from, to, replace)
    }
    #[cfg(not(windows))]
    {
        if replace {
            std::fs::rename(from, to)?;
        } else {
            // A hard link fails if `to` exists, unlike a rename.
            std::fs::hard_link(from, to)?;
            std::fs::remove_file(from)?;
        }
        // The rename is durable only once the directory entry is.
        #[cfg(unix)]
        {
            let dir = to.parent().filter(|p| !p.as_os_str().is_empty());
            std::fs::File::open(dir.unwrap_or(Path::new(".")))?.sync_all()?;
        }
        Ok(())
    }
}

/// The temp file next to `path` that `write_durably` writes first.
fn temp_path(path: &Path) -> PathBuf {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    dir.unwrap_or(Path::new(".")).join(format!(".{name}.tmp"))
}

/// Writes `bytes` to a fresh temp file next to `path` (opened with `opts`; on Windows with
/// `owner_only` its DACL is restricted to the current user before anything is written),
/// fsyncs it and renames it to `path` durably; the temp file is removed on failure.
fn write_durably(
    path: &Path,
    bytes: &[u8],
    opts: &std::fs::OpenOptions,
    replace: bool,
    owner_only: bool,
) -> std::io::Result<()> {
    let tmp = temp_path(path);
    // A leftover from a crash: start afresh, so `opts` (permissions) apply.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let res = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        #[cfg(windows)]
        if owner_only {
            crate::winfs::restrict_to_current_user(&tmp)?;
        }
        #[cfg(not(windows))]
        let _ = owner_only; // Unix: the mode in `opts` does it.
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        durable_rename(&tmp, path, replace)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Write `bytes` to `path` via a temp file in the same directory, fsync, then a durable
/// rename over `path` (the directory entry is synced too: on Unix by fsyncing the
/// directory, on Windows by `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), NodeError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    Ok(write_durably(path, bytes, &opts, true, false)?)
}

/// Load `node.key`, creating a fresh random key if the file does not exist.
pub fn load_or_create_key(dir: &Path) -> Result<SigningKey, NodeError> {
    let path = dir.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let seed: [u8; 32] = bytes.try_into().map_err(|_| NodeError::BadKey)?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            let seed: [u8; 32] = rand::random();
            match create_private(&path, &seed) {
                Ok(()) => Ok(SigningKey::from_bytes(&seed)),
                // Created meanwhile by someone else: use that one.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create_key(dir),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Creates `path` holding `bytes`, readable by the owner only, atomically (temp file and a
/// durable rename that never replaces): a crash leaves either no file or the whole one.
fn create_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    write_durably(path, bytes, &opts, false, true)
}

#[derive(Serialize, Deserialize)]
struct GenesisFile {
    founder: String,
    nonce: String,
}

fn hex32(s: &str) -> Result<[u8; 32], NodeError> {
    let v = hex::decode(s).map_err(|e| bad_file(e.to_string()))?;
    v.try_into()
        .map_err(|_| bad_file("expected 32 bytes".into()))
}

fn incompatible_file(file: &str, detail: String) -> NodeError {
    NodeError::Incompatible {
        file: file.into(),
        detail,
    }
}

fn bad_file(msg: String) -> NodeError {
    NodeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, msg))
}

/// `Ok(None)` when `mesh.json` does not exist.
pub fn read_genesis(dir: &Path) -> Result<Option<MeshGenesis>, NodeError> {
    let bytes = match std::fs::read(dir.join(GENESIS_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let incompatible = |detail: String| incompatible_file(GENESIS_FILE, detail);
    let f: GenesisFile = serde_json::from_slice(&bytes).map_err(|e| incompatible(e.to_string()))?;
    let hex = |s: &str| hex32(s).map_err(|_| incompatible("not a 32-byte hex id".into()));
    Ok(Some(MeshGenesis {
        founder: NodeId(hex(&f.founder)?),
        nonce: hex(&f.nonce)?,
    }))
}

pub fn write_genesis(dir: &Path, g: &MeshGenesis) -> Result<(), NodeError> {
    let f = GenesisFile {
        founder: hex::encode(g.founder.0),
        nonce: hex::encode(g.nonce),
    };
    let bytes = serde_json::to_vec_pretty(&f).map_err(|e| bad_file(e.to_string()))?;
    atomic_write(&dir.join(GENESIS_FILE), &bytes)
}

/// Deletes `mesh.json` (no error if it is already gone): a join whose node failed to start
/// leaves the directory reusable with the same ticket.
pub fn remove_genesis(dir: &Path) -> Result<(), NodeError> {
    match std::fs::remove_file(dir.join(GENESIS_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[derive(Serialize, Deserialize)]
struct RoleFile {
    /// `"host"` or `"client"`.
    joined_as: String,
}

/// Records the role this node entered the mesh with (`role.json`): the founder a host, a
/// joiner its ticket's role. Written before `mesh.json`, so a node always knows whether its
/// store is partial (sub-spec 8a §3), even before its own `members` record has arrived.
/// A role upgrade (client to host) rewrites it as `host` once its Full rebuild is done.
pub fn write_own_role(dir: &Path, role: Role) -> Result<(), NodeError> {
    let f = RoleFile {
        joined_as: match role {
            Role::Host => "host",
            Role::Client => "client",
        }
        .to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&f).map_err(|e| bad_file(e.to_string()))?;
    atomic_write(&dir.join(ROLE_FILE), &bytes)
}

/// `Ok(None)` when `role.json` does not exist.
pub fn read_own_role(dir: &Path) -> Result<Option<Role>, NodeError> {
    let bytes = match std::fs::read(dir.join(ROLE_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let f: RoleFile =
        serde_json::from_slice(&bytes).map_err(|e| incompatible_file(ROLE_FILE, e.to_string()))?;
    match f.joined_as.as_str() {
        "host" => Ok(Some(Role::Host)),
        "client" => Ok(Some(Role::Client)),
        other => Err(incompatible_file(
            ROLE_FILE,
            format!("unknown role `{other}`"),
        )),
    }
}

/// Deletes `role.json` (no error if it is already gone).
pub fn remove_own_role(dir: &Path) -> Result<(), NodeError> {
    match std::fs::remove_file(dir.join(ROLE_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InviteEntry {
    pub role: Role,
    pub expires_ms: u64,
    pub redeemed_by: Option<NodeId>,
}

#[derive(Serialize, Deserialize)]
struct InviteRow {
    host: bool,
    expires_ms: u64,
    redeemed_by: Option<String>,
}

/// Issued invites keyed by `blake3(token)`, persisted as `invites.json` after every change.
pub struct Invites {
    path: PathBuf,
    map: BTreeMap<[u8; 32], InviteEntry>,
}

impl Invites {
    pub fn load(dir: &Path) -> Result<Invites, NodeError> {
        let path = dir.join(INVITES_FILE);
        let mut map = BTreeMap::new();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let bad = |detail: String| incompatible_file(INVITES_FILE, detail);
                let hex = |s: &str| hex32(s).map_err(|_| bad("not a 32-byte hex id".into()));
                let rows: BTreeMap<String, InviteRow> =
                    serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
                for (k, r) in rows {
                    let redeemed_by = r.redeemed_by.as_deref().map(hex).transpose()?.map(NodeId);
                    let role = if r.host { Role::Host } else { Role::Client };
                    map.insert(
                        hex(&k)?,
                        InviteEntry {
                            role,
                            expires_ms: r.expires_ms,
                            redeemed_by,
                        },
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Invites { path, map })
    }

    fn save(&self) -> Result<(), NodeError> {
        let rows: BTreeMap<String, InviteRow> = self
            .map
            .iter()
            .map(|(k, e)| {
                (
                    hex::encode(k),
                    InviteRow {
                        host: e.role == Role::Host,
                        expires_ms: e.expires_ms,
                        redeemed_by: e.redeemed_by.map(|n| hex::encode(n.0)),
                    },
                )
            })
            .collect();
        let bytes = serde_json::to_vec_pretty(&rows).map_err(|e| bad_file(e.to_string()))?;
        atomic_write(&self.path, &bytes)
    }

    pub fn insert(&mut self, hash: [u8; 32], entry: InviteEntry) -> Result<(), NodeError> {
        self.map.insert(hash, entry);
        self.save()
    }

    pub fn get(&self, hash: &[u8; 32]) -> Option<&InviteEntry> {
        self.map.get(hash)
    }

    /// Mark the invite redeemed; a no-op if the hash is unknown.
    pub fn set_redeemed(&mut self, hash: [u8; 32], by: NodeId) -> Result<(), NodeError> {
        if let Some(e) = self.map.get_mut(&hash) {
            e.redeemed_by = Some(by);
            self.save()?;
        }
        Ok(())
    }

    /// Drop expired entries that were never redeemed.
    pub fn prune(&mut self, now_ms: u64) -> Result<(), NodeError> {
        let before = self.map.len();
        self.map
            .retain(|_, e| e.redeemed_by.is_some() || e.expires_ms > now_ms);
        if self.map.len() != before {
            self.save()?;
        }
        Ok(())
    }
}

/// The in-progress re-bootstrap export (spec §7 step 2), persisted before the store is reset.
///
/// Beyond spec §7's `{ s, items, expired }` it carries `last_seq` (to check that no own op
/// slipped in between the export and the reset, and to cover a trailing hole) and `peers`
/// (so a node resumed after a crash can reach a peer although its emptied store holds no
/// addresses). Not a stable format: a file from an older build fails to decode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebootstrapFile {
    /// `synced_self_seq` read before the reset.
    pub s: u64,
    /// The last own seq used before the reset (`next_seq - 1`).
    pub last_seq: u64,
    /// `Export::items`, re-applied once the catch-up reached `s`.
    pub items: Vec<SyncItem>,
    /// `Export::expired`, own edits too old to re-apply.
    pub expired: Vec<SignedOp>,
    /// Effective members (but this node) with their last known address.
    pub peers: Vec<(NodeId, NodeAddrBytes)>,
    /// A role upgrade (sub-spec 8a §3): the store was Partial and is rebuilt as Full. The
    /// rebuild announces `Full`, and on completion the scope flips to Full and `role.json`
    /// says `host`.
    pub full: bool,
}

/// Writes `rebootstrap.bin` atomically (temp file, fsync, rename).
pub fn write_rebootstrap(dir: &Path, f: &RebootstrapFile) -> Result<(), NodeError> {
    let bytes = postcard::to_stdvec(f).map_err(|e| bad_file(e.to_string()))?;
    atomic_write(&dir.join(REBOOTSTRAP_FILE), &bytes)
}

/// `Ok(None)` when no re-bootstrap is in progress. A file that does not decode is an error:
/// the export is never recomputed from a half-reset store.
pub fn read_rebootstrap(dir: &Path) -> Result<Option<RebootstrapFile>, NodeError> {
    match std::fs::read(dir.join(REBOOTSTRAP_FILE)) {
        Ok(b) => postcard::from_bytes(&b)
            .map(Some)
            .map_err(|e| bad_file(format!("{REBOOTSTRAP_FILE}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether `rebootstrap.bin` exists (a re-bootstrap must resume).
pub fn rebootstrap_pending(dir: &Path) -> Result<bool, NodeError> {
    Ok(dir.join(REBOOTSTRAP_FILE).try_exists()?)
}

/// Deletes `rebootstrap.bin` (no error if it is already gone).
pub fn remove_rebootstrap(dir: &Path) -> Result<(), NodeError> {
    match std::fs::remove_file(dir.join(REBOOTSTRAP_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Own edits left out of a re-bootstrap (spec §7 step 6), oldest run first; empty if none.
pub fn read_expired_edits(dir: &Path) -> Result<Vec<SignedOp>, NodeError> {
    match std::fs::read(dir.join(EXPIRED_FILE)) {
        Ok(b) => postcard::from_bytes(&b).map_err(|e| bad_file(format!("{EXPIRED_FILE}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Adds `ops` to `expired-edits.bin` (atomically), skipping ops already listed, so a
/// re-bootstrap repeated after a crash lists each edit once.
pub fn append_expired_edits(dir: &Path, ops: &[SignedOp]) -> Result<(), NodeError> {
    let mut all = read_expired_edits(dir)?;
    for op in ops {
        let id = (op.body.origin, op.body.seq);
        if !all.iter().any(|o| (o.body.origin, o.body.seq) == id) {
            all.push(op.clone());
        }
    }
    let bytes = postcard::to_stdvec(&all).map_err(|e| bad_file(e.to_string()))?;
    atomic_write(&dir.join(EXPIRED_FILE), &bytes)
}
