//! THROWAWAY SPIKE — not production code. Answers Meshnode spec §14 step 0.
//! Library used by the CLI (`src/bin.rs`, run via `adb shell`) and by the spike APK (JNI, `android` module).
use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use http_body_util::BodyExt;
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::{Connection, presets}};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

const ALPN: &[u8] = b"meshnode/spike/0";

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn set_log_file(path: &str) -> Result<()> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let _ = LOG_FILE.set(Mutex::new(f));
    Ok(())
}

pub fn log(msg: impl AsRef<str>) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let line = format!("{}.{:03} {}", d.as_secs(), d.subsec_millis(), msg.as_ref());
    println!("{line}");
    if let Some(f) = LOG_FILE.get() {
        let mut f = f.lock().unwrap();
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

macro_rules! log { ($($t:tt)*) => { $crate::log(format!($($t)*)) } }

pub async fn run_cli(args: Vec<String>) -> Result<()> {
    match args.get(1).map(String::as_str) {
        Some("wasm") => wasm(&std::fs::read(&args[2]).context("read component")?, &args[3], args[4].parse()?).await,
        Some("listen") => listen(args.get(2).cloned()).await,
        Some("dial") => dial(&args[2], args[3].parse()?, args.get(4).map(|c| c.parse()).transpose()?, None).await,
        _ => bail!("usage: spike wasm <file> <cranelift|pulley> <n> | listen [key] | dial <id> <secs> [count]"),
    }
}

// ---------------------------------------------------------------- wasm

pub struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}
impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}
impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: Default::default() }
    }
}

/// Compile → serialize → in-memory deserialize → n requests. Returns the ProxyPre for reuse.
pub async fn wasm_prepare(bytes: &[u8], mode: &str) -> Result<ProxyPre<State>> {
    let mut cfg = Config::new();
    cfg.epoch_interruption(true);
    match mode {
        "cranelift" => {}
        "pulley" => {
            cfg.target("pulley64")?;
        }
        m => bail!("unknown mode {m}"),
    }
    let engine = Engine::new(&cfg)?;
    let t = Instant::now();
    let compiled = Component::new(&engine, bytes)?;
    log!("[{mode}] compile: {:?}", t.elapsed());
    let t = Instant::now();
    let ser = compiled.serialize()?;
    log!("[{mode}] serialize: {:?} ({} bytes)", t.elapsed(), ser.len());
    drop(compiled);
    // In-memory deserialize (NOT deserialize_file/mmap) — the path the spec proposes for Android.
    let t = Instant::now();
    let component = unsafe { Component::deserialize(&engine, &ser)? };
    log!("[{mode}] deserialize(in-memory): {:?}", t.elapsed());

    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    let pre = ProxyPre::new(linker.instantiate_pre(&component)?)?;
    let e2 = engine.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            e2.increment_epoch();
        }
    });
    Ok(pre)
}

pub async fn wasm(bytes: &[u8], mode: &str, n: usize) -> Result<()> {
    let pre = wasm_prepare(bytes, mode).await?;
    let mut times = Vec::with_capacity(n);
    for i in 0..n {
        let t = Instant::now();
        let body = handle(&pre, &format!("/req/{i}")).await?;
        times.push(t.elapsed());
        if i == 0 {
            log!("[{mode}] first response: {}", body.trim_end());
        }
    }
    times.sort();
    log!("[{mode}] {n} requests: min {:?} median {:?} max {:?}", times[0], times[n / 2], times[n - 1]);
    Ok(())
}

