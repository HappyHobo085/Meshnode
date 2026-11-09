//! Node-local files in the data directory: `node.key`, `mesh.json`, `invites.json`,
//! `rebootstrap.bin` and `expired-edits.bin`.
//!
//! `node.key` is the raw 32-byte signing seed. On Unix it is created with mode 0600. On Windows
//! no extra ACL code is run: the file inherits the ACL of the data directory, which normally lives
//! under the user profile (or `%ProgramData%` for services) and is therefore not world-readable.

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
const INVITES_FILE: &str = "invites.json";
const REBOOTSTRAP_FILE: &str = "rebootstrap.bin";
const EXPIRED_FILE: &str = "expired-edits.bin";

/// Write `bytes` to `path` via a temp file in the same directory, fsync, then rename over.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), NodeError> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp: PathBuf = dir.join(format!(".{name}.tmp"));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(res?)
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
            create_private(&path, &seed)?;
            Ok(SigningKey::from_bytes(&seed))
        }
        Err(e) => Err(e.into()),
    }
}

fn create_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
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
    let f: GenesisFile = serde_json::from_slice(&bytes).map_err(|e| bad_file(e.to_string()))?;
    Ok(Some(MeshGenesis {
        founder: NodeId(hex32(&f.founder)?),
        nonce: hex32(&f.nonce)?,
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
                let rows: BTreeMap<String, InviteRow> =
                    serde_json::from_slice(&bytes).map_err(|e| bad_file(e.to_string()))?;
                for (k, r) in rows {
                    let redeemed_by = r.redeemed_by.as_deref().map(hex32).transpose()?.map(NodeId);
                    let role = if r.host { Role::Host } else { Role::Client };
                    map.insert(
                        hex32(&k)?,
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
