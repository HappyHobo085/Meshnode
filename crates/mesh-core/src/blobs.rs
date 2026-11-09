//! Minimal content-addressed blobs (spec §8): an iroh-blobs `FsStore` at `<data>/blobs/`,
//! served behind the membership gate, fetched from online members in RTT order and
//! protected from GC by a named pin tag. iroh-blobs' own GC is never switched on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use iroh::Endpoint;
use iroh_blobs::api::Store;
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::hashseq::HashSeq;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::fs::options::Options;
use iroh_blobs::{BlobFormat, Hash, HashAndFormat};
use mesh_proto::NodeId;
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::error::NodeError;
use crate::membership::MembershipSnapshot;
use crate::peers::PeerManager;
use crate::transport::endpoint_id_of;

/// The hash type of a blob or collection.
pub type BlobHash = Hash;

const DIR: &str = "blobs";
/// A collection root (32 bytes per entry) larger than this is not loaded.
const MAX_COLLECTION_ROOT: u64 = 16 << 20;

fn pin_tag(hash: &Hash) -> String {
    format!("meshnode/pin/{hash}")
}

fn err(e: impl std::fmt::Display) -> NodeError {
    NodeError::Iroh(e.to_string())
}

/// Where `fetch` learns its peers from.
#[derive(Clone)]
pub(crate) struct PeerSource {
    pub membership: Arc<watch::Sender<Arc<MembershipSnapshot>>>,
    pub peers: PeerManager,
    pub me: NodeId,
}

impl PeerSource {
    /// Online effective members other than this node, lowest RTT first (unknown last).
    fn ranked(&self) -> Vec<NodeId> {
        let snap = self.membership.borrow().clone();
        let mut v: Vec<(u64, NodeId)> = snap
            .members()
            .filter(|(id, _)| **id != self.me && snap.is_effective(id))
            .filter_map(|(id, _)| {
                self.peers
                    .link(id)
                    .map(|rtt| (rtt.unwrap_or(u64::MAX), *id))
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, id)| id).collect()
    }
}

/// Handle to the node's blob store. Cheap to clone.
#[derive(Clone)]
pub struct Blobs {
    store: FsStore,
    downloader: Downloader,
    source: PeerSource,
}

impl Blobs {
    pub(crate) async fn open(
        data_dir: &Path,
        endpoint: &Endpoint,
        source: PeerSource,
    ) -> Result<Blobs, NodeError> {
        let root = data_dir.join(DIR);
        let mut opts = Options::new(&root);
        opts.gc = None; // spec §8: iroh-blobs GC stays off
        let store = FsStore::load_with_opts(root.join("blobs.db"), opts)
            .await
            .map_err(err)?;
        let downloader = Downloader::new(&store, endpoint);
        Ok(Blobs {
            store,
            downloader,
            source,
        })
    }

    /// The protocol handler to mount (behind the gate) on `iroh_blobs::ALPN`.
    pub(crate) fn protocol(&self) -> iroh_blobs::BlobsProtocol {
        iroh_blobs::BlobsProtocol::new(&self.store, None)
    }

    /// Flushes and closes the store so the directory can be reopened.
    pub(crate) async fn shutdown(&self) {
        if let Err(e) = self.store.shutdown().await {
            debug!(error = %e, "blob store shutdown");
        }
    }

    fn api(&self) -> &Store {
        &self.store
    }

    async fn pin(&self, hash: Hash, format: BlobFormat) -> Result<(), NodeError> {
        self.api()
            .tags()
            .set(pin_tag(&hash), HashAndFormat { hash, format })
            .await
            .map_err(err)
    }

    /// Whether the pin tag `meshnode/pin/<hash>` exists.
    pub async fn is_pinned(&self, hash: Hash) -> Result<bool, NodeError> {
        let t = self.api().tags().get(pin_tag(&hash)).await.map_err(err)?;
        Ok(t.is_some_and(|t| t.hash == hash))
    }

