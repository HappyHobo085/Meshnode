//! Invites and the `join/1` protocol (spec §5): the inviter's handler and the joiner's flow.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use iroh::endpoint::{Connection, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use mesh_hlc::WallClock;
use mesh_proto::{
    InviteTicket, JoinRequest, JoinResponse, MemberInfo, MeshGenesis, NodeAddrBytes, NodeId,
    ProtoError, RefuseReason, Role,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::NodeConfig;
use crate::db::StoreHandle;
use crate::error::NodeError;
use crate::events::NodeEvent;
use crate::frame::{FrameError, JOIN_MAX_FRAME, read_frame, write_frame};
use crate::local::{
    InviteEntry, Invites, load_or_create_key, read_genesis, remove_genesis, write_genesis,
};
use crate::membership::{
    CONSUMED, MEMBERS, MESH_NS, MembershipSnapshot, addr_key, encode_addr, encode_info, info_key,
    member_bytes,
};
use crate::node::{Node, NodeStatus, own_info, start};
use crate::transport::{
    ALPN_JOIN, PROTOCOL, addr_from_bytes, addr_to_bytes, build_endpoint, node_id_of,
};

/// How long the joiner waits to reach the inviter.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long either side waits for the other's message once connected.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// The most peers a reply may list (`mesh-proto` refuses more on decode).
const MAX_REPLY_PEERS: usize = 64;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn join_err(e: tokio::task::JoinError) -> NodeError {
    NodeError::Io(std::io::Error::other(e))
}

fn iroh_err(e: impl fmt::Display) -> NodeError {
    NodeError::Iroh(e.to_string())
}

/// The inviter's side: the state shared by `Node::invite` and the `join/1` handler.
pub(crate) struct JoinInner {
    genesis: MeshGenesis,
    me: NodeId,
    store: StoreHandle,
    invites: Arc<Mutex<Invites>>,
    wall: Arc<dyn WallClock>,
    status: Arc<watch::Sender<NodeStatus>>,
    endpoint: Endpoint,
    /// Join requests are handled one at a time.
    serial: tokio::sync::Mutex<()>,
    drop_reply: AtomicBool,
}

impl JoinInner {
    pub fn new(
        genesis: MeshGenesis,
        me: NodeId,
        store: StoreHandle,
        invites: Arc<Mutex<Invites>>,
        wall: Arc<dyn WallClock>,
        status: Arc<watch::Sender<NodeStatus>>,
        endpoint: Endpoint,
    ) -> Self {
        JoinInner {
            genesis,
            me,
            store,
            invites,
            wall,
            status,
            endpoint,
            serial: tokio::sync::Mutex::new(()),
            drop_reply: AtomicBool::new(false),
        }
    }

    /// Test hook: the next accepted join is applied but its reply is never sent.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn drop_next_reply(&self) {
        self.drop_reply.store(true, Ordering::SeqCst);
    }

    async fn snapshot(&self) -> Result<MembershipSnapshot, NodeError> {
        let g = self.genesis.clone();
        self.store
            .call_raw(move |s| MembershipSnapshot::load(s, &g))
            .await
    }

    fn running(&self) -> bool {
        *self.status.borrow() == NodeStatus::Running
    }

    /// Spec §5.2: issues a ticket and records its hash in `invites.json`.
    pub async fn invite(&self, role: Role, ttl: Duration) -> Result<InviteTicket, NodeError> {
        if !self.running() || !self.snapshot().await?.is_effective(&self.me) {
            return Err(NodeError::NotEffective);
        }
        let token: [u8; 32] = rand::random();
        let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        let expires_ms = self.wall.now_ms().saturating_add(ttl_ms);
        let entry = InviteEntry {
            role,
            expires_ms,
            redeemed_by: None,
        };
        let invites = self.invites.clone();
        let hash = *blake3::hash(&token).as_bytes();
        tokio::task::spawn_blocking(move || lock(&invites).insert(hash, entry))
            .await
            .map_err(join_err)??;
        Ok(InviteTicket {
            mesh: self.genesis.mesh_id(),
            inviter: addr_to_bytes(&self.endpoint.addr()),
            token,
            role,
            expires_ms,
        })
    }

    async fn invite_entry(&self, hash: [u8; 32]) -> Result<Option<InviteEntry>, NodeError> {
        let invites = self.invites.clone();
        tokio::task::spawn_blocking(move || lock(&invites).get(&hash).cloned())
            .await
            .map_err(join_err)
    }

    /// Spec §5.3 steps 3 and 4: the checks in order, then the writes.
    async fn decide(&self, joiner: NodeId, req: &JoinRequest) -> Result<JoinResponse, NodeError> {
        let refuse = |r| Ok(JoinResponse::Refused(r));
        let _serial = self.serial.lock().await;

        if req.mesh != self.genesis.mesh_id() {
            return refuse(RefuseReason::WrongMesh);
        }
        let hash = *blake3::hash(&req.token).as_bytes();
        let Some(entry) = self.invite_entry(hash).await? else {
            return refuse(RefuseReason::Unknown);
        };
        if self.wall.now_ms() > entry.expires_ms {
            return refuse(RefuseReason::Expired);
        }
        let snap = self.snapshot().await?;
        if snap.cutoff(&joiner).is_some() {
            return refuse(RefuseReason::Removed);
        }
        let consumed = self
            .store
            .call_raw(move |s| s.set_contains(MESH_NS, CONSUMED, &hash))
            .await?;
        // A retry after a lost reply: the same node redeeming the same ticket again.
        let retry = entry.redeemed_by == Some(joiner);
        // A crash between the store writes and `set_redeemed` leaves the token consumed and
        // the joiner a member, but unmarked; the same joiner's retry completes it.
        let member = member_bytes(&joiner, entry.role).to_vec();
        let m = member.clone();
        let half_done = consumed
            && entry.redeemed_by.is_none()
            && self
                .store
                .call_raw(move |s| s.set_contains(MESH_NS, MEMBERS, &m))
                .await?;
        if consumed && !retry && !half_done {
            return refuse(RefuseReason::Used);
        }
        if !self.running() || !snap.is_effective(&self.me) {
            return refuse(RefuseReason::InviterUnavailable);
        }

        if !retry {
            // Store writes first, then the redemption mark, then (in `serve`) the reply.
            // All three writes are idempotent, so completing a half-done join re-applies them.
            self.store.set_add(MESH_NS, MEMBERS, member).await?;
            self.store.set_add(MESH_NS, CONSUMED, hash.to_vec()).await?;
            self.store
                .set(MESH_NS, &info_key(&joiner), encode_info(&req.info))
                .await?;
            let invites = self.invites.clone();
            tokio::task::spawn_blocking(move || lock(&invites).set_redeemed(hash, joiner))
                .await
                .map_err(join_err)??;
        }
        Ok(JoinResponse::Accepted {
            genesis: self.genesis.clone(),
            peers: self.peers_for(&snap, &joiner),
        })
    }

    /// Every effective member except the joiner, with its published address (this node's
    /// own current address for itself).
    fn peers_for(
        &self,
        snap: &MembershipSnapshot,
        joiner: &NodeId,
    ) -> Vec<(NodeId, NodeAddrBytes)> {
        let mut out = Vec::new();
        for id in snap.effective_members() {
            if id == *joiner {
                continue;
            }
            let addr = if id == self.me {
                Some(addr_to_bytes(&self.endpoint.addr()))
            } else {
                snap.addr(&id).cloned()
            };
            if let Some(a) = addr {
                out.push((id, a));
            }
        }
        out.truncate(MAX_REPLY_PEERS);
        out
    }

    async fn serve(&self, conn: &Connection, joiner: NodeId) -> Result<(), NodeError> {
        let (mut send, mut recv) = timeout(EXCHANGE_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| iroh_err("no request stream"))?
            .map_err(iroh_err)?;
        let req = match timeout(
            EXCHANGE_TIMEOUT,
            read_frame::<_, JoinRequest>(&mut recv, JOIN_MAX_FRAME),
        )
        .await
        {
            Ok(Ok(Some(r))) if r.validate().is_ok() => r,
            _ => {
                warn!(peer = %joiner.short(), "bad join request");
                conn.close(PROTOCOL, b"bad request");
                return Ok(());
            }
        };
        let resp = self.decide(joiner, &req).await?;
        if matches!(resp, JoinResponse::Accepted { .. }) {
            info!(peer = %joiner.short(), "join accepted");
        }
        if self.drop_reply.swap(false, Ordering::SeqCst) {
            conn.close(PROTOCOL, b"reply dropped");
            return Ok(());
        }
        write_frame(&mut send, &resp).await?;
        send.finish().map_err(iroh_err)?;
        // The joiner closes once it has the reply; closing first could discard it.
        let _ = timeout(EXCHANGE_TIMEOUT, conn.closed()).await;
        Ok(())
    }
}

