//! Minimal content-addressed blobs (spec §8): an iroh-blobs `FsStore` at `<data>/blobs/`,
//! served behind the membership gate, fetched from online members in RTT order and
//! protected from GC by a named pin tag. iroh-blobs' own GC is never switched on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use iroh::{Endpoint, EndpointId};
use iroh_blobs::api::Store;
use iroh_blobs::api::downloader::Downloader;
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

    /// Whether the blob is stored completely (for a collection: with all its files).
    pub async fn has(&self, hash: Hash) -> Result<bool, NodeError> {
        if !self.root_complete(hash).await? {
            return Ok(false);
        }
        match self.collection(hash).await? {
            Some(c) => {
                for (_, h) in c.iter() {
                    if !self.root_complete(*h).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            None => Ok(true),
        }
    }

    async fn root_complete(&self, hash: Hash) -> Result<bool, NodeError> {
        self.api().blobs().has(hash).await.map_err(err)
    }

    /// The collection rooted at `hash`, if it is one and its metadata is stored.
    async fn collection(&self, hash: Hash) -> Result<Option<Collection>, NodeError> {
        if self.links(hash).await?.is_none() {
            return Ok(None);
        }
        Ok(Collection::load(hash, self.api()).await.ok())
    }

    /// The links of a stored blob whose size is a non-zero multiple of 32 (it might be a
    /// hash sequence).
    async fn links(&self, hash: Hash) -> Result<Option<HashSeq>, NodeError> {
        if !self.root_complete(hash).await? {
            return Ok(None);
        }
        let b = self.api().blobs().get_bytes(hash).await.map_err(err)?;
        Ok(HashSeq::try_from(b).ok().filter(|s| !s.is_empty()))
    }

    /// Fetches from online members, nearest first, moving on after a failure. A no-op if
    /// the blob is already here. A call has no deadline of its own; wrap it in a timeout.
    pub async fn fetch(&self, hash: Hash) -> Result<(), NodeError> {
        if self.has(hash).await? {
            let format = if self.collection(hash).await?.is_some() {
                BlobFormat::HashSeq
            } else {
                BlobFormat::Raw
            };
            return self.pin(hash, format).await;
        }
        for peer in self.source.ranked() {
            let Ok(pid) = endpoint_id_of(&peer) else {
                continue;
            };
            match self.fetch_from(hash, pid).await {
                Ok(format) => return self.pin(hash, format).await,
                Err(e) => warn!(peer = %peer.short(), error = %e, "blob fetch failed"),
            }
        }
        Err(NodeError::BlobUnavailable)
    }

    async fn download(&self, req: HashAndFormat, peer: EndpointId) -> Result<(), NodeError> {
        self.downloader.download(req, vec![peer]).await.map_err(err)
    }

    /// Fetches `hash` from one peer and says whether it is a plain blob or a collection.
    /// The wire does not say which; a blob that parses as a hash sequence is tried as one.
    async fn fetch_from(&self, hash: Hash, peer: EndpointId) -> Result<BlobFormat, NodeError> {
        self.download(HashAndFormat::raw(hash), peer).await?;
        let Some(links) = self.links(hash).await? else {
            return Ok(BlobFormat::Raw);
        };
        match self.download(HashAndFormat::hash_seq(hash), peer).await {
            Ok(()) => Ok(BlobFormat::HashSeq),
            Err(e) => {
                // A plain blob that merely has a multiple-of-32 size has no such children.
                // A real collection has stored metadata at its first link.
                let first = links.get(0).ok_or_else(|| err("empty hash sequence"))?;
                let is_collection = self.download(HashAndFormat::raw(first), peer).await.is_ok()
                    && Collection::load(hash, self.api()).await.is_ok();
                if is_collection {
                    Err(e)
                } else {
                    Ok(BlobFormat::Raw)
                }
            }
        }
    }

    pub async fn read(&self, hash: Hash) -> Result<Bytes, NodeError> {
        if !self.root_complete(hash).await? {
            return Err(NodeError::Io(std::io::ErrorKind::NotFound.into()));
        }
        self.api().blobs().get_bytes(hash).await.map_err(err)
    }

    /// A blob is written to the file `dest`; a collection to the directory `dest`.
    pub async fn export(&self, hash: Hash, dest: &Path) -> Result<(), NodeError> {
        match self.collection(hash).await? {
            None => {
                make_parent(dest).await?;
                self.api().blobs().export(hash, dest).await.map_err(err)?;
            }
            Some(c) => {
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
            }
        }
        Ok(())
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