    pub async fn add_bytes(&self, bytes: Bytes) -> Result<Hash, NodeError> {
        let tt = self.api().add_bytes(bytes).temp_tag().await.map_err(err)?;
        let h = tt.hash();
        self.pin(h, BlobFormat::Raw).await?;
        Ok(h)
    }

    /// A file becomes a blob; a directory becomes a collection of its files (relative
    /// paths with `/` separators), and the collection's hash is returned.
    pub async fn add_path(&self, path: &Path) -> Result<Hash, NodeError> {
        let meta = tokio::fs::metadata(path).await?;
        if meta.is_file() {
            let tt = self.api().add_path(path).temp_tag().await.map_err(err)?;
            let h = tt.hash();
            self.pin(h, BlobFormat::Raw).await?;
            return Ok(h);
        }
        let root = path.to_owned();
        let files = tokio::task::spawn_blocking(move || list_files(&root))
            .await
            .map_err(crate::node::join_err)??;
        let mut keep = Vec::with_capacity(files.len());
        let mut coll = Vec::with_capacity(files.len());
        for (rel, abs) in files {
            let tt = self.api().add_path(&abs).temp_tag().await.map_err(err)?;
            coll.push((rel, tt.hash()));
            keep.push(tt);
        }
        let tt = Collection::from_iter(coll)
            .store(self.api())
            .await
            .map_err(err)?;
        let h = tt.hash();
        self.pin(h, BlobFormat::HashSeq).await?;
        drop(keep);
        Ok(h)
    }

    /// Whether the blob is stored completely. For a collection use [`Blobs::has_collection`].
    pub async fn has(&self, hash: Hash) -> Result<bool, NodeError> {
        self.root_complete(hash).await
    }

