//! Per-request host state and the linker that defines the `meshnode:service/service` world.
//!
//! The linker holds exactly the world's imports, so a component importing anything else
//! (filesystem, sockets, unknown interfaces) fails `instantiate_pre` and is rejected at load.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use http::StatusCode;
use mesh_proto::NodeId;
use wasmtime::component::{HasData, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, ResourceLimiter, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::cli::{WasiCli, WasiCliView};
use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
use wasmtime_wasi::p2::bindings as wasi;
use wasmtime_wasi::p2::pipe::SinkOutputStream;
use wasmtime_wasi::random::{WasiRandom, WasiRandomView};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::{KvBackend, KvError, RuntimeError};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "meshnode:service/service",
        imports: { default: async | trappable },
        with: {
            "wasi:http": wasmtime_wasi_http::p2::bindings::http,
            "wasi": wasmtime_wasi::p2::bindings,
        },
        require_store_data_send: true,
    });
}

use bindings::meshnode::kv::store;

/// The store data of one request's instance.
pub(crate) struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    kv: Arc<dyn KvBackend>,
    node_id: NodeId,
    kv_status: KvStatus,
    limits: StoreLimits,
}

/// The HTTP status of a backend error that trapped the guest; 0 while there was none.
/// Shared so `serve` can read it after the store has moved into the guest task.
#[derive(Clone, Default)]
pub(crate) struct KvStatus(Arc<AtomicU16>);

impl KvStatus {
    pub(crate) fn get(&self) -> Option<StatusCode> {
        StatusCode::from_u16(self.0.load(Ordering::Relaxed)).ok()
    }
}

impl State {
    /// No preopens, no environment, no arguments, stdin closed, stdout and stderr to
    /// [`guest_output`]; resources capped by [`store_limits`].
    pub(crate) fn new(
        service: &str,
        kv: Arc<dyn KvBackend>,
        node_id: NodeId,
        kv_status: KvStatus,
        memory_mb: u32,
    ) -> Self {
        let wasi = WasiCtx::builder()
            .stdout(guest_output(service, Stream::Stdout))
            .stderr(guest_output(service, Stream::Stderr))
            .build();
        State {
            wasi,
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            kv,
            node_id,
            kv_status,
            limits: store_limits(memory_mb),
        }
    }

    /// For [`wasmtime::Store::limiter`].
    pub(crate) fn limiter(&mut self) -> &mut dyn ResourceLimiter {
        &mut self.limits
    }
}

/// Core instances one request may create. A component's instance count is fixed by its
/// binary (wstd guests use a handful), so this only bounds what a hostile binary declares.
const MAX_INSTANCES: usize = 32;
/// Tables, and elements per table: at most 16 x 100k funcref slots (about 13 MiB of host
/// memory), which wasmtime does not count against `memory_mb`.
const MAX_TABLES: usize = 16;
const MAX_TABLE_ELEMENTS: usize = 100_000;
/// One linear memory, so `memory_mb` caps the request's whole linear memory, not each of
/// several memories (wasmtime's `memory_size` limit applies per memory).
const MAX_MEMORIES: usize = 1;

/// Caps a request's linear memory at `memory_mb` MiB (failing growth makes the guest's
/// allocator fail, which traps) and bounds the instances and tables it can create.
fn store_limits(memory_mb: u32) -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size((memory_mb as usize).saturating_mul(1 << 20))
        .memories(MAX_MEMORIES)
        .instances(MAX_INSTANCES)
        .tables(MAX_TABLES)
        .table_elements(MAX_TABLE_ELEMENTS)
        .build()
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Where a guest's stdout and stderr go. Discarded for now; routed into `tracing` later
/// (sub-spec 3 §2.2). This is the only place that decides it.
fn guest_output(_service: &str, _stream: Stream) -> SinkOutputStream {
    SinkOutputStream
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

/// `wasi:io` is implemented directly on the resource table.
struct HasIo;

impl HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
}