pub async fn handle(pre: &ProxyPre<State>, path: &str) -> Result<String> {
    let mut store = Store::new(
        pre.engine(),
        State { wasi: WasiCtx::builder().build(), http: WasiHttpCtx::new(), table: ResourceTable::new() },
    );
    // Generous: epoch ticks are timer-driven and stall while the phone sleeps.
    store.set_epoch_deadline(1000);
    let req = hyper::Request::builder()
        .uri(format!("http://spike.mesh.localhost{path}"))
        .body(http_body_util::Empty::<bytes::Bytes>::new())?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let req = store.data_mut().http().new_incoming_request(Scheme::Http, req)?;
    let out = store.data_mut().http().new_response_outparam(tx)?;
    let pre = pre.clone();
    let task = tokio::spawn(async move {
        let proxy = pre.instantiate_async(&mut store).await?;
        proxy.wasi_http_incoming_handler().call_handle(&mut store, req, out).await
    });
    let resp = match rx.await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(anyhow!("guest error code: {e:?}")),
        Err(_) => {
            task.await??;
            bail!("guest never set a response");
        }
    };
    let body = resp.into_body().collect().await.map_err(|e| anyhow!("{e:?}"))?.to_bytes();
    task.await??;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

// ---------------------------------------------------------------- iroh

fn load_key(path: Option<String>) -> Result<SecretKey> {
    let Some(p) = path else { return Ok(SecretKey::generate()) };
    if let Ok(b) = std::fs::read(&p) {
        let arr: [u8; 32] = b.try_into().map_err(|_| anyhow!("bad key file"))?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let k = SecretKey::generate();
    std::fs::write(&p, k.to_bytes())?;
    Ok(k)
}

pub async fn listen(key: Option<String>) -> Result<()> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(load_key(key)?)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    ep.online().await;
    log!("LISTEN endpoint-id {}", ep.id());
    for a in ep.addr().ip_addrs() {
        log!("  ip {a}");
    }
    for r in ep.addr().relay_urls() {
        log!("  relay {r}");
    }
    while let Some(incoming) = ep.accept().await {
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => return log!("accept failed: {e:#}"),
            };
            let peer = conn.remote_id();
            log!("conn from {peer}");
            loop {
                match conn.accept_bi().await {
                    Ok((mut s, mut r)) => {
                        let msg = match r.read_to_end(1024).await {
                            Ok(m) => m,
                            Err(e) => {
                                log!("read err {e:#}");
                                continue;
                            }
                        };
                        let _ = s.write_all(&msg).await;
                        let _ = s.finish();
                        log!("echo to {} '{}' [{}]", peer.fmt_short(), String::from_utf8_lossy(&msg), path_desc(&conn));
                    }
                    Err(e) => {
                        log!("conn from {} closed: {e:#}", peer.fmt_short());
                        break;
                    }
                }
            }
        });
    }
    Ok(())
}