    /// Whether the collection rooted at `hash` is stored: the root and every child.
    pub async fn has_collection(&self, hash: Hash) -> Result<bool, NodeError> {
        let Some(c) = self.load_collection(hash).await? else {
            return Ok(false);
        };
        for (_, h) in c.iter() {
            if !self.root_complete(*h).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn root_complete(&self, hash: Hash) -> Result<bool, NodeError> {
        self.api().blobs().has(hash).await.map_err(err)
    }

    /// The format `hash` is pinned with, if pinned.
    async fn pinned_format(&self, hash: Hash) -> Result<Option<BlobFormat>, NodeError> {
        let t = self.api().tags().get(pin_tag(&hash)).await.map_err(err)?;
        Ok(t.filter(|t| t.hash == hash).map(|t| t.format))
    }

    /// Loads the collection rooted at `hash` if its root is stored, within the size cap, and
    /// its metadata is present. The caller says it is a collection; nothing is sniffed.
    async fn load_collection(&self, hash: Hash) -> Result<Option<Collection>, NodeError> {
        match self.api().blobs().status(hash).await.map_err(err)? {
            BlobStatus::Complete { size } if size <= MAX_COLLECTION_ROOT => {}
            _ => return Ok(None),
        }
        // `Collection::load` also reads the names blob (the first link): cap it too.
        let root = self.api().blobs().get_bytes(hash).await.map_err(err)?;
        let Some(meta) = HashSeq::try_from(root).ok().and_then(|s| s.get(0)) else {
            return Ok(None);
        };
        match self.api().blobs().status(meta).await.map_err(err)? {
            BlobStatus::Complete { size } if size > MAX_COLLECTION_ROOT => {
                Err(err("collection metadata too large"))
            }
            BlobStatus::Complete { .. } => Ok(Collection::load(hash, self.api()).await.ok()),
            _ => Ok(None),
        }
    }

    /// Fetches a plain blob from online members, nearest first, moving on after a failure.
    /// A no-op if it is already here. A call has no deadline of its own; wrap it in a
    /// timeout. For a collection use [`Blobs::fetch_collection`].
    pub async fn fetch(&self, hash: Hash) -> Result<(), NodeError> {
        if self.root_complete(hash).await? {
            if self.pinned_format(hash).await?.is_none() {
                self.pin(hash, BlobFormat::Raw).await?;
            }
            return Ok(());
        }
        self.fetch_with(HashAndFormat::raw(hash)).await
    }

    /// Like [`Blobs::fetch`] for a collection: the root and all its children.
    pub async fn fetch_collection(&self, hash: Hash) -> Result<(), NodeError> {
        if self.has_collection(hash).await? {
            return self.pin(hash, BlobFormat::HashSeq).await;
        }
        self.fetch_with(HashAndFormat::hash_seq(hash)).await
    }

    async fn fetch_with(&self, req: HashAndFormat) -> Result<(), NodeError> {
        for peer in self.source.ranked() {
            let Ok(pid) = endpoint_id_of(&peer) else {
                continue;
            };
            match self.downloader.download(req, vec![pid]).await {
                Ok(()) => return self.pin(req.hash, req.format).await,
                Err(e) => warn!(peer = %peer.short(), error = %e, "blob fetch failed"),
            }
        }
        Err(NodeError::BlobUnavailable)
    }

    pub async fn read(&self, hash: Hash) -> Result<Bytes, NodeError> {
        if !self.root_complete(hash).await? {
            return Err(NodeError::BlobUnavailable);
        }
        self.api().blobs().get_bytes(hash).await.map_err(err)
    }

    /// A blob is written to the file `dest`; a collection (one pinned as such, by `add_path`
    /// or `fetch_collection`) to the directory `dest`. Nothing is written if a name is unsafe.
    pub async fn export(&self, hash: Hash, dest: &Path) -> Result<(), NodeError> {
        if self.pinned_format(hash).await? != Some(BlobFormat::HashSeq) {
            make_parent(dest).await?;
            self.api().blobs().export(hash, dest).await.map_err(err)?;
            return Ok(());
        }
        let c = self
            .load_collection(hash)
            .await?
            .ok_or(NodeError::BlobUnavailable)?;
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for (name, h) in c.iter() {
            let rel = safe_rel(name)?;
            if !seen.insert(rel.clone()) {
                return Err(err(format!("duplicate name {name:?} in collection")));
            }
            targets.push((dest.join(rel), *h));
        }
        tokio::fs::create_dir_all(dest).await?;
        for (path, h) in targets {
            make_parent(&path).await?;
            self.api().blobs().export(h, &path).await.map_err(err)?;
        }
        Ok(())
    }

    /// Test hook: stores a collection with arbitrary entry names, pinned as a collection.
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn add_named_for_test(
        &self,
        entries: Vec<(String, Bytes)>,
    ) -> Result<Hash, NodeError> {
        let mut keep = Vec::new();
        let mut coll = Vec::new();
        for (name, data) in entries {
            let tt = self.api().add_bytes(data).temp_tag().await.map_err(err)?;
            coll.push((name, tt.hash()));
            keep.push(tt);
        }
        let tt = Collection::from_iter(coll)
            .store(self.api())
            .await
            .map_err(err)?;
        self.pin(tt.hash(), BlobFormat::HashSeq).await?;
        Ok(tt.hash())
    }
}

async fn make_parent(p: &Path) -> Result<(), NodeError> {
    if let Some(parent) = p.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

/// Collection names come from peers: only plain relative `/`-separated names are written.
fn safe_rel(name: &str) -> Result<PathBuf, NodeError> {
    let bad = |c: &str| c.is_empty() || c == "." || c == ".." || c.contains(['\\', ':', '\0']);
    if name.split('/').any(bad) {
        return Err(err(format!("unsafe name {name:?} in collection")));
    }
    Ok(name.split('/').collect())
}

/// Every regular file below `root` as (`/`-separated relative name, path), sorted.
fn list_files(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_owned(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push((entry.path(), rel));
            } else if ft.is_file() {
                out.push((rel, entry.path()));
            }
        }
    }
    out.sort();
    Ok(out)
}