/// A linker providing exactly the imports of world `meshnode:service/service`
/// (`wit/world.wit`), plus the `wasi:io` and `wasi:http/types` interfaces they depend on.
pub(crate) fn world_linker(engine: &Engine) -> Result<Linker<State>, RuntimeError> {
    let mut linker = Linker::new(engine);
    link_world(&mut linker).map_err(|e| RuntimeError::Compile(format!("link host: {e:#}")))?;
    Ok(linker)
}

fn link_world(l: &mut Linker<State>) -> wasmtime::Result<()> {
    wasi::io::error::add_to_linker::<State, HasIo>(l, |s| &mut s.table)?;
    wasi::io::poll::add_to_linker::<State, HasIo>(l, |s| &mut s.table)?;
    wasi::io::streams::add_to_linker::<State, HasIo>(l, |s| &mut s.table)?;
    wasi::clocks::monotonic_clock::add_to_linker::<State, WasiClocks>(l, State::clocks)?;
    wasi::clocks::wall_clock::add_to_linker::<State, WasiClocks>(l, State::clocks)?;
    wasi::random::random::add_to_linker::<State, WasiRandom>(l, State::random)?;
    wasi::random::insecure::add_to_linker::<State, WasiRandom>(l, State::random)?;
    wasi::random::insecure_seed::add_to_linker::<State, WasiRandom>(l, State::random)?;
    wasi::cli::stdin::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::stdout::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::stderr::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::terminal_input::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::terminal_output::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::terminal_stdin::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::terminal_stdout::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::terminal_stderr::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::environment::add_to_linker::<State, WasiCli>(l, State::cli)?;
    wasi::cli::exit::add_to_linker::<State, WasiCli>(l, State::cli)?;
    // `wasi:http/types` and `wasi:http/outgoing-handler`.
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(l)?;
    store::add_to_linker::<State, HasSelf<State>>(l, |s| s)?;
    Ok(())
}

/// The most list entries one `list-keys` / `set-members` call returns.
const MAX_LIST: u32 = 1000;

/// Records the HTTP status for a backend error and turns it into a guest trap.
fn kv_trap(status: &KvStatus, err: KvError) -> wasmtime::Error {
    let code = match &err {
        KvError::Full => StatusCode::INSUFFICIENT_STORAGE,
        KvError::Removed | KvError::Rebootstrapping => StatusCode::SERVICE_UNAVAILABLE,
        KvError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    status.0.store(code.as_u16(), Ordering::Relaxed);
    wasmtime::format_err!("meshnode:kv/store: {err}")
}

impl store::Host for State {
    async fn get(&mut self, key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        self.kv
            .get(&key)
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn set(&mut self, key: String, value: Vec<u8>) -> wasmtime::Result<()> {
        self.kv
            .set(&key, value)
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn set_ordered(
        &mut self,
        key: String,
        order: Vec<u8>,
        value: Option<Vec<u8>>,
    ) -> wasmtime::Result<()> {
        self.kv
            .set_ordered(&key, order, value)
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn delete(&mut self, key: String) -> wasmtime::Result<()> {
        self.kv
            .delete(&key)
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn list_keys(
        &mut self,
        prefix: String,
        start_after: Option<String>,
        limit: u32,
    ) -> wasmtime::Result<Vec<String>> {
        self.kv
            .list_keys(&prefix, start_after.as_deref(), limit.min(MAX_LIST))
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn set_add(&mut self, set: String, member: Vec<u8>) -> wasmtime::Result<()> {
        self.kv
            .set_add(&set, member)
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn set_contains(&mut self, set: String, member: Vec<u8>) -> wasmtime::Result<bool> {
        self.kv
            .set_contains(&set, &member)
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn set_members(
        &mut self,
        set: String,
        start_after: Option<Vec<u8>>,
        limit: u32,
    ) -> wasmtime::Result<Vec<Vec<u8>>> {
        self.kv
            .set_members(&set, start_after.as_deref(), limit.min(MAX_LIST))
            .await
            .map_err(|e| kv_trap(&self.kv_status, e))
    }
    async fn node_id(&mut self) -> wasmtime::Result<Vec<u8>> {
        Ok(self.node_id.0.to_vec())
    }
}