/// The ungated `join/1` handler (a joiner is not a member yet).
#[derive(Clone)]
pub(crate) struct JoinHandler(pub Arc<JoinInner>);

impl fmt::Debug for JoinHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JoinHandler")
    }
}

impl ProtocolHandler for JoinHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let joiner = node_id_of(conn.remote_id());
        if let Err(e) = self.0.serve(&conn, joiner).await {
            warn!(peer = %joiner.short(), error = %e, "join request failed");
        }
        Ok(())
    }
}

/// Sends the request and reads the reply. Nothing has been written locally at this point.
async fn exchange(
    endpoint: &Endpoint,
    inviter: EndpointAddr,
    req: &JoinRequest,
) -> Result<JoinResponse, NodeError> {
    let conn = timeout(DIAL_TIMEOUT, endpoint.connect(inviter, ALPN_JOIN))
        .await
        .map_err(|_| NodeError::InviterUnreachable)?
        .map_err(|_| NodeError::InviterUnreachable)?;
    let talk = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(iroh_err)?;
        write_frame(&mut send, req).await?;
        send.finish().map_err(iroh_err)?;
        match read_frame::<_, JoinResponse>(&mut recv, JOIN_MAX_FRAME).await {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err(NodeError::InviterUnreachable),
            Err(FrameError::Io(e)) => Err(NodeError::Io(e)),
            Err(FrameError::Proto(e)) => Err(NodeError::Proto(e)),
            Err(FrameError::TooLarge(_)) => Err(NodeError::Proto(ProtoError::Limit("join reply"))),
        }
    };
    let r = timeout(EXCHANGE_TIMEOUT, talk)
        .await
        .map_err(|_| NodeError::InviterUnreachable)?;
    conn.close(VarInt::from_u32(0), b"done");
    r
}