fn path_desc(conn: &Connection) -> String {
    conn.paths()
        .iter()
        .map(|p| format!("{}{}:{:?}", if p.is_selected() { "*" } else { "" }, if p.is_relay() { "relay" } else { "ip" }, p.rtt()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ping on one long-lived connection; reconnect on failure. If `wasm` is given, also serve one
/// wasm request per tick to prove the JIT keeps working in the background.
pub async fn dial(id: &str, interval: u64, count: Option<u64>, wasm: Option<Arc<ProxyPre<State>>>) -> Result<()> {
    let id: EndpointId = id.parse()?;
    let ep = Endpoint::builder(presets::N0).bind().await?;
    ep.online().await;
    log!("DIAL me={} -> {}", ep.id().fmt_short(), id.fmt_short());
    let started = Instant::now();
    let mut conn: Option<Connection> = None;
    let mut i = 0u64;
    let mut last_tick = Instant::now();
    loop {
        if count.is_some_and(|c| i >= c) {
            break;
        }
        // Gap detection: if the tick fired much later than scheduled, the process was frozen (doze/suspend).
        let gap = last_tick.elapsed();
        last_tick = Instant::now();
        if i > 0 && gap > Duration::from_secs(interval + 10) {
            log!("TICK LATE by {:?} (process likely suspended)", gap - Duration::from_secs(interval));
        }
        if conn.as_ref().is_none_or(|c| c.close_reason().is_some()) {
            let t = Instant::now();
            match ep.connect(id, ALPN).await {
                Ok(c) => {
                    log!("connected in {:?} (t+{}s)", t.elapsed(), started.elapsed().as_secs());
                    conn = Some(c);
                }
                Err(e) => {
                    log!("connect failed: {e:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }
        let c = conn.as_ref().unwrap();
        let t = Instant::now();
        let res: Result<()> = async {
            let (mut s, mut r) = c.open_bi().await?;
            s.write_all(format!("ping {i}").as_bytes()).await?;
            s.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(20), r.read_to_end(1024)).await??;
            Ok(())
        }
        .await;
        let wasm_res = match &wasm {
            Some(pre) => {
                let t = Instant::now();
                match handle(pre, &format!("/tick/{i}")).await {
                    Ok(_) => format!(" wasm ok {:?}", t.elapsed()),
                    Err(e) => format!(" WASM ERR {e:#}"),
                }
            }
            None => String::new(),
        };
        match res {
            Ok(()) => log!("ping {i} rtt {:?} [{}]{wasm_res} t+{}s", t.elapsed(), path_desc(c), started.elapsed().as_secs()),
            Err(e) => {
                log!("ping {i} FAILED after {:?}: {e:#} — reconnecting{wasm_res}", t.elapsed());
                c.close(0u32.into(), b"retry");
                conn = None;
                continue;
            }
        }
        i += 1;
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
    ep.close().await;
    Ok(())
}

// ---------------------------------------------------------------- Android JNI

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use jni::JNIEnv;
    use jni::objects::{JClass, JObject, JString};

    static STARTED: OnceLock<()> = OnceLock::new();
    const GUEST: &[u8] = include_bytes!("../../guest/target/wasm32-wasip2/release/guest.wasm");

    /// com.meshnode.spike.Native.start(Context ctx, String filesDir, String peerId, int intervalSecs)
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_meshnode_spike_Native_start<'l>(
        mut env: JNIEnv<'l>,
        _class: JClass<'l>,
        ctx: JObject<'l>,
        files_dir: JString<'l>,
        peer: JString<'l>,
        interval: i32,
    ) {
        let files_dir: String = env.get_string(&files_dir).map(Into::into).unwrap_or_default();
        let peer: String = env.get_string(&peer).map(Into::into).unwrap_or_default();
        let _ = set_log_file(&format!("{files_dir}/spike.log"));
        if STARTED.set(()).is_err() {
            log!("start() called again — already running (service restart)");
            return;
        }
        // REQUIRED FINDING: iroh's DNS resolver + netdev need the Android context (ndk-context),
        // otherwise a worker thread panics with "android context was not initialized".
        let vm = env.get_java_vm().expect("vm");
        let gref = env.new_global_ref(ctx).expect("global ref");
        unsafe { ndk_context::initialize_android_context(vm.get_java_vm_pointer() as *mut _, gref.as_obj().as_raw() as *mut _) };
        std::mem::forget(gref);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("rt");
            rt.block_on(async move {
                log!("=== spike started in app sandbox; interval {interval}s ===");
                for mode in ["cranelift", "pulley"] {
                    if let Err(e) = wasm(GUEST, mode, 5).await {
                        log!("[{mode}] WASM FAILED: {e:#}");
                    }
                }
                let pre = match wasm_prepare(GUEST, "cranelift").await {
                    Ok(p) => Some(Arc::new(p)),
                    Err(e) => {
                        log!("cranelift prepare failed ({e:#}); trying pulley for ticks");
                        wasm_prepare(GUEST, "pulley").await.ok().map(Arc::new)
                    }
                };
                if let Err(e) = dial(&peer, interval as u64, None, pre).await {
                    log!("dial loop ended: {e:#}");
                }
            });
        });
    }

    /// Lets Java write markers (screen on/off, doze changes) into the same log.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_meshnode_spike_Native_mark<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>, msg: JString<'l>) {
        let msg: String = env.get_string(&msg).map(Into::into).unwrap_or_default();
        log!("[java] {msg}");
    }
}
