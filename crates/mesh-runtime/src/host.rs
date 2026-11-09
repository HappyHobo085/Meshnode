//! Per-request host state and the linker that defines the `meshnode:service/service` world.
//!
//! The linker holds exactly the world's imports, so a component importing anything else
//! (filesystem, sockets, unknown interfaces) fails `instantiate_pre` and is rejected at load.

use wasmtime::Engine;
use wasmtime::component::{HasData, HasSelf, Linker, ResourceTable};
use wasmtime_wasi::cli::{WasiCli, WasiCliView};
use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
use wasmtime_wasi::p2::bindings as wasi;
use wasmtime_wasi::p2::pipe::SinkOutputStream;
use wasmtime_wasi::random::{WasiRandom, WasiRandomView};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::RuntimeError;

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
}

impl State {
    /// No preopens, no environment, no arguments, stdin closed, stdout and stderr to
    /// [`guest_output`].
    pub(crate) fn new(service: &str) -> Self {
        let wasi = WasiCtx::builder()
            .stdout(guest_output(service, Stream::Stdout))
            .stderr(guest_output(service, Stream::Stderr))
            .build();
        State {
            wasi,
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        }
    }
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

/// `meshnode:kv/store` placeholder: every call traps until the KV binding is wired.
fn kv_unavailable<T>() -> wasmtime::Result<T> {
    Err(wasmtime::format_err!("meshnode:kv/store is not available"))
}

impl store::Host for State {
    async fn get(&mut self, _key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        kv_unavailable()
    }
    async fn set(&mut self, _key: String, _value: Vec<u8>) -> wasmtime::Result<()> {
        kv_unavailable()
    }
    async fn set_ordered(
        &mut self,
        _key: String,
        _order: Vec<u8>,
        _value: Option<Vec<u8>>,
    ) -> wasmtime::Result<()> {
        kv_unavailable()
    }
    async fn delete(&mut self, _key: String) -> wasmtime::Result<()> {
        kv_unavailable()
    }
    async fn list_keys(
        &mut self,
        _prefix: String,
        _start_after: Option<String>,
        _limit: u32,
    ) -> wasmtime::Result<Vec<String>> {
        kv_unavailable()
    }
    async fn set_add(&mut self, _set: String, _member: Vec<u8>) -> wasmtime::Result<()> {
        kv_unavailable()
    }
    async fn set_contains(&mut self, _set: String, _member: Vec<u8>) -> wasmtime::Result<bool> {
        kv_unavailable()
    }
    async fn set_members(
        &mut self,
        _set: String,
        _start_after: Option<Vec<u8>>,
        _limit: u32,
    ) -> wasmtime::Result<Vec<Vec<u8>>> {
        kv_unavailable()
    }
    async fn node_id(&mut self) -> wasmtime::Result<Vec<u8>> {
        kv_unavailable()
    }
}