/// Spec §5.3 steps 1, 2 and 5: nothing but `node.key` is written until the reply's genesis
/// is verified, so a failed join leaves the directory reusable with the same ticket.
pub(crate) async fn join(cfg: NodeConfig, ticket: InviteTicket) -> Result<Node, NodeError> {
    if read_genesis(&cfg.data_dir)?.is_some() {
        return Err(NodeError::AlreadyInMesh);
    }
    let info = own_info(&cfg);
    info.validate()?;
    let inviter = addr_from_bytes(&ticket.inviter)?;
    let inviter_id = node_id_of(inviter.id);
    let key = load_or_create_key(&cfg.data_dir)?;
    let endpoint = build_endpoint(&cfg, &key).await?;
    let req = JoinRequest {
        mesh: ticket.mesh,
        token: ticket.token,
        info,
    };
    let reply = exchange(&endpoint, inviter, &req).await;
    endpoint.close().await;
    let (genesis, mut peers) = match reply? {
        JoinResponse::Refused(r) => return Err(NodeError::JoinRefused(r)),
        JoinResponse::Accepted { genesis, peers } => (genesis, peers),
    };
    if genesis.mesh_id() != ticket.mesh {
        return Err(NodeError::JoinRefused(RefuseReason::WrongMesh));
    }
    // The ticket's own address is a fallback for the inviter if the reply lacks it.
    if !peers.iter().any(|(id, _)| *id == inviter_id) {
        peers.push((inviter_id, ticket.inviter.clone()));
    }
    let dir = cfg.data_dir.clone();
    write_genesis(&dir, &genesis)?;
    let ids: Vec<NodeId> = peers.iter().map(|(id, _)| *id).collect();
    let node = match start(cfg, key, genesis, peers).await {
        Ok(node) => node,
        Err(e) => {
            // Leave the directory reusable with the same ticket (the join is idempotent).
            if let Err(e) = remove_genesis(&dir) {
                warn!(error = %e, "could not remove mesh.json after a failed start");
            }
            return Err(e);
        }
    };
    // The empty store's snapshot knows only the founder: dial the listed peers too.
    node.add_join_peers(&ids);
    node.spawn_publish_own();
    Ok(node)
}

/// After the first completed round (spec §5.3 step 6) writes `info/<me>` and `addr/<me>`.
/// The member record arriving also counts, in case the round finished before this task
/// subscribed.
pub(crate) async fn publish_own(
    mut events: broadcast::Receiver<NodeEvent>,
    mut members: watch::Receiver<Arc<MembershipSnapshot>>,
    store: StoreHandle,
    endpoint: Endpoint,
    me: NodeId,
    own: MemberInfo,
    cancel: CancellationToken,
) {
    loop {
        if members.borrow().is_member(&me) {
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = events.recv() => match r {
                Ok(NodeEvent::FirstSyncComplete) => break,
                Err(RecvError::Closed) => return,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
            },
            r = members.changed() => if r.is_err() { return },
        }
    }
    let addr = endpoint.addr();
    let mut writes = vec![(info_key(&me), encode_info(&own))];
    if !addr.addrs.is_empty() {
        writes.push((addr_key(&me), encode_addr(&addr_to_bytes(&addr))));
    }
    for (key, value) in writes {
        let r = async {
            if store.get(MESH_NS, &key).await?.as_deref() != Some(value.as_slice()) {
                store.set(MESH_NS, &key, value).await?;
            }
            Ok::<(), NodeError>(())
        }
        .await;
        if let Err(e) = r {
            warn!(error = %e, key, "could not publish own record after joining");
        }
    }
}
